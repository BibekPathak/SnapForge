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
lower layers reusable and independently testable.

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

## Later phases (planned)

- **Format**: immutable versioned manifest, strict validation, atomic publish.
- **Storage**: streaming chunker, BLAKE3 content-addressing, dedup, bounded
  parallel writes with backpressure and a documented memory budget.
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
