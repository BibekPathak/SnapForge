# SnapForge Snapshot Format

The snapshot format is a versioned, immutable manifest plus artifact data. The
manifest is stored **separately** from artifact bytes so that metadata can be
inspected and validated without reading (or trusting) the data.

## Layout

```
<store>/
├── cas/                     content-addressed chunk store (Phase 4)
│   └── <blake3-hex>         one file per distinct chunk
├── snapshots/
│   └── <snapshot-id>/
│       └── manifest.json    the manifest (this document)
└── staging/                 temporary in-progress publications
```

Chunks are addressed by the hex-encoded BLAKE3 hash of their bytes, which makes
identity intrinsic: identical bytes share an id, enabling deduplication and
integrity checks.

## Manifest schema (format version 1)

| Field | Type | Restore-critical? | Notes |
|---|---|---|---|
| `format_version` | u32 | **required** | Must equal `1`. |
| `snapshot_id` | string | **required** | `[A-Za-z0-9._-]{1,128}`, no `..`/separators. |
| `parent` | string? | for incremental | Parent snapshot id; must be acyclic. |
| `workload` | object | informational | Source workload id/description. |
| `captured_at` | string | informational | RFC 3339 timestamp. |
| `compatibility` | object | **required** | arch, kernel release, CRIU version. |
| `artifacts` | array | **required** | Ordered artifacts (see below). |
| `stats` | object | informational | Packaging statistics. |
| `integrity` | string? | **required** | BLAKE3 hex over the canonical body. |

### Artifact

| Field | Type | Notes |
|---|---|---|
| `path` | string | Relative, normalized; no absolute paths, no `..`. |
| `size` | u64 | Must equal the sum of chunk lengths. |
| `chunks` | array | Ordered `{ id, length }`; `id` is 64-char hex BLAKE3. |

### Informational vs required

- **Required for restore compatibility**: `format_version`, `snapshot_id`,
  `compatibility`, `artifacts` (paths, sizes, chunk maps), `integrity`.
- **Informational only**: `workload.description`, `captured_at`, `stats`. These
  are recorded for reproducibility and reporting but are not consulted for
  correctness.

## Integrity

`integrity` is a BLAKE3 digest over the manifest body with the `integrity`
field omitted, serialized as canonical JSON. It is computed by
`Manifest::integrity_digest` and set by `Manifest::sealed`. Verification
recomputes the digest and compares; any mismatch is rejected.

## Validation rules

A manifest is rejected when any of the following hold:

1. `format_version != 1`.
2. `snapshot_id` is empty, too long, or contains disallowed characters.
3. An artifact path is absolute, contains `.`/`..` components, or escapes the
   snapshot root (path traversal).
4. Two artifacts share the same `path`.
5. A chunk id is not a 64-character hex digest.
6. A chunk has zero length.
7. An artifact's chunk lengths do not sum exactly to its `size`
   (incomplete or over-long chunk map).
8. The same chunk id declares two different lengths (content addressing implies
   a unique length).
9. A referenced chunk is absent from the available chunk set (when the caller
   supplies one).
10. `integrity` is missing or does not match the recomputed digest.
11. `parent` equals `snapshot_id` (self-cycle), or a parent chain forms a cycle.

## Atomic publication

Publication never exposes a partial snapshot as valid:

1. Write the complete snapshot (manifest and, in later phases, data) into a
   **staging** directory.
2. Load and fully validate the staged manifest.
3. Refuse to overwrite an existing snapshot id.
4. Atomically `rename(2)` the staging directory to
   `snapshots/<snapshot-id>`.

A `rename` within the same filesystem is atomic. If publication fails at any
step, the destination is left untouched and the caller may
`cleanup_staging`.

## CLI

```bash
# Inspect a stored snapshot (by id or path).
snapforge --store /tmp/snapforge inspect fixture-1

# Verify integrity; optionally require all chunks to be present in the CAS.
snapforge --store /tmp/snapforge verify fixture-1 --require-chunks
```

## Producing a fixture (development)

`snapforge-format` ships an example that builds a valid published snapshot with
real, content-addressed chunk data. It performs **no** CRIU operation and is
labelled as a fixture:

```bash
cargo run -p snapforge-format --example make_fixture -- /tmp/snapforge
```
