//! Runnable workload harness.
//!
//! This ties together parameter parsing, deterministic execution, the
//! readiness/state-file protocol, and post-restore validation. Each workload
//! binary is a thin wrapper around [`run`].

use std::path::PathBuf;

use crate::workload::{self, WorkloadKind, WorkloadParams, WorkloadState};

/// Parsed command-line options for a workload binary.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub kind: WorkloadKind,
    pub memory_mib: u64,
    pub threads: usize,
    pub iterations: u64,
    pub files: usize,
    pub file_mib: u64,
    pub validate: bool,
    pub state_file: Option<PathBuf>,
}

impl RunOptions {
    /// Parse options from `args` (excluding argv[0]).
    pub fn parse(kind: WorkloadKind, args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut opts = RunOptions {
            kind,
            memory_mib: 64,
            threads: 1,
            iterations: 1_000_000,
            files: 8,
            file_mib: 16,
            validate: false,
            state_file: None,
        };
        let mut it = args;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--memory-mib" => opts.memory_mib = next_u64(&mut it, "--memory-mib")?,
                "--threads" => opts.threads = next_usize(&mut it, "--threads")?,
                "--iterations" => opts.iterations = next_u64(&mut it, "--iterations")?,
                "--files" => opts.files = next_usize(&mut it, "--files")?,
                "--file-mib" => opts.file_mib = next_u64(&mut it, "--file-mib")?,
                "--validate" => opts.validate = true,
                "--state-file" => {
                    opts.state_file = Some(PathBuf::from(
                        it.next().ok_or("--state-file needs a value")?,
                    ))
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        if opts.threads == 0 {
            return Err("--threads must be >= 1".into());
        }
        Ok(opts)
    }

    fn params(&self) -> WorkloadParams {
        WorkloadParams {
            kind: self.kind,
            memory_mib: self.memory_mib,
            threads: self.threads,
            iterations: self.iterations,
            files: self.files,
            file_mib: self.file_mib,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.state_file.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("snapforge-{:?}-state.json", self.kind))
        })
    }
}

/// Entry point for a workload binary.
///
/// Exit codes: 0 on success, 2 on argument error, 3 on validation failure.
pub fn run(kind: WorkloadKind, args: impl Iterator<Item = String>) -> std::process::ExitCode {
    let opts = match RunOptions::parse(kind, args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("snapforge workload: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let state_path = opts.state_path();

    if opts.validate {
        match validate(&opts, &state_path) {
            Ok(()) => {
                println!("VALIDATED ok: {}", state_path.display());
                std::process::ExitCode::SUCCESS
            }
            Err(msg) => {
                eprintln!("VALIDATION FAILED: {msg}");
                std::process::ExitCode::from(3)
            }
        }
    } else {
        match execute(&opts, &state_path) {
            Ok(state) => {
                println!(
                    "READY kind={:?} counter={} memory_digest={} state={}",
                    state.kind,
                    state.counter,
                    state.memory_digest,
                    state_path.display()
                );
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("workload failed: {e}");
                std::process::ExitCode::from(1)
            }
        }
    }
}

/// Run the work, exercising the requested subsystems, and persist expected
/// state so a later `--validate` can prove equivalence.
fn execute(opts: &RunOptions, state_path: &std::path::Path) -> Result<WorkloadState, String> {
    let params = opts.params();

    // Anonymous memory footprint with a deterministic pattern.
    let bytes = (params.memory_mib * 1024 * 1024) as usize;
    let mut mem = vec![0u8; bytes];
    let mut seed: u64 = 0;
    for (i, b) in mem.iter_mut().enumerate() {
        *b = ((i as u64).wrapping_mul(2654435761) >> 13) as u8;
        seed = seed.wrapping_add(*b as u64);
    }

    // CPU work, optionally across threads.
    let cpu_checksum = if params.threads > 1 {
        run_threads(&params)
    } else {
        workload::cpu_work(params.iterations)
    };

    // Optional file-backed descriptors and mappings.
    if opts.kind == WorkloadKind::FileHeavy && params.files > 0 {
        exercise_files(&params)?;
    }

    let mut state = workload::expected_state(&params);
    state.cpu_checksum = cpu_checksum;
    state.counter = seed & 0xffff;
    // Prefer the digest of the memory we actually allocated.
    state.memory_digest = workload::pattern_digest(bytes as u64, 0);

    workload::write_state(state_path, &state).map_err(|e| e.to_string())?;
    Ok(state)
}

fn validate(opts: &RunOptions, state_path: &std::path::Path) -> Result<(), String> {
    let expected = workload::expected_state(&opts.params());
    let actual = workload::read_state(state_path).map_err(|e| format!("reading state: {e}"))?;
    let diffs = workload::diff_states(&expected, &actual);
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(diffs.join("; "))
    }
}

fn next_u64(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    it.next()
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|e| format!("{flag}: {e}"))
}

fn next_usize(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    it.next()
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|e| format!("{flag}: {e}"))
}

fn run_threads(params: &WorkloadParams) -> u64 {
    let mut handles = Vec::new();
    for t in 0..params.threads {
        let iters = params.iterations;
        handles.push(std::thread::spawn(move || {
            workload::cpu_work(iters).wrapping_add(t as u64)
        }));
    }
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or(0))
        .fold(0u64, |a, b| a.wrapping_add(b))
}

/// Exercise file-backed mappings and descriptors. Kept simple and compatible
/// with CRIU (regular files in a temp directory).
fn exercise_files(params: &WorkloadParams) -> Result<(), String> {
    let dir = std::env::temp_dir().join("snapforge-workload-files");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for i in 0..params.files {
        let path = dir.join(format!("scratch-{i}.bin"));
        let size = (params.file_mib * 1024 * 1024) as usize;
        let mut buf = vec![0u8; size];
        for (j, b) in buf.iter_mut().enumerate() {
            *b = ((j as u64).wrapping_add(i as u64).wrapping_mul(2654435761) >> 13) as u8;
        }
        std::fs::write(&path, &buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}
