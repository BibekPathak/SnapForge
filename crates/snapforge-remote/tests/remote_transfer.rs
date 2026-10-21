//! End-to-end tests for the HTTP remote transport.

use std::io::Cursor;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use snapforge_format::{
    publish, Compatibility, Manifest, SnapshotStats, ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_remote::{
    health, push_snapshot, serve, PushOptions, RemoteSession, ServerOptions, PROTOCOL_VERSION,
};
use snapforge_storage::{extract_snapshot, store_reader, Cas, ExtractOptions, PipelineOptions};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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

async fn build_snapshot(
    store: &Path,
    id: &str,
    blobs: &[(&str, Vec<u8>)],
    chunk_size: usize,
) -> Manifest {
    let cas = Arc::new(Cas::open(store.join("cas")).unwrap());
    let snapshots = store.join("snapshots");
    let staging = store.join("staging").join(id);
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::create_dir_all(&staging).unwrap();

    let mut artifacts = Vec::new();
    let mut stats = SnapshotStats::default();
    let mut distinct = std::collections::BTreeSet::new();
    for (name, bytes) in blobs {
        let ingested = store_reader(
            cas.clone(),
            Cursor::new(bytes.clone()),
            PipelineOptions {
                chunk_size,
                concurrency: 4,
                verify_on_write: false,
            },
        )
        .await
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
        parent: None,
        workload: WorkloadInfo {
            id: "remote".into(),
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

async fn start_server(store: &Path, token: Option<&str>) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut opts = ServerOptions::new(store);
    opts.token = token.map(String::from);
    let handle = tokio::spawn(async move {
        serve(listener, opts).await.unwrap();
    });
    (addr, handle)
}

fn endpoint(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_copies_snapshot_and_remote_extracts_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let blobs = vec![("core.img", blob(1, 300_000)), ("mm.img", blob(2, 150_000))];
    let manifest = build_snapshot(&src, "snap", &blobs, 65_536).await;
    let (addr, handle) = start_server(&dst, None).await;

    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let outcome = push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint(addr),
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap();

    let distinct = manifest.chunk_lengths().len() as u64;
    assert_eq!(outcome.chunks_total, distinct);
    assert_eq!(outcome.chunks_uploaded, distinct);
    assert_eq!(outcome.chunks_skipped, 0);
    assert!(outcome.bytes_transferred > 0);

    let remote_snap = dst.join("snapshots").join("snap");
    let remote_cas = Arc::new(Cas::open(dst.join("cas")).unwrap());
    let available: std::collections::BTreeSet<String> =
        remote_cas.list().unwrap().into_iter().collect();
    snapforge_format::load_validated(
        &remote_snap,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: Some(available),
        },
    )
    .unwrap();

    let restored = tmp.path().join("restored");
    extract_snapshot(
        remote_cas,
        &remote_snap,
        &restored,
        ExtractOptions::default(),
    )
    .await
    .unwrap();
    for (name, bytes) in &blobs {
        assert_eq!(
            &std::fs::read(restored.join(name)).unwrap(),
            bytes,
            "{name}"
        );
    }

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_snapshot(&src, "snap", &[("a.img", blob(3, 200_000))], 65_536).await;
    let (addr, handle) = start_server(&dst, None).await;
    let url = endpoint(addr);

    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let first = push_snapshot(
        cas.clone(),
        &src.join("snapshots"),
        &url,
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap();
    assert!(first.chunks_uploaded > 0);

    let second = push_snapshot(
        cas,
        &src.join("snapshots"),
        &url,
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(second.chunks_uploaded, 0);
    assert_eq!(second.chunks_skipped, second.chunks_total);
    assert_eq!(second.bytes_transferred, 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_fails_on_missing_source_chunk_without_publishing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_snapshot(&src, "snap", &[("a.img", blob(4, 200_000))], 65_536).await;
    let (addr, handle) = start_server(&dst, None).await;

    let victim = std::fs::read_dir(src.join("cas"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
        .unwrap();
    std::fs::remove_file(victim).unwrap();

    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let err = push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint(addr),
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("missing"),
        "unexpected error: {err}"
    );
    assert!(!dst.join("snapshots").join("snap").exists());

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_requires_bearer_token_when_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    build_snapshot(&src, "snap", &[("a.img", blob(5, 100_000))], 65_536).await;
    let (addr, handle) = start_server(&dst, Some("s3cret")).await;
    let url = endpoint(addr);
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());

    let unauth = push_snapshot(
        cas.clone(),
        &src.join("snapshots"),
        &url,
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(unauth.is_unauthorized(), "unexpected error: {unauth}");

    let ok = push_snapshot(
        cas,
        &src.join("snapshots"),
        &url,
        "snap",
        PushOptions {
            session: RemoteSession {
                token: Some("s3cret".into()),
                ..RemoteSession::default()
            },
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(ok.chunks_uploaded > 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_rejects_chunk_whose_content_does_not_match_its_id() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("dst");
    let (addr, handle) = start_server(&dst, None).await;

    let id = "f".repeat(64);
    let resp = reqwest::Client::new()
        .put(format!("{}/v1/chunks/{id}", endpoint(addr)))
        .body(b"this is not the hashed content".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(!dst.join("cas").join(&id).exists());

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_conflicts_when_remote_has_a_different_snapshot_with_same_id() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let dst = tmp.path().join("dst");
    build_snapshot(&a, "dup", &[("a.img", blob(6, 100_000))], 65_536).await;
    build_snapshot(&b, "dup", &[("b.img", blob(7, 100_000))], 65_536).await;
    let (addr, handle) = start_server(&dst, None).await;
    let url = endpoint(addr);

    push_snapshot(
        Arc::new(Cas::open(a.join("cas")).unwrap()),
        &a.join("snapshots"),
        &url,
        "dup",
        PushOptions::default(),
    )
    .await
    .unwrap();

    let err = push_snapshot(
        Arc::new(Cas::open(b.join("cas")).unwrap()),
        &b.join("snapshots"),
        &url,
        "dup",
        PushOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("different snapshot"),
        "unexpected error: {err}"
    );

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_existing_repairs_a_corrupt_remote_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let blobs = vec![("a.img", blob(8, 200_000))];
    let manifest = build_snapshot(&src, "snap", &blobs, 65_536).await;
    let (addr, handle) = start_server(&dst, None).await;
    let url = endpoint(addr);
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());

    push_snapshot(
        cas.clone(),
        &src.join("snapshots"),
        &url,
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap();

    let chunk_id = manifest.chunk_lengths().into_keys().next().unwrap();
    let corrupt_path = dst.join("cas").join(&chunk_id);
    std::fs::write(&corrupt_path, b"corrupt").unwrap();

    let outcome = push_snapshot(
        cas,
        &src.join("snapshots"),
        &url,
        "snap",
        PushOptions {
            verify_existing: true,
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(outcome.chunks_uploaded >= 1);

    let remote_cas = Cas::open(dst.join("cas")).unwrap();
    remote_cas.verify(&chunk_id).unwrap();

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_protocol_version() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("dst");
    let (addr, handle) = start_server(&dst, None).await;

    let body = health(&endpoint(addr), &RemoteSession::default())
        .await
        .unwrap();
    assert!(
        body.contains(PROTOCOL_VERSION),
        "unexpected health body: {body}"
    );

    handle.abort();
}
