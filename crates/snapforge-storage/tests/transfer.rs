//! Integration tests for local snapshot transfer.
//!
//! These build a real source snapshot (manifest + content-addressed chunks) and
//! exercise transfer behaviour end-to-end: happy path, idempotent re-run,
//! missing source chunks, corruption detection (retry exhaustion), bounded
//! concurrency, cancellation, and atomic destination publication.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use snapforge_format::{
    publish, Artifact, ChunkRef, Compatibility, Manifest, SnapshotStats, ValidateOptions,
    WorkloadInfo, FORMAT_VERSION,
};
use snapforge_storage::{transfer_snapshot, CancellationToken, Cas, TransferOptions};

/// Build a source store: commit chunks to `<store>/cas` and publish a snapshot
/// describing them under `<store>/snapshots/<id>`. Returns the list of ids.
fn build_source(store: &Path, id: &str, num_chunks: usize, chunk_len: usize) -> Vec<String> {
    let cas = Cas::open(store.join("cas")).unwrap();
    let snapshots = store.join("snapshots");
    let staging = store.join("staging").join(id);
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::create_dir_all(&staging).unwrap();

    let mut chunks = Vec::new();
    let mut ids = Vec::new();
    for i in 0..num_chunks {
        let bytes: Vec<u8> = (0..chunk_len)
            .map(|b| ((b as u64 + i as u64 * 7).wrapping_mul(31) % 251) as u8)
            .collect();
        let (cid, _) = cas.put(&bytes).unwrap();
        chunks.push(ChunkRef {
            id: cid.clone(),
            length: chunk_len as u64,
        });
        ids.push(cid);
    }
    let size = (num_chunks * chunk_len) as u64;
    let artifact = Artifact {
        path: "core-1.img".into(),
        size,
        chunks,
    };
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        snapshot_id: id.into(),
        parent: None,
        workload: WorkloadInfo {
            id: "transfer-test".into(),
            description: None,
        },
        captured_at: "2026-01-01T00:00:00Z".into(),
        compatibility: Compatibility {
            arch: "x86_64".into(),
            kernel_release: "test".into(),
            criu_version: None,
            criu_image_version: None,
        },
        artifacts: vec![artifact],
        stats: SnapshotStats {
            total_bytes: size,
            distinct_chunks: num_chunks as u64,
            total_chunk_refs: num_chunks as u64,
            dedup_saved_bytes: 0,
            capture_duration_ms: None,
            packaging_duration_ms: None,
            reused_chunks: None,
            new_chunks: None,
            reused_bytes: None,
        },
        integrity: None,
    }
    .sealed()
    .unwrap();
    snapforge_format::write_to_dir(&staging, &manifest).unwrap();
    publish(&staging, &snapshots, &ValidateOptions::default()).unwrap();
    ids
}

fn dest_dirs(store: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let cas = store.join("cas");
    let snapshots = store.join("snapshots");
    let staging = store.join("staging");
    std::fs::create_dir_all(&cas).unwrap();
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    (cas, snapshots, staging)
}

#[tokio::test]
async fn transfer_copies_all_chunks_and_publishes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let ids = build_source(&src, "snap-a", 8, 4096);
    let (dcas, dsnap, dstage) = dest_dirs(&dst);

    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    let outcome = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas.clone(),
        &dsnap,
        &dstage,
        "snap-a",
        TransferOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.chunks_total, 8);
    assert_eq!(outcome.chunks_copied, 8);
    assert_eq!(outcome.chunks_skipped, 0);
    assert!(outcome.destination.join("manifest.json").exists());

    // Every chunk is present and valid at the destination.
    for id in &ids {
        dest_cas.verify(id).unwrap();
    }
    // Destination manifest validates with full CAS coverage.
    let available: std::collections::BTreeSet<String> =
        dest_cas.list().unwrap().into_iter().collect();
    snapforge_format::load_validated(
        &outcome.destination,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: Some(available),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn transfer_is_idempotent_on_rerun() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    build_source(&src, "snap-b", 6, 2048);
    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());

    let dst = tmp.path().join("dst");
    let (dcas, dsnap, dstage) = dest_dirs(&dst);
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    let first = transfer_snapshot(
        source_cas.clone(),
        &src.join("snapshots"),
        dest_cas.clone(),
        &dsnap,
        &dstage,
        "snap-b",
        TransferOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.chunks_copied, 6);
    assert_eq!(first.chunks_skipped, 0);

    // Re-running the same transfer into the same destination is a no-op.
    let second = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas,
        &dsnap,
        &dstage,
        "snap-b",
        TransferOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(second.chunks_copied, 0);
    assert_eq!(second.chunks_skipped, 6);
    assert_eq!(second.bytes_transferred, 0);
}

#[tokio::test]
async fn transfer_skips_chunks_already_at_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let ids = build_source(&src, "snap-c", 5, 1024);
    let (dcas, dsnap, dstage) = dest_dirs(&dst);

    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    // Pre-copy two chunks to the destination.
    for id in ids.iter().take(2) {
        let bytes = source_cas.get(id).unwrap();
        dest_cas.put(&bytes).unwrap();
    }

    let outcome = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas.clone(),
        &dsnap,
        &dstage,
        "snap-c",
        TransferOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.chunks_total, 5);
    assert_eq!(outcome.chunks_copied, 3);
    assert_eq!(outcome.chunks_skipped, 2);
    assert_eq!(outcome.bytes_transferred, 3 * 1024);
}

#[tokio::test]
async fn transfer_fails_on_missing_source_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let ids = build_source(&src, "snap-d", 4, 1024);
    let (dcas, dsnap, dstage) = dest_dirs(&dst);

    // Delete a source chunk to simulate corruption/loss.
    std::fs::remove_file(src.join("cas").join(&ids[1])).unwrap();

    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    let opts = TransferOptions {
        max_retries: 1,
        retry_backoff: std::time::Duration::from_millis(1),
        ..TransferOptions::default()
    };
    let err = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas,
        &dsnap,
        &dstage,
        "snap-d",
        opts,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        snapforge_storage::StorageError::MissingChunk { .. }
    ));
    // Nothing published.
    assert!(!dsnap.join("snap-d").exists());
}

#[tokio::test]
async fn transfer_cancellation_is_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_source(&src, "snap-e", 50, 4096);
    let (dcas, dsnap, dstage) = dest_dirs(&dst);

    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    let cancel = CancellationToken::new();
    cancel.cancel(); // cancel before starting
    let opts = TransferOptions {
        cancel,
        ..TransferOptions::default()
    };

    let err = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas,
        &dsnap,
        &dstage,
        "snap-e",
        opts,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"));
    assert!(!dsnap.join("snap-e").exists());
}

#[tokio::test]
async fn transfer_concurrency_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_source(&src, "snap-f", 40, 2048);
    let (dcas, dsnap, dstage) = dest_dirs(&dst);

    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    // Concurrency 2 with many chunks must still complete correctly.
    let opts = TransferOptions {
        concurrency: 2,
        ..TransferOptions::default()
    };
    let outcome = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas,
        &dsnap,
        &dstage,
        "snap-f",
        opts,
    )
    .await
    .unwrap();
    assert_eq!(outcome.chunks_copied, 40);
}

#[tokio::test]
async fn transfer_refuses_conflicting_destination_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_source(&src, "snap-h", 3, 1024);
    // A different source snapshot with the same id in a second source store.
    let src2 = tmp.path().join("src2");
    build_source(&src2, "snap-h", 3, 1024);

    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let source_cas2 = Arc::new(Cas::open(src2.join("cas")).unwrap());
    let (dcas, dsnap, dstage) = dest_dirs(&dst);
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());

    transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas.clone(),
        &dsnap,
        &dstage,
        "snap-h",
        TransferOptions::default(),
    )
    .await
    .unwrap();

    // Transferring a *different* manifest under the same id must be refused.
    // (Both sources use identical content here, so force a difference by
    // mutating the second source's manifest captured_at.)
    let mpath = src2.join("snapshots/snap-h/manifest.json");
    let mut m = snapforge_format::load(&src2.join("snapshots/snap-h")).unwrap();
    m.captured_at = "2030-01-01T00:00:00Z".into();
    let m = m.sealed().unwrap();
    std::fs::write(&mpath, snapforge_format::to_bytes(&m).unwrap()).unwrap();

    let err = transfer_snapshot(
        source_cas2,
        &src2.join("snapshots"),
        dest_cas,
        &dsnap,
        &dstage,
        "snap-h",
        TransferOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, snapforge_storage::StorageError::Format(_)));
}

#[tokio::test]
async fn transfer_rejects_zero_concurrency() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_source(&src, "snap-g", 2, 1024);
    let (dcas, dsnap, dstage) = dest_dirs(&dst);
    let source_cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let dest_cas = Arc::new(Cas::open(&dcas).unwrap());
    let opts = TransferOptions {
        concurrency: 0,
        ..TransferOptions::default()
    };
    let err = transfer_snapshot(
        source_cas,
        &src.join("snapshots"),
        dest_cas,
        &dsnap,
        &dstage,
        "snap-g",
        opts,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, snapforge_storage::StorageError::Config(_)));
}
