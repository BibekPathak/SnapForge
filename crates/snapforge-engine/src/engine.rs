//! High-level checkpoint/restore operations.
//!
//! `Engine` validates prerequisites, stages artifacts in a temporary directory,
//! invokes CRIU with explicit arguments, records timing/diagnostics, and
//! performs cleanup on failure. It never claims success when CRIU failed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snapforge_core::host::{self, CriuReadiness, HostReport};

use crate::criu::{Criu, CriuCommand, CriuVersion};
use crate::error::{EngineError, Result};

/// Options controlling a checkpoint capture.
#[derive(Debug, Clone)]
pub struct CheckpointOptions {
    /// PID of the process to checkpoint.
    pub pid: i32,
    /// Directory that will receive the CRIU image files.
    pub output_dir: PathBuf,
    /// Allow `--shell-job` (checkpoint a process that has its own session).
    pub shell_job: bool,
    /// Leave the target process running after the dump (`--leave-running`).
    pub leave_running: bool,
    /// Optional timeout; if exceeded the CRIU process is killed.
    pub timeout: Option<Duration>,
    /// Extra raw CRIU arguments (advanced use; validated by the caller).
    pub extra_args: Vec<String>,
}

impl CheckpointOptions {
    /// Create options with conservative defaults for the given PID/dir.
    pub fn new(pid: i32, output_dir: impl Into<PathBuf>) -> Self {
        CheckpointOptions {
            pid,
            output_dir: output_dir.into(),
            shell_job: true,
            leave_running: false,
            timeout: Some(Duration::from_secs(120)),
            extra_args: Vec::new(),
        }
    }
}

/// Options controlling a restore.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// Directory containing the CRIU image files.
    pub input_dir: PathBuf,
    /// Restore into a new set of namespaces (`--restore-detached` is implied).
    pub shell_job: bool,
    /// Detach after restore rather than attaching to the caller's terminal.
    pub detached: bool,
    /// Optional timeout.
    pub timeout: Option<Duration>,
    /// Extra raw CRIU arguments.
    pub extra_args: Vec<String>,
}

impl RestoreOptions {
    pub fn new(input_dir: impl Into<PathBuf>) -> Self {
        RestoreOptions {
            input_dir: input_dir.into(),
            shell_job: true,
            detached: true,
            timeout: Some(Duration::from_secs(120)),
            extra_args: Vec::new(),
        }
    }
}

/// Result of a successful checkpoint capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointOutcome {
    pub pid: i32,
    pub output_dir: PathBuf,
    pub duration_ms: u64,
    /// Number of CRIU image files produced.
    pub artifact_count: usize,
    /// Sum of artifact file sizes in bytes.
    pub total_bytes: u64,
    pub criu_version: Option<CriuVersion>,
    pub command: CriuCommand,
}

/// Result of a successful restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub input_dir: PathBuf,
    pub duration_ms: u64,
    pub command: CriuCommand,
}

/// Snapshot of a process's lifecycle state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub pid: i32,
    pub exists: bool,
    pub state: Option<String>,
    pub name: Option<String>,
    pub threads: Option<usize>,
}

/// Orchestrates CRIU operations with prerequisite validation and cleanup.
pub struct Engine {
    criu: Criu,
    host_report: HostReport,
}

impl Engine {
    /// Build an engine, discovering CRIU and probing the host.
    pub fn new() -> Result<Self> {
        let criu = Criu::discover()?;
        Ok(Engine {
            criu,
            host_report: host::probe(),
        })
    }

    /// Build an engine from an explicit CRIU path (useful in tests).
    pub fn with_criu_path(path: impl Into<PathBuf>) -> Result<Self> {
        Ok(Engine {
            criu: Criu::from_path(path)?,
            host_report: host::probe(),
        })
    }

    /// Access the cached host report.
    pub fn host_report(&self) -> &HostReport {
        &self.host_report
    }

    /// Verify that the host can actually perform CRIU checkpoint/restore.
    ///
    /// Returns `Err(HostNotCapable)` with the precise readiness classification
    /// when it cannot, so callers never proceed to fabricate a result.
    pub fn ensure_capable(&self) -> Result<()> {
        if self.host_report.criu_readiness != CriuReadiness::Ready {
            return Err(EngineError::HostNotCapable {
                readiness: self.host_report.criu_readiness,
            });
        }
        Ok(())
    }

    /// Capture a checkpoint of `opts.pid`.
    pub fn checkpoint(&mut self, opts: &CheckpointOptions) -> Result<CheckpointOutcome> {
        self.ensure_capable()?;
        if !process_exists(opts.pid) {
            return Err(EngineError::TargetNotFound { pid: opts.pid });
        }
        validate_output_dir(&opts.output_dir)?;

        let version = self.criu.version().ok();

        let mut args: Vec<String> = vec![
            "dump".into(),
            "-t".into(),
            opts.pid.to_string(),
            "-D".into(),
            opts.output_dir.display().to_string(),
        ];
        if opts.shell_job {
            args.push("--shell-job".into());
        }
        if opts.leave_running {
            args.push("--leave-running".into());
        }
        args.extend(opts.extra_args.iter().cloned());

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let started = Instant::now();
        let command = self.criu.run(&arg_refs, opts.timeout)?;

        if command.timed_out {
            self.cleanup_on_failure(&opts.output_dir);
            return Err(EngineError::Timeout {
                op: "dump".into(),
                seconds: opts.timeout.map(|d| d.as_secs()).unwrap_or(0),
            });
        }
        if !command.succeeded() {
            self.cleanup_on_failure(&opts.output_dir);
            return Err(EngineError::CriuFailed {
                op: "dump".into(),
                code: command.exit_code,
                stderr: command.stderr.clone(),
            });
        }

        let (artifact_count, total_bytes) = summarize_artifacts(&opts.output_dir)?;
        if artifact_count == 0 {
            // CRIU claimed success but produced nothing: treat as failure and
            // never publish an empty checkpoint as valid.
            self.cleanup_on_failure(&opts.output_dir);
            return Err(EngineError::EmptyCheckpoint(opts.output_dir.clone()));
        }

        Ok(CheckpointOutcome {
            pid: opts.pid,
            output_dir: opts.output_dir.clone(),
            duration_ms: started.elapsed().as_millis() as u64,
            artifact_count,
            total_bytes,
            criu_version: version,
            command,
        })
    }

    /// Restore a checkpoint from `opts.input_dir`.
    pub fn restore(&mut self, opts: &RestoreOptions) -> Result<RestoreOutcome> {
        self.ensure_capable()?;
        if !opts.input_dir.is_dir() {
            return Err(EngineError::Other(format!(
                "checkpoint directory {} does not exist",
                opts.input_dir.display()
            )));
        }
        let (count, _) = summarize_artifacts(&opts.input_dir)?;
        if count == 0 {
            return Err(EngineError::EmptyCheckpoint(opts.input_dir.clone()));
        }

        let mut args: Vec<String> = vec![
            "restore".into(),
            "-D".into(),
            opts.input_dir.display().to_string(),
        ];
        if opts.shell_job {
            args.push("--shell-job".into());
        }
        if opts.detached {
            args.push("--restore-detached".into());
        }
        args.extend(opts.extra_args.iter().cloned());

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let started = Instant::now();
        let command = self.criu.run(&arg_refs, opts.timeout)?;

        if command.timed_out {
            return Err(EngineError::Timeout {
                op: "restore".into(),
                seconds: opts.timeout.map(|d| d.as_secs()).unwrap_or(0),
            });
        }
        if !command.succeeded() {
            return Err(EngineError::CriuFailed {
                op: "restore".into(),
                code: command.exit_code,
                stderr: command.stderr.clone(),
            });
        }

        Ok(RestoreOutcome {
            input_dir: opts.input_dir.clone(),
            duration_ms: started.elapsed().as_millis() as u64,
            command,
        })
    }

    /// Report lifecycle status of a process from `/proc`.
    pub fn process_status(&self, pid: i32) -> ProcessStatus {
        status_of(pid)
    }

    /// Remove SnapForge-owned partial output after a failed capture.
    ///
    /// Only files under `dir` are touched. If the directory becomes empty it is
    /// removed; pre-existing unrelated directories are left alone.
    fn cleanup_on_failure(&self, dir: &Path) {
        tracing::warn!(dir = %dir.display(), "cleaning up partial checkpoint output");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        // Attempt to remove the directory only if now empty.
        let _ = std::fs::remove_dir(dir);
    }
}

/// True when `/proc/<pid>` exists and is accessible.
pub fn process_exists(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).is_dir()
}

/// Read lifecycle status of a process from `/proc`. Does not require CRIU.
pub fn status_of(pid: i32) -> ProcessStatus {
    let base = PathBuf::from(format!("/proc/{pid}"));
    if !base.is_dir() {
        return ProcessStatus {
            pid,
            exists: false,
            state: None,
            name: None,
            threads: None,
        };
    }
    let state = read_proc_field(&base.join("stat")).and_then(|s| parse_state_from_stat(&s));
    let name = read_proc_field(&base.join("comm"));
    let threads = std::fs::read_dir(base.join("task"))
        .ok()
        .map(|rd| rd.filter_map(|e| e.ok()).count());
    ProcessStatus {
        pid,
        exists: true,
        state,
        name,
        threads,
    }
}

fn validate_output_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        let empty = std::fs::read_dir(dir)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(EngineError::Config(format!(
                "output directory {} already exists and is not empty",
                dir.display()
            )));
        }
    } else {
        std::fs::create_dir_all(dir).map_err(|e| EngineError::io_at(dir.to_path_buf(), e))?;
    }
    Ok(())
}

/// Count artifacts and sum their sizes in a directory.
///
/// Public so that the "empty checkpoint is a failure" rule can be tested
/// without requiring a CRIU-capable host.
pub fn summarize_artifacts(dir: &Path) -> Result<(usize, u64)> {
    let mut count = 0usize;
    let mut bytes = 0u64;
    let rd = std::fs::read_dir(dir).map_err(|e| EngineError::io_at(dir.to_path_buf(), e))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_file() {
            count += 1;
            if let Ok(meta) = entry.metadata() {
                bytes += meta.len();
            }
        }
    }
    Ok((count, bytes))
}

fn read_proc_field(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Extract the state character from `/proc/<pid>/stat`, handling process names
/// that contain spaces or parentheses.
fn parse_state_from_stat(stat: &str) -> Option<String> {
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 1..)?.trim();
    rest.chars().next().map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_exists_for_self() {
        let pid = std::process::id() as i32;
        assert!(process_exists(pid));
    }

    #[test]
    fn process_status_of_self() {
        let engine = match Engine::new() {
            Ok(e) => e,
            // No criu here; construct via a fake path is awkward, so test the
            // free functions instead when the engine cannot be built.
            Err(_) => {
                let pid = std::process::id() as i32;
                assert!(process_exists(pid));
                return;
            }
        };
        let pid = std::process::id() as i32;
        let status = engine.process_status(pid);
        assert!(status.exists);
        assert!(status.state.is_some());
        assert!(status.threads.unwrap_or(0) >= 1);
    }

    #[test]
    fn parse_stat_state() {
        let stat = "1234 (my proc) S 1 2 3";
        assert_eq!(parse_state_from_stat(stat).as_deref(), Some("S"));
    }

    #[test]
    fn parse_stat_state_with_spaces_in_name() {
        let stat = "1234 (weird ) name) R 1 2 3";
        assert_eq!(parse_state_from_stat(stat).as_deref(), Some("R"));
    }

    #[test]
    fn missing_process_not_found() {
        // PID 0 is never a normal process directory.
        assert!(!process_exists(0));
    }
}
