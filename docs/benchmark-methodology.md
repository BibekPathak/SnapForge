# Benchmark Methodology

This document describes how SnapForge benchmarks are produced, what they do and
do not measure, and how to reproduce the numbers in
[`performance-report.md`](performance-report.md).

The guiding rule is the same as the rest of the project: **never fabricate a
measurement**. If an experiment cannot run on a host (for example, because the
kernel lacks `CONFIG_CHECKPOINT_RESTORE`), it is recorded as `skipped` with the
reason, not estimated.

## Scope

SnapForge has two very different performance domains:

| Domain | What it covers | Availability |
|---|---|---|
| **Storage pipeline** | chunking, BLAKE3 hashing, content-addressed writes, dedup, transfer, incremental reuse | Always available; pure user-space |
| **Process checkpoint/restore** | CRIU capture and restore of a live process | Requires a CRIU-capable host |

Only the storage pipeline can be measured on the development host used for the
current report; the CRIU rows are recorded as skipped. The harness and report
structure are designed so that a capable host simply fills in the same tables.

## Harness

`benchmarks/run.py` is a standard-library-only Python driver. For each
measurement it shells out to the release `snapforge` binary, parses JSON, and
appends a record to `benchmarks/results/raw.jsonl` (one JSON object per line).
It then also writes:

- `raw.csv` — the same records flattened for spreadsheets,
- `summary.md` — median/spread tables,
- `plots/*.png` — charts when `matplotlib` is installed (optional).

The JSON record schema is [`schemas/result.schema.json`](../benchmarks/schemas/result.schema.json).
Each record captures the parameters, the environment, the metrics, and a
`status` (`ok` / `skipped` / `failed`).

### Environment capture

Every record embeds the environment so results remain interpretable later:
OS, kernel, architecture, CPU model and count, total memory, Python version,
CPU frequency governor, CRIU version, `CONFIG_CHECKPOINT_RESTORE`, presence of
`/proc/sys/kernel/ns_last_pid`, and privilege level. The environment block is
identical across records in a single harness invocation.

### Repetitions and aggregation

Timings are taken over several repetitions (the report uses three). The summary
reports the **median**; min/max are retained in the raw CSV. Medians are chosen
because a single scheduler hiccup should not dominate a result, and because the
development host runs a `powersave` governor that introduces run-to-run noise.

The harness never discards a run. Slow or failed runs remain in `raw.jsonl` and
are visible in `raw.csv`.

## Experiments

### 1. `storage_pipeline`

Ingests a synthetic, deterministic, incompressible-enough buffer of
`size_mib` MiB and measures the storage path end to end. Swept parameters:

- data size: 64 MiB, 256 MiB (1024 MiB optional),
- chunk size: 64 KiB, 256 KiB, 1 MiB, 4 MiB,
- pipeline concurrency: 1, 4, 8.

The `bench` subcommand reports stage timings (`chunking_time_ns`,
`storage_time_ns`, `total_time_ns`), throughput, chunk counts, dedup savings,
bounded in-flight count, and peak RSS (`VmHWM`). It is explicitly labeled
`storage_only` because **it does not capture or restore a process**.

> Note: the benchmark input is held in the harness process memory. Peak RSS
> therefore scales with the chosen data size and is not representative of a
> streaming production workload. It is reported for completeness, not as a
> memory-efficiency claim.

### 2. `transfer`

Stores a snapshot of a deterministic 64 MiB, four-file source into store A,
then transfers it to a fresh store B. Two cases per concurrency level:

- **cold** — empty destination; all chunks are copied. Measured end-to-end
  wall clock, divided by bytes transferred.
- **warm** — the same transfer repeated; all chunks already exist at the
  destination, so it should be an idempotent no-op. Reported separately because
  its "throughput" is not meaningful (zero bytes moved) and is shown as `—`.

Concurrency is swept at 1, 4, 8.

### 3. `incremental`

Stores a baseline snapshot of 8 files (8 MiB each, 64 MiB total), then stores a
child snapshot in which exactly one file changed, parented to the baseline. The
reported reuse ratio should be `7/8 = 0.875`: the unchanged files' chunks are
already present in the content-addressed store and are reused, while only the
changed file's chunks are new. This is a correctness-oriented benchmark as much
as a performance one.

### 4. `criu`

Real process checkpoint/restore. This experiment first checks the host. If
`CONFIG_CHECKPOINT_RESTORE`, `/proc/sys/kernel/ns_last_pid`, and `criu` are all
present, it would launch a workload and time `snapforge checkpoint` /
`snapforge restore`. Otherwise it records a single `skipped` record with the
exact missing prerequisites. **No placeholder numbers are produced.**

## Reproducing

```bash
# Build the binary the harness invokes.
CARGO_TARGET_DIR=/tmp/snapforge-target cargo build --release -p snapforge-cli

# Full sweep used for the report (disk-backed workdir recommended).
python3 benchmarks/run.py \
  --sizes 64,256 \
  --chunk-sizes 65536,262144,1048576,4194304 \
  --concurrencies 1,4,8 \
  --transfer-concurrencies 1,4,8 \
  --transfer-mib 64 \
  --repetitions 3 \
  --workdir /var/tmp/snapforge-bench \
  --out benchmarks/results

# Fast smoke test of the harness itself.
python3 benchmarks/run.py --quick --out /tmp/sf-bench-out
```

The harness runs the `snapforge bench` subcommand directly for the storage
sweep, which can also be used on its own:

```bash
snapforge --store /tmp/bench-store --chunk-size 262144 --concurrency 4 \
  --json bench --size-mib 256
```

`cargo bench -p snapforge-storage` runs the separate Criterion microbenchmarks
(chunker, hasher, pipeline) for lower-level, statistically-driven measurements.
Those are complementary to the end-to-end harness and are not merged into its
tables.

## Sources of noise and bias

These caveats apply to every number in the report:

- **Single host.** All results come from one machine and one kernel; they are
  not a cross-platform comparison.
- **Governor.** The CPU frequency governor is `powersave`, so absolute
  throughput is lower and noisier than on a `performance` governor.
- **Filesystem.** The report run uses a disk-backed workdir on NVMe. Where
  `/tmp` is `tmpfs` (RAM-backed), throughput is memory-bandwidth-bound and not
  comparable to disk numbers. Always record the workdir medium.
- **Page cache.** Repeated runs benefit from the OS page cache. The harness
  uses fresh store directories per repetition but does not drop caches (which
  requires root), so warm-cache effects are present.
- **No CPU pinning / isolation.** Other host activity can perturb short runs.
  Medians and the reported spread mitigate but do not eliminate this.
- **Synthetic data.** The storage sweep ingests pseudo-random data, which is
  near-worst-case for dedup and representative for hashing/writing but not for
  compressible real artifacts.
- **In-memory input.** The `bench` path buffers the input, as noted above.
- **`f64` throughput.** Throughput is computed from wall-clock duration and is
  therefore sensitive to timer granularity on very short runs.

## What the numbers support

The report is intended to answer: "does the storage pipeline scale with
concurrency, how does chunk size affect throughput, does transfer parallelize,
and does incremental capture actually avoid rewriting unchanged data?" It is
**not** a claim about checkpoint/restore latency, which requires a
CRIU-capable host and is reported as skipped here.
