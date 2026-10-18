# SnapForge

High-performance Linux checkpoint, snapshot, and restore engine.

SnapForge orchestrates **CRIU**-based process checkpoint/restore and adds a
versioned snapshot format, content-addressed chunk storage, a bounded parallel
transfer pipeline, and a reproducible benchmark harness.

> **Status: Phase 1 complete.** The workspace, `snapforge doctor`, and the
> deterministic workload harness are implemented, tested, and buildable.
> Snapshot format, storage, transfer, incremental, and benchmark phases follow.

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
```

## Workspace layout

```
crates/
  snapforge-core      errors, config, logging, host probing, workload harness
  snapforge-cli       the `snapforge` binary
workloads/
  cpu-memory/         CPU + anonymous-memory workload
  file-heavy/         file-backed mapping / descriptor workload
  multithreaded/      multithreaded workload
docs/                 architecture and compatibility notes
```

Additional crates (`snapforge-format`, `snapforge-engine`,
`snapforge-storage`) and the `benchmarks/` harness are added in later phases.

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

## Limitations (current)

- Real checkpoint/restore is unavailable on the development host because its
  kernel lacks `CONFIG_CHECKPOINT_RESTORE`. See `docs/compatibility.md`.
- Storage/transfer/verification features are host-independent and are developed
  and tested regardless of CRIU availability.
- SnapForge is **not** a sandbox or security boundary. Checkpoints may encode
  process state (including secrets) and must be treated as sensitive data.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
