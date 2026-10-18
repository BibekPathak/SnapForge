# SnapForge Architecture

## Scope

Single-host, Linux-first process checkpoint/restore. Container orchestration,
multi-node restore, and GPU checkpointing are explicitly out of scope for the
first version.

## Responsibility split

| Concern | Owner |
|---|---|
| Process-state capture/restore (threads, mappings, fds, sockets, namespaces) | **CRIU** |
| Workflow orchestration, artifact representation, storage, validation, measurement | **SnapForge** |

SnapForge does **not** attempt to reconstruct process state from raw memory
bytes. Saving a region of RAM is not sufficient to restore arbitrary threads,
mappings, file descriptors, sockets, namespaces, or external resources. CRIU is
the component that understands those semantics.

## Layering

```
                 +-----------------------------+
                 |        snapforge-cli        |  clap; user surface
                 +--------------+--------------+
                                |
        +-----------------------+-----------------------+
        |            +---------------------+            |
        |            |  snapforge-engine   |            |  orchestration
        |            +----------+----------+            |
        |                       |                       |
+-------+--------+   +----------+----------+   +--------+---------+
| snapforge-     |   | snapforge-storage   |   | snapforge-format |
| core           |   | chunker, CAS,       |   | manifest,
| config, host   |   | transfer, backpressure |  validation,
| probing,       |   +---------------------+   | atomic publish
| workload       |                             +------------------+
+----------------+
```

`snapforge-core` has no dependency on the other SnapForge crates, keeping the
lower layers reusable and independently testable. `snapforge-engine` depends on
`snapforge-core` for host probing and configuration.

## Phase 1 components

### `snapforge doctor` (host probing)

`snapforge-core::host` probes, in a read-only fashion:

- OS, kernel release, architecture, effective UID.
- `CONFIG_CHECKPOINT_RESTORE` from `/proc/config.gz` (or `/boot/config-*`).
- Presence of `/proc/sys/kernel/ns_last_pid`.
- `kernel.unprivileged_userns_clone`.
- CRIU binary location and `criu --version`.
- Effective capabilities (`CapEff`, checking `CAP_SYS_PTRACE`).

It produces a `HostReport` with an explicit `CriuReadiness` classification:

- `ready` — CRIU present and preconditions satisfied.
- `missing_binary` — no `criu` on `PATH`.
- `unsupported_kernel` — kernel lacks `CONFIG_CHECKPOINT_RESTORE` or `ns_last_pid`.
- `insufficient_privileges` — not root and no `CAP_SYS_PTRACE`.

Higher layers consult this to decide whether an operation can proceed. This is
the mechanism that prevents SnapForge from fabricating checkpoint results.

### Workload harness

`snapforge-core::workload` defines deterministic, verifiable state:

- A position-dependent anonymous-memory pattern with a BLAKE3 digest.
- A deterministic CPU checksum as a function of iteration count and thread index.
- A monotonic counter.

`snapforge-core::runner` ties parameters, execution, the readiness/state-file
protocol, and post-restore validation together. The three workload binaries
(`cpu-memory`, `file-heavy`, `multithreaded`) are thin wrappers.

Validation compares the *expected* state derived from parameters against the
state actually observed, catching both tampering/corruption (digest mismatch)
and wrong-parameter restores.

## Phase 2 components

### `snapforge-engine` (CRIU orchestration)

`snapforge-engine` owns the workflow around CRIU. It is deliberately explicit
about the responsibility boundary: CRIU captures and restores process state;
the engine decides *whether*, *with what arguments*, and *how to report*.

Modules:

- `criu` — discovers the `criu` binary (shell-free `PATH` search), parses
  `criu --version` (`CriuVersion`, minimum 3.15), and invokes CRIU through
  `Criu::run`. Every invocation uses an explicit argument array (never shell
  interpolation) and captures stdout, stderr, exit code, duration, and whether
  a timeout fired.
- `engine` — `Engine` validates prerequisites, runs `dump`/`restore`, stages
  artifacts, summarizes them, and cleans up after failure. `status_of(pid)`
  reads process lifecycle state from `/proc` without needing CRIU.
- `error` — `EngineError`, including the `is_capability_block()` predicate that
  distinguishes "this host cannot do it" from "the operation failed".

Key behaviours, all covered by tests:

1. **Capability gate.** `Engine::ensure_capable` consults the Phase 1
   `HostReport`. If `CriuReadiness != Ready`, the operation fails with
   `HostNotCapable` *before* invoking CRIU. No fabricated results.
2. **Never success on failure.** A non-zero CRIU exit becomes
   `EngineError::CriuFailed` (with captured stderr). Partial output is cleaned.
3. **Empty checkpoint is failure.** If CRIU reports success but produces zero
   artifact files, the engine raises `EmptyCheckpoint` and cleans up, so an
   empty directory is never published as a valid snapshot.
4. **Bounded timeouts.** The child CRIU process is placed in its own process
   group (`setpgid`); on timeout the whole group is killed with `killpg`, and
   stdout/stderr are drained on dedicated threads so an inherited pipe cannot
   block completion. `Timeout` is returned and output cleaned.
5. **Explicit arguments.** Flags such as `--shell-job`, `--leave-running`, and
   `--restore-detached` are built as discrete argv entries.
6. **Safe cleanup.** On failure, only files under the checkpoint directory are
   removed; the directory itself is removed only if it becomes empty.

Real capture/restore cannot run on the development host (see
`docs/compatibility.md`); the engine's control flow is therefore verified with
controllable fake CRIU binaries plus an explicit runtime capability gate.

## Phase 3 components

### `snapforge-format` (versioned manifest + atomic publication)

`snapforge-format` defines the immutable, versioned snapshot manifest and the
rules for validating and publishing it. It is storage-agnostic: the
content-addressed chunk identity it defines (`ChunkRef { id, length }`, where
`id` is a BLAKE3 hex digest) is what `snapforge-storage` implements in Phase 4.

Modules:

- `manifest` — the data model (`Manifest`, `Artifact`, `ChunkRef`,
  `Compatibility`, `SnapshotStats`), the `FORMAT_VERSION`, `Manifest::sealed`
  (compute the integrity digest), and helpers such as `chunk_lengths`.
- `validate` — `validate(&Manifest, &ValidateOptions) -> ValidManifest`,
  rejecting unsupported versions, path traversal/absolute paths, duplicate
  artifacts, bad chunk ids/lengths, incomplete maps, unknown chunks, missing or
  mismatched integrity, and self/cyclic parents. `detect_parent_cycle` walks a
  parent chain with a bounded iteration cap.
- `store` — manifest (de)serialization and **atomic publication**:
  stage → validate → refuse-overwrite → `rename(2)`.

The `ValidManifest` newtype is the only way to assert a manifest has passed
structural validation, which keeps untrusted-input handling explicit.

Publication is never partial: a snapshot is validated in a staging directory
and only then renamed into place; an existing snapshot id is never overwritten.
See `docs/snapshot-format.md` for the field-by-field schema, the informational
vs restore-critical distinction, and the full validation rule list.

## Phase 4 components

### `snapforge-storage` (content-addressed storage pipeline)

`snapforge-storage` implements the storage side of the snapshot model. It never
reads an entire snapshot into RAM and never exposes a partially written chunk as
valid.

Modules:

- `chunker` — a streaming fixed-size chunker over any `Read`. It holds at most
  one chunk-sized buffer at a time, so memory is bounded by the configured chunk
  size regardless of input length. Each chunk's id is the hex BLAKE3 digest of
  its bytes. Chunk sizes are configurable; 64 KiB, 256 KiB, 1 MiB and 4 MiB are
  benchmarked. SnapForge does not assume a best size.
- `cas` — the content-addressed store at `cas/<blake3-hex>`. Writes are atomic
  (unique temp file + `rename`) and idempotent: storing existing content is a
  no-op (deduplication). Reads recompute the digest, so corruption is an error,
  never a silent pass.
- `pipeline` — ingests a source into the CAS with **bounded concurrency** and
  backpressure.

### Bounded parallelism and backpressure

The pipeline is bounded at three points, which together cap memory:

1. A bounded `mpsc` channel (capacity = `concurrency`) between the blocking
   chunker and the storing stage; `blocking_send` applies backpressure to the
   chunker when the pipeline is saturated.
2. A `Semaphore(concurrency)` bounds how many chunk-store operations run at
   once.
3. Each in-flight unit owns exactly one chunk (`chunk_size` bytes).

Peak memory is therefore bounded by `concurrency * chunk_size` plus a small
constant. Chunks are stored on `spawn_blocking` threads so the async runtime is
not stalled. Chunk order is preserved so artifacts reassemble deterministically.

The pipeline records `IngestStats` including `chunks_written`,
`chunks_deduplicated`, `dedup_saved_bytes`, and `peak_in_flight`, which the
tests assert never exceeds the configured bound.

### Microbenchmarks

`cargo bench -p snapforge-storage` measures chunking throughput, hashing
throughput, and end-to-end pipeline ingest throughput across chunk sizes. On the
development host (Intel Core 5 120U, `powersave` governor, short measurement
window) representative figures are: chunking ~1.0–1.1 GiB/s, BLAKE3 hashing
~1.2–2.5 GiB/s, and end-to-end ingest (1 MiB chunks, concurrency 4) ~660 MiB/s.
These are indicative only; the full, repeated benchmark suite is Phase 7.

## Later phases (planned)
- **Transfer**: parallel, retrying, cancellable, integrity-verified local copy
  with atomic destination publication.
- **Incremental**: content-addressed reuse across compatible captures, with
  explicit parent dependencies; documented as a storage-layer experiment.
- **Benchmarks**: Python harness distinguishing capture, packaging, transfer,
  and restore costs, with raw JSON/CSV and Markdown reports.

## Concurrency and memory model (design intent)

- All parallel stages use bounded concurrency (`Semaphore`), never unbounded
  task spawning.
- Chunk processing is streaming: SnapForge never reads an entire snapshot into
  RAM.
- Backpressure is enforced by bounding both the number of in-flight tasks and
  the size of each buffer (chunk size).
