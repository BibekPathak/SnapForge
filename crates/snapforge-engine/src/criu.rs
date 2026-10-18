//! CRIU binary discovery and command invocation.
//!
//! All invocations use explicit argument arrays (never shell interpolation),
//! capture stdout/stderr/exit/duration, and enforce an optional timeout.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, Result};

/// Parsed CRIU version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriuVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub raw: String,
}

impl CriuVersion {
    /// Parse the output of `criu --version`, e.g. `Version: 4.2.1`.
    pub fn parse(output: &str) -> Option<Self> {
        let raw = output.trim().to_string();
        let num = raw
            .lines()
            .find_map(|l| l.strip_prefix("Version:"))
            .map(|s| s.trim())
            .or(Some(raw.as_str()))?;
        let mut parts = num.split('.');
        let major = parts.next()?.trim().parse().ok()?;
        let minor = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let patch = parts
            .next()
            .unwrap_or("0")
            .trim()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        Some(CriuVersion {
            major,
            minor,
            patch,
            raw,
        })
    }

    /// Minimum version SnapForge supports.
    pub fn is_supported(&self) -> bool {
        (self.major, self.minor) >= (3, 15)
    }
}

impl std::fmt::Display for CriuVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A single CRIU invocation with captured results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriuCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

impl CriuCommand {
    /// True when the process exited zero and was not killed by the timeout.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// Handle to a discovered CRIU binary.
#[derive(Debug, Clone)]
pub struct Criu {
    program: PathBuf,
    version: Option<CriuVersion>,
}

impl Criu {
    /// Locate `criu` on `PATH`.
    pub fn discover() -> Result<Self> {
        let program = which("criu").ok_or(EngineError::CriuNotFound)?;
        Ok(Criu {
            program,
            version: None,
        })
    }

    /// Construct from an explicit path (used by tests and advanced users).
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self> {
        let program = path.into();
        if !program.is_file() {
            return Err(EngineError::CriuNotFound);
        }
        Ok(Criu {
            program,
            version: None,
        })
    }

    /// Path to the CRIU binary.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Query and cache `criu --version`.
    pub fn version(&mut self) -> Result<CriuVersion> {
        if let Some(v) = &self.version {
            return Ok(v.clone());
        }
        let cmd = self.run(&["--version"], None)?;
        if !cmd.succeeded() {
            return Err(EngineError::CriuFailed {
                op: "--version".into(),
                code: cmd.exit_code,
                stderr: cmd.stderr,
            });
        }
        let v = CriuVersion::parse(&cmd.stdout)
            .or_else(|| CriuVersion::parse(&cmd.stderr))
            .ok_or_else(|| EngineError::Other("could not parse criu version".into()))?;
        self.version = Some(v.clone());
        Ok(v)
    }

    /// Run `criu check` to validate kernel feature support. Returns the command
    /// result so callers can inspect feature-level diagnostics.
    pub fn check(&self) -> Result<CriuCommand> {
        self.run(&["check"], None)
    }

    /// Invoke CRIU with explicit args, an optional timeout, and full capture.
    ///
    /// The child is placed in its own process group so that a timeout kills the
    /// entire group (including grandchildren). Output is drained on dedicated
    /// threads so that an inherited pipe descriptor cannot block us past the
    /// deadline.
    pub fn run(&self, args: &[&str], timeout: Option<Duration>) -> Result<CriuCommand> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Put the child in a new process group so we can signal the whole tree.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                // setpgid(0, 0): new process group led by the child.
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let start = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|e| EngineError::io_at(self.program.clone(), e))?;

        let child_pid = child.id() as i32;

        // Drain pipes on background threads so a lingering grandchild holding a
        // pipe cannot block completion.
        let stdout_handle = child.stdout.take().map(|mut s| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut s, &mut buf);
                buf
            })
        });
        let stderr_handle = child.stderr.take().map(|mut s| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut s, &mut buf);
                buf
            })
        });

        let mut timed_out = false;
        let exit_code: Option<i32>;

        match timeout {
            None => {
                let status = child
                    .wait()
                    .map_err(|e| EngineError::io_at(self.program.clone(), e))?;
                exit_code = status.code();
            }
            Some(limit) => {
                let deadline = start + limit;
                loop {
                    match child
                        .try_wait()
                        .map_err(|e| EngineError::io_at(self.program.clone(), e))?
                    {
                        Some(status) => {
                            exit_code = status.code();
                            break;
                        }
                        None => {
                            if Instant::now() >= deadline {
                                // Kill the entire process group.
                                unsafe {
                                    libc::killpg(child_pid, libc::SIGKILL);
                                }
                                let status = child
                                    .wait()
                                    .map_err(|e| EngineError::io_at(self.program.clone(), e))?;
                                exit_code = status.code();
                                timed_out = true;
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(25));
                        }
                    }
                }
            }
        }

        let stdout = stdout_handle
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let stderr = stderr_handle
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();

        Ok(CriuCommand {
            program: self.program.clone(),
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_code,
            stdout,
            stderr,
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out,
        })
    }
}

/// Minimal shell-free `which`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_line() {
        let v = CriuVersion::parse("Version: 4.2.1\n").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (4, 2, 1));
    }

    #[test]
    fn parse_bare_version() {
        let v = CriuVersion::parse("3.17.1").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (3, 17, 1));
    }

    #[test]
    fn parse_version_with_suffix() {
        let v = CriuVersion::parse("Version: 3.19.0-dev\n").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (3, 19, 0));
    }

    #[test]
    fn supported_version_threshold() {
        assert!(CriuVersion::parse("3.15.0").unwrap().is_supported());
        assert!(!CriuVersion::parse("3.14.9").unwrap().is_supported());
    }

    #[test]
    fn discover_reports_not_found_cleanly() {
        // On hosts without criu this must be a clean error, not a panic.
        match Criu::discover() {
            Ok(c) => assert!(c.program().is_file()),
            Err(EngineError::CriuNotFound) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn timeout_kills_long_running_command() {
        use std::os::unix::fs::PermissionsExt;
        // Create a wrapper that always sleeps, as a stand-in for a hung CRIU.
        let dir = std::env::temp_dir().join("snapforge-test-sleep");
        std::fs::create_dir_all(&dir).unwrap();
        let wrapper = dir.join("sleep-wrapper");
        std::fs::write(&wrapper, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper, perms).unwrap();

        let criu = Criu::from_path(&wrapper).unwrap();
        let result = criu.run(&[], Some(Duration::from_millis(200))).unwrap();
        assert!(result.timed_out);
        assert!(!result.succeeded());
    }
}
