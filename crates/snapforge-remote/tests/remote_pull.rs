//! End-to-end tests for remote pull (the mirror of push).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use snapforge_format::{
    publish, Compatibility, Manifest, SnapshotStats, ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_remote::{
    pull_snapshot, push_snapshot, serve, PullOptions, PushOptions, ServerOptions,
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
            std::io::Cursor::new(bytes.clone()),
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
            id: "pull".into(),
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
    if let Some(token) = token {
        opts = opts.with_token(token);
    }
    let handle = tokio::spawn(async move {
        serve(listener, opts).await.unwrap();
    });
    (addr, handle)
}

fn endpoint(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// Seed a remote store by pushing a locally built snapshot.
async fn seed_remote(addr: SocketAddr, src: &Path, id: &str) {
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint(addr),
        id,
        PushOptions::default(),
    )
    .await
    .unwrap();
}

fn prepare_store(dir: &Path) {
    std::fs::create_dir_all(dir.join("cas")).unwrap();
    std::fs::create_dir_all(dir.join("snapshots")).unwrap();
    std::fs::create_dir_all(dir.join("staging")).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_copies_snapshot_and_extracts_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    let blobs = vec![("core.img", blob(1, 300_000)), ("mm.img", blob(2, 150_000))];
    build_snapshot(&src, "snap", &blobs, 65_536).await;
    let (addr, handle) = start_server(&remote, None).await;
    seed_remote(addr, &src, "snap").await;
    prepare_store(&dst);

    let outcome = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.snapshot_id, "snap");
    assert!(outcome.chunks_downloaded > 0);
    assert_eq!(outcome.chunks_skipped, 0);
    assert!(outcome.bytes_transferred > 0);

    let restored = tmp.path().join("restored");
    extract_snapshot(
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots").join("snap"),
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
async fn pull_is_idempotent_on_rerun() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    build_snapshot(&src, "snap", &[("a.img", blob(3, 200_000))], 65_536).await;
    let (addr, handle) = start_server(&remote, None).await;
    seed_remote(addr, &src, "snap").await;
    prepare_store(&dst);

    let first = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions::default(),
    )
    .await
    .unwrap();
    assert!(first.chunks_downloaded > 0);

    let second = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(second.chunks_downloaded, 0);
    assert_eq!(second.chunks_skipped, second.chunks_total);
    assert_eq!(second.bytes_transferred, 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_resumes_a_partially_downloaded_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    let blobs = vec![("a.img", blob(5, 300_000))];
    let manifest = build_snapshot(&src, "snap", &blobs, 65_536).await;
    let (addr, handle) = start_server(&remote, None).await;
    seed_remote(addr, &src, "snap").await;
    prepare_store(&dst);

    // Simulate an interrupted download of the first chunk: a `.part` file with
    // the first half of the real bytes.
    let cas_dir = dst.join("cas");
    let chunk_id = manifest.chunk_lengths().into_keys().next().unwrap();
    let full = std::fs::read(src.join("cas").join(&chunk_id)).unwrap();
    let half = full.len() / 2;
    std::fs::write(cas_dir.join(format!(".part-{chunk_id}")), &full[..half]).unwrap();

    let outcome = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(&cas_dir).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions::default(),
    )
    .await
    .unwrap();

    // The seeded chunk was completed via a range request, not re-downloaded.
    assert_eq!(outcome.chunks_resumed, 1);
    assert!(cas_dir.join(&chunk_id).is_file());
    // The `.part` scratch file is gone.
    assert!(!cas_dir.join(format!(".part-{chunk_id}")).exists());
    // The pulled chunk matches the source byte-for-byte.
    assert_eq!(std::fs::read(cas_dir.join(&chunk_id)).unwrap(), full);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_fails_on_a_corrupt_remote_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    let blobs = vec![("a.img", blob(6, 200_000))];
    let manifest = build_snapshot(&src, "snap", &blobs, 65_536).await;
    let (addr, handle) = start_server(&remote, None).await;
    seed_remote(addr, &src, "snap").await;
    prepare_store(&dst);

    // Corrupt a chunk directly in the remote CAS.
    let chunk_id = manifest.chunk_lengths().into_keys().next().unwrap();
    std::fs::write(remote.join("cas").join(&chunk_id), b"corrupt").unwrap();

    let err = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions::default(),
    )
    .await
    .unwrap_err();
    // A corrupt remote chunk surfaces as a server-side 422.
    assert!(
        err.to_string().contains("corrupt") || err.to_string().contains("422"),
        "unexpected error: {err}"
    );
    assert!(!dst.join("snapshots").join("snap").exists());

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_requires_token_when_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    build_snapshot(&src, "snap", &[("a.img", blob(7, 100_000))], 65_536).await;
    // Seed the remote through an unauthenticated server, then restart with auth.
    let (seed_addr, seed_handle) = start_server(&remote, None).await;
    seed_remote(seed_addr, &src, "snap").await;
    seed_handle.abort();

    let (addr, handle) = start_server(&remote, Some("tok")).await;
    prepare_store(&dst);
    let opts = PullOptions::default();

    let denied = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        opts.clone(),
    )
    .await
    .unwrap_err();
    assert!(denied.is_unauthorized(), "unexpected error: {denied}");

    let allowed = pull_snapshot(
        &endpoint(addr),
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions {
            session: snapforge_remote::RemoteSession {
                token: Some("tok".into()),
                ..snapforge_remote::RemoteSession::default()
            },
            ..PullOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(allowed.chunks_downloaded > 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_missing_snapshot_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    let (addr, handle) = start_server(&remote, None).await;
    prepare_store(&dst);

    let err = pull_snapshot(
        &endpoint(addr),
        "nope",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no snapshot"), "unexpected: {err}");

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_range_request_returns_partial_content() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let blobs = vec![("a.img", blob(8, 200_000))];
    let manifest = build_snapshot(&src, "snap", &blobs, 65_536).await;
    let (addr, handle) = start_server(&remote, None).await;
    seed_remote(addr, &src, "snap").await;

    let chunk_id = manifest.chunk_lengths().into_keys().next().unwrap();
    let client = reqwest::Client::new();

    // A full GET returns 200.
    let full = client
        .get(format!("{}/v1/chunks/{chunk_id}", endpoint(addr)))
        .send()
        .await
        .unwrap();
    assert_eq!(full.status(), reqwest::StatusCode::OK);
    let full_len = full.bytes().await.unwrap().len();

    // A ranged GET returns 206 with only the requested tail.
    let ranged = client
        .get(format!("{}/v1/chunks/{chunk_id}", endpoint(addr)))
        .header(reqwest::header::RANGE, "bytes=100-")
        .send()
        .await
        .unwrap();
    assert_eq!(ranged.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let range_len = ranged.bytes().await.unwrap().len();
    assert_eq!(range_len, full_len - 100);

    handle.abort();
}
