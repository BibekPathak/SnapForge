//! Integration tests for incremental snapshots (content-addressed reuse).
//!
//! These build a baseline snapshot and a child that shares most content, then
//! assert that (a) only new chunks consume storage, (b) the computed reuse
//! report matches reality, and (c) the parent chain resolves and verifies.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use snapforge_format::{
    publish, Compatibility, Manifest, SnapshotStats, ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_storage::{analyze_compaction, resolve_chain, store_reader, Cas, PipelineOptions};

/// Ingest a set of named blobs into a store's CAS and publish a sealed manifest.
fn build_snapshot(
    store: &Path,
    id: &str,
    parent: Option<&str>,
    blobs: &[(&str, Vec<u8>)],
    chunk_size: usize,
) -> (Manifest, u64) {
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let snapshots = store.join("snapshots");
    let staging = store.join("staging").join(id);
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::create_dir_all(&staging).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let new_bytes_before = count_bytes(&store.join("cas"));

    let mut artifacts = Vec::new();
    let mut stats = SnapshotStats::default();
    let mut distinct = std::collections::BTreeSet::new();
    for (name, bytes) in blobs {
        let ingested = rt
            .block_on(store_reader(
                cas.clone(),
                Cursor::new(bytes.clone()),
                PipelineOptions {
                    chunk_size,
                    concurrency: 4,
                    verify_on_write: false,
                },
            ))
            .unwrap();
        stats.total_bytes += ingested.size;
        stats.total_chunk_refs += ingested.chunks.len() as u64;
        stats.dedup_saved_bytes += ingested.stats.dedup_saved_bytes;
        for c in &ingested.chunks {
            distinct.insert(c.id.clone());
        }
        artifacts.push(ingested.into_artifact(name.to_string()));
    }
    stats.distinct_chunks = distinct.len() as u64;

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        snapshot_id: id.into(),
        parent: parent.map(String::from),
        workload: WorkloadInfo {
            id: "inc".into(),
            description: None,
        },
        captured_at: "2026-01-01T00:00:00Z".into(),
        compatibility: Compatibility {
            arch: "x86_64".into(),
            kernel_release: "test".into(),
            criu_version: None,
            criu_image_version: None,
        },
        artifacts,
        stats,
        integrity: None,
    }
    .sealed()
    .unwrap();

    snapforge_format::write_to_dir(&staging, &manifest).unwrap();
    publish(&staging, &snapshots, &ValidateOptions::default()).unwrap();

    let new_bytes_after = count_bytes(&store.join("cas"));
    (manifest, new_bytes_after - new_bytes_before)
}

fn count_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn blob(seed: u8, len: usize) -> Vec<u8> {
    // Pseudo-random-looking but deterministic, so distinct blobs produce
    // distinct chunk ids and each blob splits into distinct chunks.
    let mut out = Vec::with_capacity(len);
    let mut x = 0x1234_5678u64 ^ (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    out
}

#[test]
fn child_store_only_writes_new_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();

    // Baseline: four distinct 64 KiB blobs.
    let b1 = blob(1, 65536);
    let b2 = blob(2, 65536);
    let b3 = blob(3, 65536);
    let b4 = blob(4, 65536);
    let (base, _) = build_snapshot(
        &store,
        "base",
        None,
        &[
            ("f1", b1.clone()),
            ("f2", b2.clone()),
            ("f3", b3.clone()),
            ("f4", b4.clone()),
        ],
        16384,
    );

    // Child: three blobs identical, one changed.
    let b4_new = blob(99, 65536);
    let (child, new_bytes) = build_snapshot(
        &store,
        "child",
        Some("base"),
        &[("f1", b1), ("f2", b2), ("f3", b3), ("f4", b4_new)],
        16384,
    );

    // Only the changed blob's chunks should be newly written.
    assert!(
        new_bytes <= 65536,
        "expected <= one blob of new bytes, wrote {new_bytes}"
    );

    let report = snapforge_storage::ReuseReport::compute(&child, &base);
    assert_eq!(report.child_chunks, 16);
    assert_eq!(report.new_chunks, 4);
    assert_eq!(report.reused_chunks, 12);
    assert_eq!(report.reused_bytes, 3 * 65536);
    assert!((report.reuse_ratio - 0.75).abs() < 1e-9);
}

#[test]
fn identical_snapshot_has_full_reuse() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();

    let b = blob(7, 131072);
    let (base, _) = build_snapshot(&store, "a", None, &[("x", b.clone())], 16384);
    let (child, new_bytes) = build_snapshot(&store, "b", Some("a"), &[("x", b)], 16384);

    assert_eq!(new_bytes, 0, "identical content must reuse every chunk");
    let report = snapforge_storage::ReuseReport::compute(&child, &base);
    assert_eq!(report.new_chunks, 0);
    assert_eq!(report.reused_chunks, report.child_chunks);
    assert!((report.reuse_ratio - 1.0).abs() < 1e-9);
}

#[test]
fn chain_resolves_and_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();

    let b1 = blob(1, 32768);
    let b2 = blob(2, 32768);
    build_snapshot(&store, "base", None, &[("x", b1.clone())], 8192);
    build_snapshot(
        &store,
        "mid",
        Some("base"),
        &[("x", b1.clone()), ("y", b2.clone())],
        8192,
    );
    build_snapshot(
        &store,
        "tip",
        Some("mid"),
        &[("x", b1.clone()), ("y", b2.clone())],
        8192,
    );

    let snapshots = store.join("snapshots");
    let chain =
        resolve_chain("tip", |id| snapforge_format::load(&snapshots.join(id)).ok()).unwrap();
    assert_eq!(chain.ids, vec!["base", "mid", "tip"]);
    assert_eq!(chain.depth(), 3);

    // Verify every chunk in the chain against the CAS.
    let cas = Cas::open(store.join("cas")).unwrap();
    let manifests: Vec<Manifest> = chain
        .ids
        .iter()
        .map(|id| snapforge_format::load(&snapshots.join(id)).unwrap())
        .collect();
    let verified = snapforge_storage::verify_chain(&cas, &manifests).unwrap();
    assert!(
        verified >= 4,
        "expected at least 4 distinct chunks, got {verified}"
    );

    // Compaction analysis: the tip alone references a subset of chain chunks.
    let analysis = analyze_compaction(&manifests);
    assert_eq!(analysis.snapshots, 3);
    assert!(analysis.chain_chunks >= analysis.tip_chunks);
}

#[test]
fn chain_detects_missing_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let b = blob(1, 8192);
    build_snapshot(&store, "child", Some("ghost"), &[("x", b)], 4096);

    let snapshots = store.join("snapshots");
    let err = resolve_chain("child", |id| {
        snapforge_format::load(&snapshots.join(id)).ok()
    })
    .unwrap_err();
    assert!(err.to_string().contains("missing"));
}
