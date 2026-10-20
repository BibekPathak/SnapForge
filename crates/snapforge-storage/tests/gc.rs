//! Integration tests for storage garbage collection and chain compaction.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use snapforge_format::{
    publish, Compatibility, Manifest, SnapshotStats, ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_storage::{
    compact_chain, gc, store_reader, Cas, CompactOptions, GcOptions, PipelineOptions,
};

fn blob(seed: u8, len: usize) -> Vec<u8> {
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

fn build_snapshot(
    store: &Path,
    id: &str,
    parent: Option<&str>,
    blobs: &[(&str, Vec<u8>)],
    chunk_size: usize,
) -> Manifest {
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let snapshots = store.join("snapshots");
    let staging = store.join("staging").join(id);
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::create_dir_all(&staging).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
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
            id: "gc".into(),
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
    manifest
}

fn write_orphan(cas_dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(cas_dir.join(name), bytes).unwrap();
}

fn cas_file_count(cas_dir: &Path) -> usize {
    std::fs::read_dir(cas_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .count()
}

fn manifest_of(store: &Path, id: &str) -> Manifest {
    snapforge_format::load(&store.join("snapshots").join(id)).unwrap()
}

fn gc_opts(dry_run: bool) -> GcOptions {
    GcOptions {
        dry_run,
        concurrency: 4,
        ..GcOptions::default()
    }
}

#[test]
fn dry_run_reports_orphans_without_deleting() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    build_snapshot(&store, "snap", None, &[("a", blob(1, 8192))], 4096);
    let cas_dir = store.join("cas");
    let orphan = "f".repeat(64);
    write_orphan(&cas_dir, &orphan, b"orphan");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(&cas_dir).unwrap());
    let out = rt
        .block_on(gc(cas, &store.join("snapshots"), gc_opts(true)))
        .unwrap();

    assert!(out.dry_run);
    assert_eq!(out.snapshots, 1);
    assert_eq!(out.unreferenced_chunks, 1);
    assert_eq!(out.unreferenced_bytes, 6);
    assert_eq!(out.removed_chunks, 0);
    assert!(cas_dir.join(&orphan).exists());
}

#[test]
fn apply_removes_orphans_but_keeps_live_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let manifest = build_snapshot(&store, "snap", None, &[("a", blob(1, 8192))], 4096);
    let cas_dir = store.join("cas");
    let live_ids: Vec<String> = manifest.chunk_lengths().into_keys().collect();
    write_orphan(&cas_dir, &"f".repeat(64), b"orphan");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(&cas_dir).unwrap());
    let out = rt
        .block_on(gc(cas, &store.join("snapshots"), gc_opts(false)))
        .unwrap();

    assert_eq!(out.removed_chunks, 1);
    assert_eq!(out.removed_bytes, 6);
    assert!(!cas_dir.join("f".repeat(64)).exists());
    for id in live_ids {
        assert!(cas_dir.join(id).exists(), "live chunk was removed");
    }
}

#[test]
fn invalid_manifest_aborts_before_deleting_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    build_snapshot(&store, "snap", None, &[("a", blob(1, 8192))], 4096);
    let cas_dir = store.join("cas");
    write_orphan(&cas_dir, &"f".repeat(64), b"orphan");

    std::fs::write(
        store.join("snapshots").join("snap").join("manifest.json"),
        b"{not json",
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(&cas_dir).unwrap());
    let err = rt
        .block_on(gc(cas, &store.join("snapshots"), gc_opts(false)))
        .unwrap_err();
    assert!(err.to_string().contains("refusing"), "{err}");
    assert!(cas_dir.join("f".repeat(64)).exists());
}

#[test]
fn apply_removes_leftover_temp_files() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    build_snapshot(&store, "snap", None, &[("a", blob(1, 4096))], 4096);
    let cas_dir = store.join("cas");
    write_orphan(&cas_dir, ".tmp-deadbeef-1234", b"partial");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(&cas_dir).unwrap());
    let out = rt
        .block_on(gc(cas, &store.join("snapshots"), gc_opts(false)))
        .unwrap();

    assert_eq!(out.removed_temp_files, 1);
    assert!(!cas_dir.join(".tmp-deadbeef-1234").exists());
}

#[test]
fn shared_chunks_survive_when_one_referrer_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let shared = blob(7, 8192);
    build_snapshot(&store, "one", None, &[("a", shared.clone())], 4096);
    build_snapshot(&store, "two", None, &[("a", shared)], 4096);

    // Remove one snapshot directory out-of-band, then sweep.
    std::fs::remove_dir_all(store.join("snapshots").join("one")).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let out = rt
        .block_on(gc(cas, &store.join("snapshots"), gc_opts(false)))
        .unwrap();

    assert_eq!(out.snapshots, 1);
    assert_eq!(out.removed_chunks, 0);
    assert_eq!(
        cas_file_count(&store.join("cas")) as u64,
        out.referenced_chunks
    );
}

#[test]
fn compact_flattens_chain_and_reclaims_ancestor_only_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let shared = blob(11, 8192);
    let extra = blob(12, 8192);
    let child_only = blob(13, 8192);
    build_snapshot(
        &store,
        "root",
        None,
        &[("shared", shared.clone()), ("extra", extra)],
        4096,
    );
    build_snapshot(
        &store,
        "child",
        Some("root"),
        &[("shared", shared), ("child", child_only)],
        4096,
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let out = rt
        .block_on(compact_chain(
            cas,
            &store.join("snapshots"),
            "child",
            CompactOptions {
                dry_run: false,
                ..CompactOptions::default()
            },
        ))
        .unwrap();

    assert_eq!(out.chain, vec!["root".to_string(), "child".to_string()]);
    assert_eq!(out.removed_snapshots, vec!["root".to_string()]);
    assert!(out.reclaimed_chunks > 0);
    assert!(!store.join("snapshots").join("root").exists());

    let child = manifest_of(&store, "child");
    assert!(child.parent.is_none());
    // Still a valid, sealed manifest.
    snapforge_format::load_validated(
        &store.join("snapshots").join("child"),
        &ValidateOptions::default(),
    )
    .unwrap();
}

#[test]
fn compact_dry_run_reports_but_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    build_snapshot(&store, "root", None, &[("a", blob(1, 8192))], 4096);
    build_snapshot(&store, "child", Some("root"), &[("b", blob(2, 8192))], 4096);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let out = rt
        .block_on(compact_chain(
            cas,
            &store.join("snapshots"),
            "child",
            CompactOptions::default(),
        ))
        .unwrap();

    assert!(out.dry_run);
    assert_eq!(out.removed_snapshots, vec!["root".to_string()]);
    assert!(out.reclaimed_chunks > 0);
    assert!(store.join("snapshots").join("root").exists());
    assert_eq!(manifest_of(&store, "child").parent.as_deref(), Some("root"));
}

#[test]
fn compact_refuses_when_ancestor_has_another_child() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    build_snapshot(&store, "root", None, &[("a", blob(1, 4096))], 4096);
    build_snapshot(&store, "child", Some("root"), &[("b", blob(2, 4096))], 4096);
    build_snapshot(&store, "other", Some("root"), &[("c", blob(3, 4096))], 4096);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let err = rt
        .block_on(compact_chain(
            cas,
            &store.join("snapshots"),
            "child",
            CompactOptions {
                dry_run: false,
                ..CompactOptions::default()
            },
        ))
        .unwrap_err();

    assert!(err.to_string().contains("still depends"), "{err}");
    assert!(store.join("snapshots").join("root").exists());
    assert!(store.join("snapshots").join("other").exists());
    assert_eq!(manifest_of(&store, "child").parent.as_deref(), Some("root"));
}

#[test]
fn compact_root_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    build_snapshot(&store, "root", None, &[("a", blob(1, 4096))], 4096);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let out = rt
        .block_on(compact_chain(
            cas,
            &store.join("snapshots"),
            "root",
            CompactOptions {
                dry_run: false,
                ..CompactOptions::default()
            },
        ))
        .unwrap();

    assert!(out.removed_snapshots.is_empty());
    assert_eq!(out.reclaimed_chunks, 0);
    assert!(store.join("snapshots").join("root").exists());
}

#[test]
fn compact_missing_snapshot_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(store.join("snapshots")).unwrap();
    std::fs::create_dir_all(store.join("cas")).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let err = rt
        .block_on(compact_chain(
            cas,
            &store.join("snapshots"),
            "nope",
            CompactOptions::default(),
        ))
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}
