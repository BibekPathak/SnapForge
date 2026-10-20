//! Integration tests for snapshot extraction.
//!
//! These verify the inverse of the storage pipeline: a snapshot built from a
//! set of artifacts extracts back to byte-identical files, with integrity and
//! destination-safety enforced.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use snapforge_format::{
    Artifact, Compatibility, Manifest, SnapshotStats, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_storage::{extract_snapshot, Cas, ExtractOptions, PipelineOptions, StorageError};

fn opts(chunk_size: usize, concurrency: usize) -> PipelineOptions {
    PipelineOptions {
        chunk_size,
        concurrency,
        verify_on_write: false,
    }
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// Ingest `files` into `cas`, build a sealed manifest, and publish it under
/// `<store_root>/snapshots/<id>`.
async fn build_snapshot(
    store_root: &Path,
    cas: &Cas,
    id: &str,
    files: &[(&str, Vec<u8>)],
    chunk_size: usize,
) -> Manifest {
    let mut artifacts: Vec<Artifact> = Vec::new();
    for (name, bytes) in files {
        let ingested = snapforge_storage::store_reader(
            Arc::new(cas.clone()),
            Cursor::new(bytes.clone()),
            opts(chunk_size, 4),
        )
        .await
        .unwrap();
        artifacts.push(ingested.into_artifact(*name));
    }

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        snapshot_id: id.into(),
        parent: None,
        workload: WorkloadInfo {
            id: "extract-test".into(),
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
        stats: SnapshotStats::default(),
        integrity: None,
    }
    .sealed()
    .unwrap();

    let snap_dir = store_root.join("snapshots").join(id);
    snapforge_format::write_to_dir(&snap_dir, &manifest).unwrap();
    manifest
}

#[tokio::test]
async fn extract_round_trips_files() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    let files = vec![
        ("core-1.img", data(70_000, 3)),
        ("mm-1.img", data(40_000, 9)),
    ];
    build_snapshot(&store, &cas, "snap-1", &files, 4096).await;

    let out = tmp.path().join("restore");
    let outcome = extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("snap-1"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.snapshot_id, "snap-1");
    assert_eq!(outcome.artifacts, 2);
    assert_eq!(outcome.bytes_written, 110_000);
    for (name, bytes) in &files {
        assert_eq!(&std::fs::read(out.join(name)).unwrap(), bytes, "{name}");
    }
}

#[tokio::test]
async fn extract_creates_missing_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    build_snapshot(&store, &cas, "s", &[("a.bin", data(1000, 1))], 4096).await;

    let out = tmp.path().join("does/not/exist/yet");
    assert!(!out.exists());
    extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), data(1000, 1));
}

#[tokio::test]
async fn extract_creates_nested_artifact_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    let nested = data(20_000, 7);
    build_snapshot(
        &store,
        &cas,
        "s",
        &[("sub/dir/deep.img", nested.clone())],
        4096,
    )
    .await;

    let out = tmp.path().join("restore");
    extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(out.join("sub/dir/deep.img")).unwrap(), nested);
}

#[tokio::test]
async fn extract_deduplicates_and_reassembles_repeated_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    // Two identical files: identical chunks are stored once and reused.
    let blob = data(30_000, 4);
    build_snapshot(
        &store,
        &cas,
        "s",
        &[("a.bin", blob.clone()), ("b.bin", blob.clone())],
        4096,
    )
    .await;

    let out = tmp.path().join("restore");
    let outcome = extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), blob);
    assert_eq!(std::fs::read(out.join("b.bin")).unwrap(), blob);
    // Distinct chunks are fewer than total reads for a duplicated artifact.
    assert!(outcome.distinct_chunks < outcome.chunks_read);
}

#[tokio::test]
async fn extract_refuses_non_empty_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    build_snapshot(&store, &cas, "s", &[("a.bin", data(1000, 1))], 4096).await;

    let out = tmp.path().join("restore");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("keep-me"), b"do not clobber").unwrap();

    let err = extract_snapshot(
        Arc::new(cas.clone()),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::Extract(_)), "{err:?}");
    // The existing file is untouched.
    assert_eq!(
        std::fs::read(out.join("keep-me")).unwrap(),
        b"do not clobber"
    );

    // With overwrite enabled, extraction proceeds.
    extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions {
            overwrite: true,
            ..ExtractOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), data(1000, 1));
}

#[tokio::test]
async fn extract_detects_missing_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    let manifest = build_snapshot(&store, &cas, "s", &[("a.bin", data(9000, 2))], 4096).await;

    // Remove one referenced chunk from the CAS.
    let victim = manifest.artifacts[0].chunks[0].id.clone();
    std::fs::remove_file(cas.chunk_path(&victim)).unwrap();

    let out = tmp.path().join("restore");
    let err = extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap_err();
    // Chunk availability is validated up front, so this is a format error
    // wrapping the missing chunk; either way it must fail, not fabricate bytes.
    let msg = err.to_string();
    assert!(
        msg.contains("missing") || msg.contains(&victim) || msg.contains("chunk"),
        "{msg}"
    );
    // No artifact file should have been published.
    assert!(!out.join("a.bin").exists());
}

#[tokio::test]
async fn extract_detects_corrupt_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    let manifest = build_snapshot(&store, &cas, "s", &[("a.bin", data(9000, 2))], 4096).await;

    // Corrupt a referenced chunk on disk (same name, different content).
    let victim = manifest.artifacts[0].chunks[0].id.clone();
    std::fs::write(cas.chunk_path(&victim), b"tampered").unwrap();

    let out = tmp.path().join("restore");
    let err = extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &out,
        ExtractOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::CorruptChunk { .. }), "{err:?}");
    assert!(!out.join("a.bin").exists());
}

#[tokio::test]
async fn extract_rejects_zero_concurrency() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let cas = Cas::open(store.join("cas")).unwrap();
    build_snapshot(&store, &cas, "s", &[("a.bin", data(100, 1))], 4096).await;

    let err = extract_snapshot(
        Arc::new(cas),
        &store.join("snapshots").join("s"),
        &tmp.path().join("out"),
        ExtractOptions {
            concurrency: 0,
            ..ExtractOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::Config(_)));
}
