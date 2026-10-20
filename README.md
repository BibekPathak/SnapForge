# SnapForge

High-performance Linux checkpoint, snapshot, and restore engine.

SnapForge orchestrates **CRIU**-based process checkpoint/restore and adds a
versioned snapshot format, content-addressed chunk storage, a bounded parallel
transfer pipeline, and a reproducible benchmark harness.

> **Status: Phase 8 complete.** The workspace, `snapforge doctor`, the
> deterministic workload harness, the CRIU orchestration engine
> (`checkpoint`, `restore`, `status`), the versioned snapshot format
> (`inspect`, `verify`), the content-addressed storage pipeline (`store`),
> reliable local snapshot transfer (`transfer`), content-addressed incremental
> snapshots with lineage (`store --parent`, `inspect --chain`, `verify --chain`),
> snapshot extraction (`extract`), and the reproducible benchmark harness
> (storage, transfer, incremental) are implemented and tested. See
> [`docs/performance-report.md`](docs/performance-report.md) for measured
> results on the current host.

## Design principle

SnapForge **never fabricates checkpoint data**. CRIU performs the actual
process-state capture and restoration; SnapForge owns the workflow, artifact
representation, storage, validation, and measurement. When the host cannot run
CRIU, commands that need it fail with an explicit diagnostic instead of
pretending to succeed.

## Host requirements for real checkpoint/restore

Real CRIU operation requires:

- A kernel built with `CONFIG_CHECKPOINT_RESTORE=y`.
- `/proc/sys/kernel/ns_last_pid` present.
- Either root or `CAP_SYS_PTRACE` (and related namespace privileges).
- A `criu` binary on `PATH` (CRIU >= 3.15).

Run `snapforge doctor` to check all of the above at once. See
[`docs/compatibility.md`](docs/compatibility.md) for the current host's status.

## Quick start

```bash
# Inspect host capabilities (always safe to run).
cargo run --release -p snapforge-cli -- doctor
cargo run --release -p snapforge-cli -- doctor --json

# Run a deterministic workload and validate its state.
cargo run --release -p snapforge-cli -- workload --kind cpu-memory \
    --memory-mib 64 --threads 2 --state-file /tmp/sf-state.json
cargo run --release -p snapforge-cli -- workload --kind cpu-memory \
    --memory-mib 64 --threads 2 --validate --state-file /tmp/sf-state.json

# Report process lifecycle status (no CRIU required).
cargo run --release -p snapforge-cli -- status --pid $$

# Capture and restore a real checkpoint (requires a CRIU-capable host).
cargo run --release -p snapforge-cli -- checkpoint --pid <PID> --output /tmp/ckpt
cargo run --release -p snapforge-cli -- restore --input /tmp/ckpt

# Snapshot format: build a fixture, then inspect and verify it.
cargo run --release -p snapforge-format --example make_fixture -- /tmp/sf-format-store
cargo run --release -p snapforge-cli -- --store /tmp/sf-format-store inspect fixture-1
cargo run --release -p snapforge-cli -- --store /tmp/sf-format-store verify fixture-1 --require-chunks

# Storage: ingest a directory of artifacts into content-addressed storage.
cargo run --release -p snapforge-cli -- --store /tmp/sf-store --chunk-size 262144 \
    store /path/to/artifacts --id ckpt-1 --workload cpu-memory
cargo run --release -p snapforge-cli -- --store /tmp/sf-store verify ckpt-1 --require-chunks

# Storage-layer microbenchmarks.
cargo bench -p snapforge-storage

# Storage-only end-to-end benchmark (explicitly NOT a process checkpoint).
cargo run --release -p snapforge-cli -- --store /tmp/sf-bench --chunk-size 262144 \
    --concurrency 4 --json bench --size-mib 256

# Full reproducible benchmark suite (writes JSON/CSV/summary/plots).
CARGO_TARGET_DIR=/tmp/snapforge-target cargo build --release -p snapforge-cli
python3 benchmarks/run.py --quick --out /tmp/sf-bench-out   # smoke test
python3 benchmarks/run.py --sizes 64,256 --repetitions 3 \
    --workdir /var/tmp/snapforge-bench --out benchmarks/results

# Transfer a snapshot between local stores (re-running is an idempotent no-op).
cargo run --release -p snapforge-cli -- --store /tmp/sf-store transfer ckpt-1 /tmp/sf-store-b
cargo run --release -p snapforge-cli -- --store /tmp/sf-store-b verify ckpt-1 --require-chunks

# Incremental: capture a child linked to a parent, then inspect/verify the chain.
cargo run --release -p snapforge-cli -- --store /tmp/sf-store --chunk-size 65536 \
    store /path/to/baseline --id base
cargo run --release -p snapforge-cli -- --store /tmp/sf-store --chunk-size 65536 \
    store /path/to/changed --id child --parent base
cargo run --release -p snapforge-cli -- --store /tmp/sf-store inspect child --chain
cargo run --release -p snapforge-cli -- --store /tmp/sf-store verify child --chain --require-chunks

# Extract (restore files from) a stored snapshot back to a directory.
cargo run --release -p snapforge-cli -- --store /tmp/sf-store extract child --output /tmp/sf-restored
cargo run --release -p snapforge-cli -- --store /tmp/sf-store extract child --output /tmp/sf-restored --json
```

Incremental snapshots are a **storage-layer** feature (content-addressed reuse
plus lineage), not a CRIU incremental-checkpoint format. See
[`docs/incremental.md`](docs/incremental.md) for the rationale and measured
results.

On a host that cannot run CRIU, `checkpoint`/`restore` fail with an explicit
capability diagnostic and produce no artifacts — SnapForge never fabricates
checkpoint data.

## Workspace layout

```
crates/
  snapforge-core      errors, config, logging, host probing, workload harness
  snapforge-engine    CRIU orchestration (checkpoint, restore, status, gating)
  snapforge-format    versioned manifest, validation, atomic publication
  snapforge-storage   streaming chunker, content-addressed store, pipeline, transfer, extract
  snapforge-cli       the `snapforge` binary
workloads/
  cpu-memory/         CPU + anonymous-memory workload
  file-heavy/         file-backed mapping / descriptor workload
  multithreaded/      multithreaded workload
docs/                 architecture, compatibility, snapshot format, incremental,
                      verification matrix, benchmark methodology, performance report
```

The `benchmarks/` directory contains the reproducible harness
(`run.py`), the result JSON schema (`schemas/result.schema.json`), and the
git-ignored `results/` output. See [`docs/benchmark-methodology.md`](docs/benchmark-methodology.md)
and [`docs/performance-report.md`](docs/performance-report.md).

## Workload harness

Each workload maintains a deterministic memory pattern, CPU counters, and
application state, writes a JSON state file, and can validate that state after a
restore. This is the readiness/validation protocol the benchmark suite relies on
to prove a restored process is genuinely the same workload.

Exit codes: `0` success, `2` argument error, `3` validation failure.

## Development

```bash
make build      # build the workspace
make test       # run unit tests
make lint       # clippy with -D warnings
make fmt        # format
make check      # fmt + lint + test
```

## Data location

By default SnapForge writes to `/tmp/snapforge` (overridable with `--store`).
On this development host the home filesystem is nearly full, so `/tmp` is used
for storage and benchmark data. Operators should point `--store` at a
sufficiently large, local filesystem for real workloads.

Because `/tmp` is RAM-backed (`tmpfs`) on many systems, the benchmark harness
defaults to a `/tmp` workdir but accepts `--workdir`; use a disk-backed path such
as `/var/tmp/snapforge-bench` when measuring storage throughput so results are
not memory-bandwidth-bound. Always record the medium alongside the numbers.

## Limitations (current)

- Real checkpoint/restore is unavailable on the development host because its
  kernel lacks `CONFIG_CHECKPOINT_RESTORE`. See `docs/compatibility.md`.
- `extract` reassembles stored **files** from content-addressed storage; it does
  not restore a running process. Process restore (`snapforge restore`) requires
  CRIU.
- Storage/transfer/verification features are host-independent and are developed
  and tested regardless of CRIU availability.
- SnapForge is **not** a sandbox or security boundary. Checkpoints may encode
  process state (including secrets) and must be treated as sensitive data.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
