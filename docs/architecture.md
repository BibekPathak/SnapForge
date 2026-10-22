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

## Phase 5 components

### `snapforge-storage::transfer` (local snapshot transfer)

Transfers copy a snapshot's manifest and all referenced chunks from one store to
another (local filesystem). The module guarantees, and its tests assert:

1. **Bounded concurrency.** A `Semaphore` caps in-flight chunk copies at
   `concurrency`; each worker copies exactly one chunk at a time.
2. **Integrity.** Source chunks are read through the CAS (which verifies their
   content id), the destination copy is re-verified, and a length check catches
   truncated reads.
3. **Idempotent retries.** Chunks already present and valid at the destination
   are skipped, so a re-run resumes cheaply. A completed re-run of the same
   snapshot is a no-op that reports `chunks_skipped == chunks_total`.
4. **Retries with backoff.** Transient per-chunk failures are retried up to
   `max_retries` with exponential backoff.
5. **Cancellation.** A `CancellationToken` (shared atomically) is checked before
   start, between chunks, and inside the retry loop; the first error cancels
   remaining work.
6. **Atomic destination publication.** All chunks are copied first, then the
   staging manifest is validated against the destination CAS, then the snapshot
   directory is `rename`d into place. A conflicting destination snapshot is
   refused (never silently replaced).
7. **Bounded memory.** Each worker holds at most one chunk.

The local implementation is fully testable without any cloud account. Phase 10
adds an HTTP-backed adapter with the same guarantees. Larger end-to-end transfer
benchmarks (cold/warm cache, chunk size, concurrency) are Phase 7.

## Phase 6 components

### `snapforge-storage::incremental` (content-addressed reuse + lineage)

Incremental snapshots are a **storage-layer** feature: because chunks are
content-addressed, repeated bytes between two captures are stored once. A child
snapshot records its `parent` id for explicit lineage, and reuse is measured.

- `ReuseReport::compute(child, parent)` quantifies reused vs new chunks and
  bytes, and the reuse ratio by bytes.
- `resolve_chain(tip, resolve)` walks parent pointers root-first with a bounded
  cap, rejecting missing links and cycles.
- `verify_chain(cas, manifests)` verifies every distinct chunk across the chain
  against the CAS.
- `analyze_compaction(chain)` reports the chunks/bytes reachable only through
  non-tip snapshots. Phase 9 adds an explicit `compact` command that applies it.

The `parent` link is lineage, **not** a restore dependency: each snapshot is a
complete manifest over the shared CAS. SnapForge does not diff memory or
synthesize CRIU images. The measured benefit is storage/packaging, not restore
speed. See `docs/incremental.md` for the full rationale, measured figures, and
the explicit list of non-claims.

The CLI records reuse in `SnapshotStats` (informational fields), reports it on
`store --parent`, prints per-link reuse on `inspect --chain`, and validates the
whole chain on `verify --chain --require-chunks`. Capture-time cycle detection
rejects a child whose id already appears in the parent's chain, and a missing
parent fails before any work.

## Phase 8 components

### `snapforge-storage::extract` (snapshot extraction / file restore)

Extraction is the inverse of the storage pipeline. Given a published snapshot,
it reassembles each artifact at its declared relative path under an output
directory. It restores **files**, not a running process — process restore
remains CRIU's `restore` path (Phase 2). Guarantees:

1. **Integrity.** Every chunk is read through the CAS, which recomputes its
   BLAKE3 digest; a mismatch is an error, never a silent pass. Each
   reconstructed artifact's length is checked against the manifest. Chunk
   availability is validated before any file is written.
2. **Bounded concurrency and memory.** At most `concurrency` artifacts are
   reconstructed at once, and each worker holds at most one chunk buffer.
3. **Atomic per-file publication.** Each artifact is written to a temporary
   file in its destination directory and `rename`d into place, so a partially
   written artifact is never observable at its final path.
4. **Safe destinations.** Artifact paths are re-validated as relative,
   normalized paths, so extraction cannot escape the output directory. A
   pre-existing non-empty destination is refused unless overwrite is enabled.
5. **Cancellation.** A shared [`CancellationToken`] stops extraction promptly.

The CLI exposes this as `snapforge extract <snapshot> --output <dir> [--force]`,
printing a summary (or JSON with `--json`). Because it is a storage-layer
operation it works regardless of CRIU availability.

## Phase 9 components

### `snapforge-storage::gc` (garbage collection + chain compaction)

Nothing removes chunks when a snapshot directory is deleted or a chain is
flattened, so the CAS accumulates unreferenced data. Phase 9 adds two explicit,
opt-in maintenance operations. Both are **non-destructive by default** and
refuse to run if any published manifest cannot be loaded and validated.

1. **`gc(cas, snapshots_dir, opts)`** — mark-and-sweep over the whole store.
   It loads and validates every published snapshot, unions their referenced
   chunk ids (the live set), and removes CAS files outside that set. It also
   removes leftover `.tmp-*` files from interrupted atomic writes. With
   `dry_run` it reports counts and bytes without deleting. Deletions are
   bounded by `concurrency`; a manifest load/validation failure aborts before
   anything is removed.
2. **`compact_chain(cas, snapshots_dir, tip, opts)`** — flattens a chain to its
   tip. With `apply` it (a) rewrites the tip's manifest with `parent = None`
   and a recomputed integrity digest via temp file + `rename`, (b) deletes the
   ancestor snapshot directories, then (c) runs the sweep. It refuses if any
   snapshot **outside** the chain still depends on an ancestor being removed,
   since that would break its lineage. Compacting an already-root snapshot is a
   no-op. Dry-run reports the ancestors and the chunks/bytes that would be
   reclaimed without touching the store.

Both operations report `duration_ms` and machine-readable JSON. The CLI exposes
them as `snapforge gc [--apply]` and `snapforge compact <tip> [--apply]`, so a
destructive operation requires an explicit flag.

## Phase 10 components

### `snapforge-remote` (HTTP transfer adapter)

The remote transport reuses the local transfer model over a small HTTP/1.1
protocol. `snapforge serve` exposes a local store; `snapforge transfer <id>
http://host:port` pushes a snapshot to it. It runs over plain HTTP by default
(ideal behind a TLS-terminating reverse proxy) or in-process TLS; see Phase 13.
Set a bearer token (`--token`, `--auth`, or `SNAPFORGE_REMOTE_TOKEN`) when peers
are not fully trusted.

Protocol (all under `/v1`):

| Method | Path | Semantics |
|---|---|---|
| `GET` | `/health` | Liveness, protocol id, version (never authenticated) |
| `HEAD` | `/chunks/{id}` | Does the remote already hold this chunk? |
| `GET` | `/chunks/{id}` | Fetch a chunk (integrity-checked on read) |
| `PUT` | `/chunks/{id}` | Store a chunk; the **server** recomputes BLAKE3 and rejects a mismatch |
| `GET` | `/snapshots/{id}` | Fetch a published manifest |
| `PUT` | `/snapshots/{id}` | Publish a manifest after checking every referenced chunk exists |

Client guarantees mirror local transfer: bounded concurrency, source chunks
read through the CAS (integrity verified), idempotent skipping via `HEAD`,
bounded retries with backoff on network/5xx/429 errors, cancellation, and the
manifest sent only after all chunks are accepted. A remote that already holds
the same snapshot id yields a no-op; a *different* snapshot with the same id is
refused with a conflict. `--verify-remote` additionally fetches and hashes
chunks the remote claims to have, and repairs a corrupt remote chunk by
re-uploading it (the server verifies an existing file before deduplicating).

Server guarantees: every `PUT /chunks` body is hashed and must match its URL id;
manifest publication validates structure, integrity, and chunk availability and
is atomic (`staging` + `rename`); an existing identical snapshot is an idempotent
success, while a different or invalid one is never overwritten. Request bodies
are size-limited (`--max-body-mib`).

## Phase 11 components

### `snapforge-storage::list` (snapshot inventory)

`list_snapshots(snapshots_dir)` is a read-only inventory: it answers "what is in
this store?" without mutating anything. Each published snapshot is reported with
its path, parent, chain depth, logical bytes, distinct chunks, and the storage
that flattening its chain to the tip would reclaim. Store-wide totals cover
logical bytes, physical CAS usage, and unreferenced chunks (the `gc` target).

Design notes:

- **Diagnostic, not strict.** Unlike `gc`, which aborts on any unreadable
  manifest, `list` reports a corrupt snapshot with `valid = false` and an error
  string and still inventories the rest of the store. An operator should see
  broken state, not an inventory that silently omits it.
- **Broken chains stop, not fail.** A missing parent or a cycle terminates the
  walk; the snapshot is still listed with the chain it did resolve.
- **Chain-tip accounting.** Every member of a chain reports the same reclaimable
  figures as its tip, so `list` and `compact` agree on what a flatten frees.

The CLI exposes this as `snapforge list` (human table or `--json`), replacing the
last `not_implemented` stub.

## Phase 12 components

### Remote pull with ranged resume

`snapforge-remote` gains the mirror of push: `pull_snapshot(endpoint, id, ...)`
fetches a remote manifest and every referenced chunk into a local store. The CLI
exposes it as `snapforge pull <snapshot> <endpoint>`.

Client guarantees mirror the push path: bounded concurrency, integrity checks
before any chunk is written, idempotent skipping of chunks the local store
already holds and verifies, bounded retries with backoff, cancellation, and
atomic publication (`staging` + `rename`) only after every chunk is present.

**Ranged resume.** An interrupted chunk download may leave a `.part-<id>`
scratch file. On the next pull, the client resumes from the bytes it already
has, requesting only the missing tail with `Range: bytes=<start>-`. The server
answers `206 Partial Content` with `Content-Range`; a malformed, multi-part, or
unsatisfiable range is deliberately ignored (the server sends the whole body) so
a hostile or buggy client can never widen a range. Resumed chunks are counted in
`chunks_resumed`. A partial file is deleted once the chunk verifies, so stale
scratch state never accumulates.

Both directions now share one code path for retries, backoff, cancellation, and
status classification (`snapforge-remote::http`); the direction-specific
`PushOptions` / `PullOptions` embed a common `RemoteSession` (retries, timeout,
token, TLS trust, cancellation). `RemoteOptions` / `RemoteOutcome` remain as
aliases of the push types from Phase 10.

## Phase 13 components

### In-process TLS and scoped authorization

The transport no longer requires a TLS-terminating reverse proxy, and no longer
relies on a single all-or-nothing token.

**TLS.** TLS is terminated **in-process** with `rustls`, using the `ring`
crypto provider so no external `openssl`/`cmake` toolchain is needed.

- The server ([`TlsServerConfig`]) loads a PEM certificate chain and private key
  (`--tls-cert` / `--tls-key`) and serves the Axum router over a `tokio-rustls`
  accept loop. A failed handshake is logged and dropped, never fatal.
- The client ([`TlsClientConfig`]) trusts the system roots by default and accepts
  an extra PEM CA bundle (`--ca-cert`) for a private CA or self-signed cert.
  `accept_invalid_certs` exists for tests only.
- Plain HTTP remains the default; the scheme in the endpoint (`http://` vs
  `https://`) selects the behavior, and both are accepted by the client.

**Scoped tokens.** [`TokenSet`] replaces the single shared token:

- [`Scope::Read`] permits `GET`/`HEAD` (fetching chunks and manifests).
- [`Scope::Write`] permits `PUT` (uploading chunks, publishing manifests), and
  implies read.
- Tokens are compared in constant time against every configured token, so the
  response does not leak which token matched.
- An empty set disables authorization (trusted network / authenticating proxy).
- The CLI accepts `--token` (single full-access) or `--auth token:read,token:write`
  (`SNAPFORGE_REMOTE_AUTH`), and `serve` reports the scheme, TLS mode, and auth
  state on startup.

## Later phases (planned)

- **mTLS and token management**: mutual TLS and per-token rotation/expiry rather
  than static configured secrets.

## Concurrency and memory model (design intent)

- All parallel stages use bounded concurrency (`Semaphore`), never unbounded
  task spawning.
- Chunk processing is streaming: SnapForge never reads an entire snapshot into
  RAM.
- Backpressure is enforced by bounding both the number of in-flight tasks and
  the size of each buffer (chunk size).
