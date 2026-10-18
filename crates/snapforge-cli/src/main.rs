//! SnapForge CLI.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use snapforge_core::config::DEFAULT_CONCURRENCY;
use snapforge_core::{host, Config, DEFAULT_STORE_DIR};

#[derive(Parser, Debug)]
#[command(
    name = "snapforge",
    version,
    about = "High-performance Linux checkpoint, snapshot & restore engine",
    long_about = "SnapForge orchestrates CRIU-based process checkpoint/restore and provides \
                  a versioned snapshot format, content-addressed storage, and a bounded \
                  parallel transfer pipeline.\n\n\
                  SnapForge never fabricates checkpoint data: when the host cannot run CRIU, \
                  commands that require it fail with an explicit diagnostic."
)]
struct Cli {
    /// Root directory for content-addressed storage and snapshots.
    #[arg(long, global = true, default_value = DEFAULT_STORE_DIR)]
    store: PathBuf,

    /// Bounded concurrency for storage/transfer operations.
    #[arg(long, global = true, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,

    /// Chunk size in bytes (power of two).
    #[arg(long, global = true, default_value_t = 1024 * 1024)]
    chunk_size: usize,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect host capabilities (kernel features, CRIU, privileges).
    Doctor,

    /// Run a deterministic workload harness (Phase 1 harness).
    Workload {
        /// Which bundled workload to run.
        #[arg(long, value_enum, default_value_t = WorkloadKind::CpuMemory)]
        kind: WorkloadKind,

        /// Anonymous memory footprint in MiB.
        #[arg(long, default_value_t = 64)]
        memory_mib: u64,

        /// Number of worker threads.
        #[arg(long, default_value_t = 1)]
        threads: usize,

        /// CPU work iterations per worker.
        #[arg(long, default_value_t = 1_000_000)]
        iterations: u64,

        /// Number of scratch files (file-heavy workload).
        #[arg(long, default_value_t = 8)]
        files: usize,

        /// Size of each scratch file in MiB (file-heavy workload).
        #[arg(long, default_value_t = 16)]
        file_mib: u64,

        /// Validate restored state instead of running fresh (workload harness mode).
        #[arg(long)]
        validate: bool,

        /// Path to a JSON state file for the readiness/validation protocol.
        #[arg(long)]
        state_file: Option<PathBuf>,
    },

    /// Capture a process checkpoint using CRIU (requires a CRIU-capable host).
    Checkpoint {
        #[arg(long)]
        pid: Option<i32>,
        #[arg(long)]
        workload: Option<String>,
    },

    /// Inspect a stored snapshot manifest.
    Inspect { snapshot: String },

    /// Verify snapshot artifacts and integrity.
    Verify { snapshot: String },

    /// Store checkpoint artifacts into content-addressed storage.
    Store { source: PathBuf },

    /// Transfer a snapshot to another store.
    Transfer { snapshot: String, dest: PathBuf },

    /// Restore a process from a checkpoint (requires a CRIU-capable host).
    Restore { snapshot: String },

    /// List stored snapshots.
    List,

    /// Run storage-only benchmarks (clearly labeled as non-CRIU experiments).
    Bench {
        /// Path to benchmark input, if any.
        #[arg(long)]
        input: Option<PathBuf>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum WorkloadKind {
    CpuMemory,
    FileHeavy,
    Multithreaded,
}

impl WorkloadKind {
    fn binary_name(self) -> &'static str {
        match self {
            WorkloadKind::CpuMemory => "workload-cpu-memory",
            WorkloadKind::FileHeavy => "workload-file-heavy",
            WorkloadKind::Multithreaded => "workload-multithreaded",
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    snapforge_core::logging::init(cli.json);

    let cfg = Config {
        store_dir: cli.store,
        concurrency: cli.concurrency,
        chunk_size: cli.chunk_size,
        timeout: None,
        json: cli.json,
    };
    cfg.validate()?;

    match cli.command {
        Command::Doctor => run_doctor(&cfg),
        Command::Workload {
            kind,
            memory_mib,
            threads,
            iterations,
            files,
            file_mib,
            validate,
            state_file,
        } => run_workload(
            kind, memory_mib, threads, iterations, files, file_mib, validate, state_file,
        ),
        Command::Checkpoint { .. } => not_implemented("checkpoint", 2),
        Command::Inspect { .. } => not_implemented("inspect", 3),
        Command::Verify { .. } => not_implemented("verify", 3),
        Command::Store { .. } => not_implemented("store", 4),
        Command::Transfer { .. } => not_implemented("transfer", 5),
        Command::Restore { .. } => not_implemented("restore", 2),
        Command::List => not_implemented("list", 3),
        Command::Bench { .. } => not_implemented("bench", 4),
    }
}

fn run_doctor(cfg: &Config) -> anyhow::Result<()> {
    let report = host::probe();
    if cfg.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render());
    }
    // Doctor is a diagnostic; a missing CRIU/kernel feature is reported but is
    // not an error exit, so scripts can still inspect the environment.
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_workload(
    kind: WorkloadKind,
    memory_mib: u64,
    threads: usize,
    iterations: u64,
    files: usize,
    file_mib: u64,
    validate: bool,
    state_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
    let binary = dir.join(kind.binary_name());
    let binary = if binary.exists() {
        binary
    } else {
        // Fall back to PATH lookup (useful when run via `cargo run -p`).
        which(kind.binary_name()).unwrap_or(binary)
    };

    let mut cmd = std::process::Command::new(&binary);
    cmd.arg("--memory-mib")
        .arg(memory_mib.to_string())
        .arg("--threads")
        .arg(threads.to_string())
        .arg("--iterations")
        .arg(iterations.to_string())
        .arg("--files")
        .arg(files.to_string())
        .arg("--file-mib")
        .arg(file_mib.to_string());
    if validate {
        cmd.arg("--validate");
    }
    if let Some(sf) = state_file {
        cmd.arg("--state-file").arg(sf);
    }

    let status = cmd.status().map_err(|e| {
        anyhow::anyhow!("failed to launch workload binary {}: {e}", binary.display())
    })?;
    if !status.success() {
        anyhow::bail!("workload exited with status {status}");
    }
    Ok(())
}

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

fn not_implemented(name: &str, phase: u8) -> anyhow::Result<()> {
    anyhow::bail!(
        "`{name}` is not implemented yet (planned for Phase {phase}). \
         Run `snapforge doctor` to inspect host capabilities."
    )
}
