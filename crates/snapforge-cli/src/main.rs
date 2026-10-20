//! SnapForge CLI.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use snapforge_core::config::DEFAULT_CONCURRENCY;
use snapforge_core::{host, Config, DEFAULT_STORE_DIR};
use snapforge_engine::engine::{CheckpointOptions, RestoreOptions};
use snapforge_engine::Engine;
use snapforge_format::{FormatError, ValidateOptions};

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
        /// PID of the process to checkpoint.
        #[arg(long)]
        pid: i32,

        /// Directory to receive the CRIU image files.
        #[arg(long)]
        output: PathBuf,

        /// Do not leave the target process running after capture.
        #[arg(long, default_value_t = false)]
        leave_running: bool,

        /// Operation timeout in seconds (0 disables the timeout).
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
    },

    /// Inspect a stored snapshot manifest.
    Inspect {
        /// Snapshot id, or a path to a snapshot directory.
        snapshot: String,

        /// Resolve and report the full parent chain, with reuse statistics.
        #[arg(long, default_value_t = false)]
        chain: bool,
    },

    /// Verify snapshot artifacts and integrity.
    Verify {
        /// Snapshot id, or a path to a snapshot directory.
        snapshot: String,

        /// Also require all referenced chunks to be present in the CAS store.
        #[arg(long, default_value_t = false)]
        require_chunks: bool,

        /// Verify every snapshot in the parent chain, not just this one.
        #[arg(long, default_value_t = false)]
        chain: bool,
    },

    /// Store a directory of artifacts into content-addressed storage as a
    /// published snapshot.
    Store {
        /// Directory containing the artifacts to ingest.
        source: PathBuf,

        /// Snapshot id to publish (defaults to the source directory name).
        #[arg(long)]
        id: Option<String>,

        /// Workload identifier to record in the manifest.
        #[arg(long, default_value = "unknown")]
        workload: String,

        /// Optional parent snapshot id (records an incremental relationship).
        #[arg(long)]
        parent: Option<String>,
    },

    /// Transfer a snapshot to another store (local filesystem).
    Transfer {
        /// Snapshot id in the source store.
        snapshot: String,

        /// Destination store directory.
        dest: PathBuf,

        /// Maximum chunk copies in flight.
        #[arg(long)]
        transfer_concurrency: Option<usize>,

        /// Maximum retry attempts per chunk.
        #[arg(long, default_value_t = 3)]
        max_retries: u32,
    },

    /// Restore a process from a checkpoint (requires a CRIU-capable host).
    Restore {
        /// Directory containing the CRIU image files.
        #[arg(long)]
        input: PathBuf,

        /// Operation timeout in seconds (0 disables the timeout).
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
    },

    /// Report process and checkpoint readiness status.
    Status {
        /// Optional process ID to inspect via /proc.
        #[arg(long)]
        pid: Option<i32>,
    },

    /// List stored snapshots.
    List,

    /// Run storage-only benchmarks (clearly labeled as non-CRIU experiments).
    ///
    /// This measures the SnapForge storage pipeline in isolation. It does NOT
    /// capture a process; output is explicitly labeled `storage_only`.
    Bench {
        /// Data size to ingest, in MiB.
        #[arg(long, default_value_t = 256)]
        size_mib: u64,

        /// Path to a source directory to ingest instead of synthetic data.
        #[arg(long)]
        input: Option<PathBuf>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        bench_json: bool,
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
        Command::Checkpoint {
            pid,
            output,
            leave_running,
            timeout_secs,
        } => run_checkpoint(&cfg, pid, output, leave_running, timeout_secs),
        Command::Inspect { snapshot, chain } => run_inspect(&cfg, &snapshot, chain),
        Command::Verify {
            snapshot,
            require_chunks,
            chain,
        } => run_verify(&cfg, &snapshot, require_chunks, chain),
        Command::Store {
            source,
            id,
            workload,
            parent,
        } => run_store(&cfg, &source, id, workload, parent),
        Command::Transfer {
            snapshot,
            dest,
            transfer_concurrency,
            max_retries,
        } => run_transfer(&cfg, &snapshot, &dest, transfer_concurrency, max_retries),
        Command::Restore {
            input,
            timeout_secs,
        } => run_restore(&cfg, input, timeout_secs),
        Command::Status { pid } => run_status(&cfg, pid),
        Command::List => not_implemented("list", 3),
        Command::Bench {
            size_mib,
            input,
            bench_json,
        } => run_bench(&cfg, size_mib, input, bench_json),
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

fn timeout_from_secs(secs: u64) -> Option<std::time::Duration> {
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
}

fn run_checkpoint(
    cfg: &Config,
    pid: i32,
    output: PathBuf,
    leave_running: bool,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let mut engine = Engine::new().map_err(capability_hint)?;

    let mut opts = CheckpointOptions::new(pid, &output);
    opts.leave_running = leave_running;
    opts.timeout = timeout_from_secs(timeout_secs);

    let outcome = engine.checkpoint(&opts).map_err(capability_hint)?;

    if cfg.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "checkpoint captured: pid={} dir={} artifacts={} bytes={} duration_ms={}",
            outcome.pid,
            outcome.output_dir.display(),
            outcome.artifact_count,
            outcome.total_bytes,
            outcome.duration_ms
        );
        if let Some(v) = &outcome.criu_version {
            println!("criu version: {v}");
        }
    }
    Ok(())
}

fn run_restore(cfg: &Config, input: PathBuf, timeout_secs: u64) -> anyhow::Result<()> {
    let mut engine = Engine::new().map_err(capability_hint)?;

    let mut opts = RestoreOptions::new(&input);
    opts.timeout = timeout_from_secs(timeout_secs);

    let outcome = engine.restore(&opts).map_err(capability_hint)?;

    if cfg.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "restore succeeded: dir={} duration_ms={}",
            outcome.input_dir.display(),
            outcome.duration_ms
        );
    }
    Ok(())
}

fn run_status(cfg: &Config, pid: Option<i32>) -> anyhow::Result<()> {
    let report = host::probe();
    match pid {
        Some(pid) => {
            // Process status does not require CRIU.
            let status = snapforge_engine::engine::status_of(pid);
            if cfg.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if status.exists {
                println!(
                    "pid {}: state={} name={} threads={}",
                    pid,
                    status.state.as_deref().unwrap_or("?"),
                    status.name.as_deref().unwrap_or("?"),
                    status.threads.unwrap_or(0)
                );
            } else {
                println!("pid {pid}: not found");
            }
        }
        None => {
            if cfg.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("criu readiness: {:?}", report.criu_readiness);
            }
        }
    }
    Ok(())
}

/// Turn an engine capability error into an actionable message.
/// Resolve a snapshot argument to a directory: either an id within the store,
/// or an existing path.
fn resolve_snapshot(cfg: &Config, arg: &str) -> PathBuf {
    let as_path = PathBuf::from(arg);
    if as_path.is_dir() {
        return as_path;
    }
    cfg.snapshots_dir().join(arg)
}

fn run_inspect(cfg: &Config, snapshot: &str, chain: bool) -> anyhow::Result<()> {
    let dir = resolve_snapshot(cfg, snapshot);
    let manifest = snapforge_format::load(&dir)
        .map_err(|e| anyhow::anyhow!("failed to load snapshot at {}: {e}", dir.display()))?;

    // Inspect reports structure but does not fail on a missing integrity digest;
    // it validates everything else so operators can see what is wrong.
    let opts = ValidateOptions {
        check_integrity: false,
        available_chunks: None,
    };
    let validation = snapforge_format::validate(&manifest, &opts);

    if cfg.json {
        let (valid, err) = match &validation {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        let mut out = serde_json::json!({
            "path": dir.display().to_string(),
            "manifest": manifest,
            "valid": valid,
            "error": err,
        });
        if chain {
            out["chain"] = chain_report(cfg, &manifest.snapshot_id)?;
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("snapshot:   {}", manifest.snapshot_id);
        println!("path:       {}", dir.display());
        println!("format:     v{}", manifest.format_version);
        println!("captured:   {}", manifest.captured_at);
        println!("workload:   {}", manifest.workload.id);
        println!(
            "compat:     arch={} kernel={} criu={}",
            manifest.compatibility.arch,
            manifest.compatibility.kernel_release,
            manifest
                .compatibility
                .criu_version
                .as_deref()
                .unwrap_or("?")
        );
        if let Some(p) = &manifest.parent {
            println!("parent:     {p}");
        }
        println!("artifacts:  {}", manifest.artifacts.len());
        println!("total bytes:{}", manifest.total_bytes());
        let s = &manifest.stats;
        println!(
            "chunks:     distinct={} refs={} dedup_saved={}",
            s.distinct_chunks, s.total_chunk_refs, s.dedup_saved_bytes
        );
        if let (Some(reused), Some(new), Some(reused_bytes)) =
            (s.reused_chunks, s.new_chunks, s.reused_bytes)
        {
            println!(
                "incremental:reused_chunks={reused} new_chunks={new} reused_bytes={reused_bytes}"
            );
        }
        println!(
            "integrity:  {}",
            manifest.integrity.as_deref().unwrap_or("<missing>")
        );
        match &validation {
            Ok(_) => println!("validation: OK"),
            Err(e) => println!("validation: FAILED ({e})"),
        }
        if chain {
            print_chain(cfg, &manifest.snapshot_id)?;
        }
    }

    if validation.is_err() {
        anyhow::bail!("snapshot manifest failed validation");
    }
    Ok(())
}

/// Resolve the parent chain of `tip` and build a JSON report with per-link
/// reuse relative to the immediate parent.
fn chain_report(cfg: &Config, tip: &str) -> anyhow::Result<serde_json::Value> {
    let snapshots_dir = cfg.snapshots_dir();
    let chain = snapforge_storage::resolve_chain(tip, |id| {
        snapforge_format::load(&snapshots_dir.join(id)).ok()
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let manifests: Vec<snapforge_format::Manifest> = chain
        .ids
        .iter()
        .map(|id| {
            snapforge_format::load(&snapshots_dir.join(id))
                .map_err(|e| anyhow::anyhow!("loading {id}: {e}"))
        })
        .collect::<anyhow::Result<_>>()?;

    let mut links = Vec::new();
    for i in 1..manifests.len() {
        let report = snapforge_storage::ReuseReport::compute(&manifests[i], &manifests[i - 1]);
        links.push(serde_json::to_value(&report)?);
    }

    let analysis = snapforge_storage::analyze_compaction(&manifests);

    Ok(serde_json::json!({
        "chain": chain.ids,
        "depth": chain.depth(),
        "links": links,
        "compaction": analysis,
    }))
}

/// Human-readable chain printing for `inspect --chain`.
fn print_chain(cfg: &Config, tip: &str) -> anyhow::Result<()> {
    let report = chain_report(cfg, tip)?;
    let ids = report["chain"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    println!("chain:      {}", ids.join(" -> "));
    if let Some(links) = report["links"].as_array() {
        for link in links {
            println!(
                "  {} <- {} : reused {}/{} chunks ({:.1}% bytes)",
                link["child"].as_str().unwrap_or("?"),
                link["parent"].as_str().unwrap_or("?"),
                link["reused_chunks"].as_u64().unwrap_or(0),
                link["child_chunks"].as_u64().unwrap_or(0),
                link["reuse_ratio"].as_f64().unwrap_or(0.0) * 100.0
            );
        }
    }
    let c = &report["compaction"];
    println!(
        "  compaction: {} snapshots, {} chain chunks, {} droppable ({} bytes)",
        c["snapshots"].as_u64().unwrap_or(0),
        c["chain_chunks"].as_u64().unwrap_or(0),
        c["chunks_droppable"].as_u64().unwrap_or(0),
        c["bytes_droppable"].as_u64().unwrap_or(0)
    );
    Ok(())
}

fn run_verify(
    cfg: &Config,
    snapshot: &str,
    require_chunks: bool,
    chain: bool,
) -> anyhow::Result<()> {
    let dir = resolve_snapshot(cfg, snapshot);
    let manifest = snapforge_format::load(&dir)
        .map_err(|e| anyhow::anyhow!("failed to load snapshot at {}: {e}", dir.display()))?;

    // Assemble the set of manifests to verify: just the tip, or the whole chain.
    let manifests: Vec<snapforge_format::Manifest> = if chain {
        let snapshots_dir = cfg.snapshots_dir();
        let c = snapforge_storage::resolve_chain(&manifest.snapshot_id, |id| {
            snapforge_format::load(&snapshots_dir.join(id)).ok()
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        c.ids
            .iter()
            .map(|id| {
                snapforge_format::load(&snapshots_dir.join(id))
                    .map_err(|e| anyhow::anyhow!("loading {id}: {e}"))
            })
            .collect::<anyhow::Result<_>>()?
    } else {
        vec![manifest.clone()]
    };

    // Structural + integrity validation for every manifest in scope.
    for m in &manifests {
        let available = if require_chunks {
            Some(collect_cas_chunks(cfg))
        } else {
            None
        };
        if let Err(e) = snapforge_format::validate(
            m,
            &ValidateOptions {
                check_integrity: true,
                available_chunks: available,
            },
        ) {
            if cfg.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "snapshot": m.snapshot_id,
                        "valid": false,
                        "error": e.to_string(),
                    }))?
                );
            }
            return Err(match e {
                FormatError::IntegrityMismatch { .. } => {
                    anyhow::anyhow!("integrity check failed for {}", m.snapshot_id)
                }
                other => anyhow::anyhow!("verification failed: {other}"),
            });
        }
    }

    // When verifying a chain with chunk checks, verify each distinct chunk's
    // content once via the CAS (stronger than presence-only checks).
    if chain && require_chunks {
        let cas = snapforge_storage::Cas::open(cfg.cas_dir())
            .map_err(|e| anyhow::anyhow!("opening CAS: {e}"))?;
        snapforge_storage::verify_chain(&cas, &manifests)
            .map_err(|e| anyhow::anyhow!("chain chunk verification failed: {e}"))?;
    }

    if cfg.json {
        let ids: Vec<&str> = manifests.iter().map(|m| m.snapshot_id.as_str()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "snapshots": ids,
                "valid": true,
            }))?
        );
    } else if chain {
        let ids: Vec<&str> = manifests.iter().map(|m| m.snapshot_id.as_str()).collect();
        println!("chain OK: {}", ids.join(" -> "));
    } else {
        println!("{}: OK", manifest.snapshot_id);
    }
    Ok(())
}

/// List chunk ids present in the content-addressed store (by file name).
fn collect_cas_chunks(cfg: &Config) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(cfg.cas_dir()) {
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                set.insert(name.to_string());
            }
        }
    }
    set
}

#[allow(clippy::too_many_arguments)]
fn run_store(
    cfg: &Config,
    source: &std::path::Path,
    id: Option<String>,
    workload: String,
    parent: Option<String>,
) -> anyhow::Result<()> {
    if !source.is_dir() {
        anyhow::bail!("source {} is not a directory", source.display());
    }

    let snapshot_id = id.unwrap_or_else(|| {
        source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("snapshot")
            .to_string()
    });
    snapforge_format::validate_snapshot_id(&snapshot_id)
        .map_err(|e| anyhow::anyhow!("invalid snapshot id: {e}"))?;

    // Collect artifact files (top-level, non-recursive, deterministic order).
    let mut files: Vec<PathBuf> = std::fs::read_dir(source)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", source.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    if files.is_empty() {
        anyhow::bail!("source {} contains no files to store", source.display());
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        cfg.ensure_dirs()
            .await
            .map_err(|e| anyhow::anyhow!("creating store dirs: {e}"))?;

        let cas = std::sync::Arc::new(
            snapforge_storage::Cas::open(cfg.cas_dir())
                .map_err(|e| anyhow::anyhow!("opening CAS: {e}"))?,
        );

        let opts = snapforge_storage::PipelineOptions {
            chunk_size: cfg.chunk_size,
            concurrency: cfg.concurrency,
            verify_on_write: false,
        };

        let mut artifacts = Vec::new();
        let mut stats = snapforge_format::SnapshotStats::default();
        let mut distinct: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        // If this is an incremental snapshot, resolve and validate the parent
        // chain up front so a broken lineage fails before any work.
        let parent_manifest = if let Some(ref parent_id) = parent {
            let parent_dir = cfg.snapshots_dir().join(parent_id);
            if !parent_dir.is_dir() {
                return Err(anyhow::anyhow!(
                    "parent snapshot `{parent_id}` not found under {}",
                    cfg.snapshots_dir().display()
                ));
            }
            let pm = snapforge_format::load_validated(
                &parent_dir,
                &snapforge_format::ValidateOptions::default(),
            )
            .map_err(|e| anyhow::anyhow!("parent snapshot `{parent_id}` is invalid: {e}"))?
            .into_inner();
            // Reject cycles: the new child must not appear in the parent chain.
            let snapshots_dir = cfg.snapshots_dir();
            let chain = snapforge_storage::resolve_chain(&pm.snapshot_id, |pid| {
                snapforge_format::load(&snapshots_dir.join(pid)).ok()
            });
            match chain {
                Ok(c) => {
                    if c.ids.iter().any(|id| id == &snapshot_id) {
                        return Err(anyhow::anyhow!(
                            "incremental parent would create a cycle through `{snapshot_id}`"
                        ));
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("invalid parent chain: {e}")),
            }
            Some(pm)
        } else {
            None
        };

        for file in &files {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 file name: {}", file.display()))?
                .to_string();
            let f = std::fs::File::open(file)
                .map_err(|e| anyhow::anyhow!("opening {}: {e}", file.display()))?;
            let ingested = snapforge_storage::store_reader(cas.clone(), f, opts.clone())
                .await
                .map_err(|e| anyhow::anyhow!("ingesting {}: {e}", file.display()))?;

            stats.total_bytes += ingested.size;
            stats.total_chunk_refs += ingested.chunks.len() as u64;
            stats.dedup_saved_bytes += ingested.stats.dedup_saved_bytes;
            for c in &ingested.chunks {
                distinct.insert(c.id.clone());
            }
            artifacts.push(ingested.into_artifact(name));
        }
        stats.distinct_chunks = distinct.len() as u64;

        let mut manifest = snapforge_format::Manifest {
            format_version: snapforge_format::FORMAT_VERSION,
            snapshot_id: snapshot_id.clone(),
            parent: parent.clone(),
            workload: snapforge_format::WorkloadInfo {
                id: workload,
                description: None,
            },
            captured_at: now_rfc3339(),
            compatibility: snapforge_format::Compatibility {
                arch: std::env::consts::ARCH.into(),
                kernel_release: host::probe().kernel_release,
                criu_version: None,
                criu_image_version: None,
            },
            artifacts,
            stats: stats.clone(),
            integrity: None,
        };

        // Compute incremental reuse against the parent, if any, and record it.
        let reuse = parent_manifest
            .as_ref()
            .map(|pm| snapforge_storage::ReuseReport::compute(&manifest, pm));
        if let Some(r) = &reuse {
            manifest.stats.reused_chunks = Some(r.reused_chunks);
            manifest.stats.new_chunks = Some(r.new_chunks);
            manifest.stats.reused_bytes = Some(r.reused_bytes);
            stats = manifest.stats.clone();
        }

        let manifest = manifest
            .sealed()
            .map_err(|e| anyhow::anyhow!("sealing manifest: {e}"))?;

        // Stage then publish atomically.
        let staging = cfg.staging_dir().join(&snapshot_id);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).ok();
        }
        snapforge_format::write_to_dir(&staging, &manifest)
            .map_err(|e| anyhow::anyhow!("staging manifest: {e}"))?;

        let published = match snapforge_format::publish(
            &staging,
            &cfg.snapshots_dir(),
            &snapforge_format::ValidateOptions::default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                snapforge_format::cleanup_staging(&staging);
                return Err(anyhow::anyhow!("publishing snapshot: {e}"));
            }
        };

        if cfg.json {
            let mut value = serde_json::json!({
                "snapshot": snapshot_id,
                "path": published.display().to_string(),
                "stats": stats,
            });
            if let Some(r) = &reuse {
                value["reuse"] = serde_json::to_value(r)?;
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("stored snapshot: {snapshot_id}");
            println!("  path:          {}", published.display());
            println!("  artifacts:     {}", manifest.artifacts.len());
            println!("  total bytes:   {}", stats.total_bytes);
            println!("  chunk refs:    {}", stats.total_chunk_refs);
            println!("  distinct:      {}", stats.distinct_chunks);
            println!("  dedup saved:   {}", stats.dedup_saved_bytes);
            if let Some(r) = &reuse {
                println!("  incremental:   parent={}", r.parent);
                println!(
                    "  reuse:         {}/{} chunks, {} bytes reused, {:.1}% by bytes",
                    r.reused_chunks,
                    r.child_chunks,
                    r.reused_bytes,
                    r.reuse_ratio * 100.0
                );
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

/// Storage-only benchmark. See the `bench` subcommand help.
fn run_bench(
    cfg: &Config,
    size_mib: u64,
    input: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<()> {
    // Generate deterministic, incompressible-ish synthetic data (or read a dir).
    let bytes: Vec<u8> = if let Some(dir) = &input {
        let mut all = Vec::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for f in files {
            all.extend_from_slice(&std::fs::read(&f).map_err(|e| anyhow::anyhow!("{e}"))?);
        }
        all
    } else {
        let len = (size_mib as usize) * 1024 * 1024;
        let mut v = Vec::with_capacity(len);
        let mut x = 0x243F_6A88_85A3_08D3u64;
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push((x >> 24) as u8);
        }
        v
    };

    let total_bytes = bytes.len() as u64;

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        cfg.ensure_dirs()
            .await
            .map_err(|e| anyhow::anyhow!("creating store dirs: {e}"))?;
        let cas = std::sync::Arc::new(
            snapforge_storage::Cas::open(cfg.cas_dir())
                .map_err(|e| anyhow::anyhow!("opening CAS: {e}"))?,
        );
        let opts = snapforge_storage::PipelineOptions {
            chunk_size: cfg.chunk_size,
            concurrency: cfg.concurrency,
            verify_on_write: false,
        };
        let ingested = snapforge_storage::store_reader(cas, std::io::Cursor::new(bytes), opts)
            .await
            .map_err(|e| anyhow::anyhow!("ingest failed: {e}"))?;
        Ok::<_, anyhow::Error>(ingested)
    })?;

    let s = &result.stats;
    let total_ns = s.total_time_ns.max(1);
    let throughput_mib_s = (total_bytes as f64 / (1024.0 * 1024.0)) / (total_ns as f64 / 1e9);
    let peak_rss_kib = read_peak_rss_kib();

    let record = serde_json::json!({
        // Explicit label: this is NOT a process checkpoint.
        "kind": "storage_only",
        "label": "storage-only experiment (not a process checkpoint)",
        "chunk_size": cfg.chunk_size,
        "concurrency": cfg.concurrency,
        "total_bytes": total_bytes,
        "chunks": s.chunk_refs,
        "chunks_written": s.chunks_written,
        "chunks_deduplicated": s.chunks_deduplicated,
        "dedup_saved_bytes": s.dedup_saved_bytes,
        "peak_in_flight": s.peak_in_flight,
        "chunking_time_ns": s.chunking_time_ns,
        "storage_time_ns": s.storage_time_ns,
        "total_time_ns": total_ns,
        "throughput_mib_s": throughput_mib_s,
        "peak_rss_kib": peak_rss_kib,
    });

    if json || cfg.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        println!("storage-only experiment (not a process checkpoint)");
        println!("  chunk size:    {}", cfg.chunk_size);
        println!("  concurrency:   {}", cfg.concurrency);
        println!("  bytes:         {total_bytes}");
        println!("  throughput:    {:.1} MiB/s", throughput_mib_s);
        println!("  total:         {:.1} ms", total_ns as f64 / 1e6);
        println!("  dedup saved:   {} bytes", s.dedup_saved_bytes);
    }
    Ok(())
}

/// Read peak resident set size (VmHWM) from `/proc/self/status`, in KiB.
fn read_peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kib = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib);
        }
    }
    None
}

/// RFC 3339 timestamp without pulling in a time crate.
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Civil-from-days conversion (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[allow(clippy::too_many_arguments)]
fn run_transfer(
    cfg: &Config,
    snapshot: &str,
    dest: &std::path::Path,
    transfer_concurrency: Option<usize>,
    max_retries: u32,
) -> anyhow::Result<()> {
    // Source layout is the current store.
    let source_snapshots = cfg.snapshots_dir();
    if !source_snapshots.join(snapshot).is_dir() {
        anyhow::bail!(
            "snapshot `{snapshot}` not found under {}",
            source_snapshots.display()
        );
    }

    // Destination layout mirrors Config's structure.
    let dest_cas_dir = dest.join("cas");
    let dest_snapshots = dest.join("snapshots");
    let dest_staging = dest.join("staging");
    for d in [&dest_cas_dir, &dest_snapshots, &dest_staging] {
        std::fs::create_dir_all(d).map_err(|e| anyhow::anyhow!("creating {}: {e}", d.display()))?;
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let source_cas = std::sync::Arc::new(
            snapforge_storage::Cas::open(cfg.cas_dir())
                .map_err(|e| anyhow::anyhow!("opening source CAS: {e}"))?,
        );
        let dest_cas = std::sync::Arc::new(
            snapforge_storage::Cas::open(&dest_cas_dir)
                .map_err(|e| anyhow::anyhow!("opening destination CAS: {e}"))?,
        );

        let opts = snapforge_storage::TransferOptions {
            concurrency: transfer_concurrency.unwrap_or(cfg.concurrency),
            max_retries,
            ..snapforge_storage::TransferOptions::default()
        };

        let outcome = snapforge_storage::transfer_snapshot(
            source_cas,
            &source_snapshots,
            dest_cas,
            &dest_snapshots,
            &dest_staging,
            snapshot,
            opts,
        )
        .await
        .map_err(|e| anyhow::anyhow!("transfer failed: {e}"))?;

        if cfg.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "snapshot": outcome.snapshot_id,
                    "destination": outcome.destination.display().to_string(),
                    "chunks_total": outcome.chunks_total,
                    "chunks_copied": outcome.chunks_copied,
                    "chunks_skipped": outcome.chunks_skipped,
                    "bytes_transferred": outcome.bytes_transferred,
                    "retries": outcome.retries,
                }))?
            );
        } else {
            println!("transferred snapshot: {}", outcome.snapshot_id);
            println!("  destination:     {}", outcome.destination.display());
            println!("  chunks total:    {}", outcome.chunks_total);
            println!("  chunks copied:   {}", outcome.chunks_copied);
            println!("  chunks skipped:  {}", outcome.chunks_skipped);
            println!("  bytes copied:    {}", outcome.bytes_transferred);
            println!("  retries:         {}", outcome.retries);
        }
        Ok::<(), anyhow::Error>(())
    })
}

fn capability_hint(e: snapforge_engine::EngineError) -> anyhow::Error {
    if e.is_capability_block() {
        anyhow::anyhow!(
            "{e}\n\
             hint: run `snapforge doctor` for details. Real checkpoint/restore needs a kernel \
             built with CONFIG_CHECKPOINT_RESTORE, the ns_last_pid sysctl, a criu binary, and \
             root or CAP_SYS_PTRACE. SnapForge never fabricates checkpoint data."
        )
    } else {
        anyhow::anyhow!(e)
    }
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
