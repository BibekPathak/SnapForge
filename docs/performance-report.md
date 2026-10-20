# SnapForge Performance Report

This report contains **measured** results from the current development host. It
is generated from the raw records in `benchmarks/results/` (git-ignored). The
methodology, parameters, and caveats are documented in
[`benchmark-methodology.md`](benchmark-methodology.md).

> The development host's kernel does **not** provide
> `CONFIG_CHECKPOINT_RESTORE`, so process checkpoint/restore cannot be measured
> here. The corresponding experiment is reported as **skipped**, with the exact
> reason. No checkpoint timings are estimated or fabricated.

## Environment

| Field | Value |
|---|---|
| OS | Arch Linux |
| Kernel | `7.1.9-hardened1-1-hardened` |
| Architecture | x86_64 |
| CPU | Intel(R) Core(TM) 5 120U (12 logical CPUs) |
| Memory | 15,964,820 KiB (~15.2 GiB) |
| Python | 3.12.7 |
| CPU governor | `powersave` |
| CRIU | not installed |
| `CONFIG_CHECKPOINT_RESTORE` | `false` |
| `/proc/sys/kernel/ns_last_pid` | absent |
| Privileges | unprivileged |
| Workdir medium | disk-backed NVMe (`/var/tmp`), not tmpfs |

Command used to produce the results:

```bash
python3 benchmarks/run.py \
  --sizes 64,256 \
  --chunk-sizes 65536,262144,1048576,4194304 \
  --concurrencies 1,4,8 \
  --transfer-concurrencies 1,4,8 \
  --transfer-mib 64 \
  --repetitions 3 \
  --workdir /var/tmp/snapforge-bench \
  --out benchmarks/results
```

Each cell is the **median of 3 repetitions**; throughput is
`bytes / wall-clock`. `—` means not applicable.

## Storage pipeline (`storage_only`)

This measures chunking, BLAKE3 hashing, content-addressed writes, and dedup. It
does **not** capture a process. Superseded by a `label` of `storage_only` in the
raw records.

### 64 MiB input

| chunk KiB | conc 1 (ms · MiB/s) | conc 4 (ms · MiB/s) | conc 8 (ms · MiB/s) |
|---|---|---|---|
| 64 | 157.09 · 407.4 | 80.66 · 793.4 | 81.32 · 787.0 |
| 256 | 103.66 · 617.4 | 77.55 · 825.2 | 78.50 · 815.3 |
| 1024 | 99.56 · 642.9 | 87.76 · 729.3 | 84.81 · 754.6 |
| 4096 | 112.02 · 571.3 | 106.79 · 599.3 | 111.32 · 574.9 |

### 256 MiB input

| chunk KiB | conc 1 (ms · MiB/s) | conc 4 (ms · MiB/s) | conc 8 (ms · MiB/s) |
|---|---|---|---|
| 64 | 785.43 · 325.9 | 455.35 · 562.2 | 535.39 · 478.2 |
| 256 | 784.86 · 326.2 | 374.35 · 683.9 | 352.66 · 725.9 |
| 1024 | 1370.35 · 186.8 | 373.94 · 684.6 | 405.63 · 631.1 |
| 4096 | 626.63 · 408.5 | 415.53 · 616.1 | 856.14 · 299.0 |

**Observations**

- Concurrency 1 → 4 is the dominant win: roughly **1.9×** on 64 MiB
  (e.g. 256 KiB: 617 → 825 MiB/s) and up to **3.7×** on 256 MiB at 1 MiB chunks
  (186.8 → 684.6 MiB/s, though the conc-1 baseline there is an outlier).
- Going 4 → 8 gives no consistent gain and sometimes regresses. With 12 logical
  CPUs but a `powersave` governor, the pipeline appears to saturate before 8
  workers; hashing is CPU-bound.
- 256 KiB is the best overall chunk size on this host (825 MiB/s at 64 MiB,
  726 MiB/s at 256 MiB). 4 MiB chunks reduce the number of parallel units and
  consistently underperform.
- Peak RSS tracked input size (~70 MiB for 64 MiB input, ~270–349 MiB for
  256 MiB input) because the `bench` path buffers the input; see the
  methodology's note. It is not a production memory claim.
- Several cells (notably 256 MiB / 1 MiB / conc 1 and 256 MiB / 4 MiB / conc 8)
  are visibly noisy. The host is not isolated and runs a power-saving governor;
  treat individual cells as indicative, and prefer the trend.

## Transfer (local filesystem)

64 MiB snapshot, four files, transferred from a populated store to a fresh one.

| Case | Concurrency | Duration ms (median) | Throughput MiB/s (median) |
|---|---|---|---|
| cold | 1 | 392.59 | 163.0 |
| cold | 4 | 292.23 | 219.0 |
| cold | 8 | 254.49 | 251.5 |
| warm (idempotent rerun) | 1 | 8.00 | — |
| warm (idempotent rerun) | 4 | 11.04 | — |
| warm (idempotent rerun) | 8 | 8.98 | — |

**Observations**

- Cold transfer scales with concurrency: **163 → 251 MiB/s** (1.54×) from 1 to 8
  workers.
- The warm (rerun) case is roughly **8–11 ms** and essentially flat across
  concurrency. That is the cost of scanning the destination, discovering every
  chunk already present, skipping all copies, and re-publishing — a cheap
  idempotent operation. Throughput is undefined (zero bytes moved), so it is
  shown as `—`.

## Incremental reuse

Baseline: 8 files × 8 MiB (64 MiB). Child: the same set with exactly one file
changed. Chunk size 256 KiB.

| chunk KiB | n | Reuse ratio (median) | Reused bytes | New bytes |
|---|---|---|---|---|
| 256 | 3 | **0.88** | 58,720,256 (56 MiB) | 8,388,608 (8 MiB) |

**Observations**

- The measured reuse ratio `0.875` is exactly `7/8`, matching the controlled
  change: all chunks of the seven unchanged files were already in the
  content-addressed store and were reused; only the changed file's chunks were
  written. This is a correctness result as much as a performance one: incremental
  capture avoids rewriting unchanged data.

## Process checkpoint/restore (CRIU)

| Experiment | Status | Reason |
|---|---|---|
| Direct CRIU checkpoint/restore | **skipped** | `CONFIG_CHECKPOINT_RESTORE=false`, `ns_last_pid` absent, `criu` not installed |

CRIU cannot run on this kernel at all (the feature is compiled out), and a
container would share this kernel, so containerizing would not help. SnapForge
therefore refuses checkpoint/restore here with an explicit capability error and
produces no artifacts. On a capable host (`CONFIG_CHECKPOINT_RESTORE=y`,
`ns_last_pid` present, CRIU ≥ 3.15 installed, and sufficient privileges) the same
harness records real `cpu-memory` / `file-heavy` / `multithreaded` runs in these
rows. Until then, this report makes **no claim** about checkpoint or restore
latency.

## Summary

- The storage pipeline is throughput-oriented: **~0.8 GiB/s** on 64 MiB inputs
  at 4 workers with 256 KiB chunks on this host, scaling to a best of
  **~726 MiB/s** on 256 MiB.
- **256 KiB chunks** are the best default on this host; **concurrency 4** is the
  knee (8 rarely helps).
- Local transfer parallelizes modestly (1.54× from 1 → 8 workers) and idempotent
  reruns are near-free (single-digit milliseconds).
- Incremental capture achieves the theoretically expected **87.5%** chunk reuse
  for a one-of-eight-file change.
- Checkpoint/restore performance is **not measurable on this host** and is
  explicitly excluded rather than estimated.

For parameters, environment capture, and noise sources, see
[`benchmark-methodology.md`](benchmark-methodology.md). Raw records and charts
live under `benchmarks/results/` when the harness is run.
