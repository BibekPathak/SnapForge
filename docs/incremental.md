# SnapForge Incremental Snapshots

## What SnapForge means by "incremental"

SnapForge implements **content-addressed reuse across compatible captures**. It
is a *storage-layer* feature, not a CRIU incremental-checkpoint format.

Because every chunk is addressed by the BLAKE3 hash of its bytes, any content
that repeats between two captures maps to the same chunk id and is stored only
once. Capturing a second snapshot after a controlled change therefore stores
only the bytes that actually changed. A child snapshot records its `parent` id,
which provides explicit lineage and lets SnapForge validate a whole chain before
a restore is attempted. Reuse statistics are measured and recorded.

## What SnapForge does **not** claim

- This is **not** a CRIU incremental image. CRIU produces a self-contained image
  set per dump and does not support merging a "parent" image set with a "child"
  image set to reconstruct arbitrary process state. SnapForge never diffs memory
  and never synthesizes a CRIU image.
- The `parent` link is **lineage, not a restore dependency**. Restoring a child
  does not "start from the parent"; each snapshot is a complete, self-describing
  manifest over the shared content-addressed store.
- The measured benefit is **storage/packaging** (fewer new bytes for a second
  compatible capture), not a faster CRIU restore.

## Workflow

```bash
# 1. Baseline capture.
snapforge --store /tmp/store --chunk-size 65536 store ./baseline --id base

# 2. Second capture after controlled changes, linked to the baseline.
snapforge --store /tmp/store --chunk-size 65536 store ./changed --id child --parent base

# The child reports measured reuse, e.g.:
#   incremental:   parent=base
#   reuse:         12/16 chunks, 786432 bytes reused, 75.0% by bytes

# 3. Inspect the chain, with per-link reuse and compaction analysis.
snapforge --store /tmp/store inspect child --chain

# 4. Verify the whole chain (structure, integrity, and CAS coverage).
snapforge --store /tmp/store verify child --chain --require-chunks
```

## Measured behaviour

The integration tests build a baseline of four 64 KiB blobs and a child that
changes exactly one blob (16 KiB chunks). Results:

| Quantity | Value |
|---|---|
| Child chunks | 16 |
| Reused chunks (present in parent) | 12 |
| New chunks | 4 |
| Reused bytes | 3 × 64 KiB = 196 608 |
| Reuse ratio (by bytes) | 0.75 |
| Bytes newly written to the CAS | ≤ one blob (only the changed content) |

An *identical* re-capture stores zero new bytes and reports a reuse ratio of
1.0. These figures come from `crates/snapforge-storage/tests/incremental.rs`.

## Chain validation

- `resolve_chain` walks `parent` pointers root-first with a bounded iteration
  cap, and rejects missing links and cycles.
- `verify_chain` verifies each distinct chunk in the chain against the CAS
  (content id recomputation), not just presence.
- A cycle is also rejected at capture time: adding a child whose id already
  appears in the parent's chain is refused.

## Compaction analysis (not applied automatically)

`analyze_compaction` reports the storage that *could* be reclaimed by flattening
a chain to its tip: the chunks reachable only through non-tip snapshots. SnapForge
does not perform compaction automatically. Flattening is only safe when the
non-tip snapshots are no longer needed independently, and the measured benefit
must justify the added complexity and the loss of lineage.

## Compatibility constraints for safe reuse

Content-addressed reuse is safe whenever two captures share byte-identical
artifacts/chunks. It says nothing about *semantic* compatibility between two
CRIU checkpoints. Two snapshots from different kernels, CRIU versions, or
architectures still carry distinct `compatibility` metadata; reuse of identical
chunks is a storage optimization only and does not merge or transform CRIU
images. Restore compatibility is validated separately from storage reuse.

## Fallback

If a safe incremental implementation were not defensible, SnapForge would ship
reliable full snapshots plus content-addressed storage. That is in fact what it
does: every snapshot is a complete manifest, and "incremental" is exactly the
storage deduplication that falls out of content addressing, plus lineage
tracking and reuse measurement. No correctness depends on the parent link.
