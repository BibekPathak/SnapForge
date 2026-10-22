//! Tests for in-process TLS and scoped bearer-token authorization.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use snapforge_format::{
    publish, Compatibility, Manifest, SnapshotStats, ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_remote::{
    pull_snapshot, serve, PullOptions, PushOptions, RemoteSession, Scope, ServerOptions,
    TlsClientConfig, TlsServerConfig, TokenSet,
};
use snapforge_storage::{store_reader, Cas, PipelineOptions};
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
            id: "tls".into(),
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

/// Generate a self-signed certificate for `127.0.0.1` and return (cert_pem, key_pem).
fn self_signed() -> (Vec<u8>, Vec<u8>) {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap();
    (
        cert.cert.pem().into_bytes(),
        cert.key_pair.serialize_pem().into_bytes(),
    )
}

async fn start_plain(store: &Path, tokens: TokenSet) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServerOptions::new(store).with_tokens(tokens);
    let handle = tokio::spawn(async move { serve(listener, opts).await.unwrap() });
    (addr, handle)
}

async fn start_tls(store: &Path, tokens: TokenSet) -> (SocketAddr, JoinHandle<()>, Vec<u8>) {
    let (cert_pem, key_pem) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = TlsServerConfig {
        cert_pem: cert_pem.clone(),
        key_pem,
    };
    let opts = ServerOptions::new(store).with_tokens(tokens).with_tls(tls);
    let handle = tokio::spawn(async move { serve(listener, opts).await.unwrap() });
    (addr, handle, cert_pem)
}

fn prepare_store(dir: &Path) {
    std::fs::create_dir_all(dir.join("cas")).unwrap();
    std::fs::create_dir_all(dir.join("snapshots")).unwrap();
    std::fs::create_dir_all(dir.join("staging")).unwrap();
}

fn session_with_ca(cert_pem: &[u8], token: Option<&str>) -> RemoteSession {
    RemoteSession {
        token: token.map(String::from),
        tls: TlsClientConfig::with_ca_pem(cert_pem.to_vec()),
        ..RemoteSession::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_push_and_pull_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    let blobs = vec![("a.img", blob(1, 200_000))];
    build_snapshot(&src, "snap", &blobs, 65_536).await;

    let (addr, handle, cert_pem) = start_tls(&remote, TokenSet::none()).await;
    let endpoint = format!("https://{addr}");

    // Push over TLS (trusting the self-signed cert).
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let pushed = snapforge_remote::push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: session_with_ca(&cert_pem, None),
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pushed.chunks_uploaded > 0);

    // Pull over TLS into a fresh store.
    prepare_store(&dst);
    let pulled = pull_snapshot(
        &endpoint,
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions {
            session: session_with_ca(&cert_pem, None),
            ..PullOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pulled.chunks_downloaded > 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_client_rejects_untrusted_certificate() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    build_snapshot(&src, "snap", &[("a.img", blob(2, 100_000))], 65_536).await;

    let (addr, handle, _cert_pem) = start_tls(&remote, TokenSet::none()).await;
    let endpoint = format!("https://{addr}");

    // Default trust (system roots) must not accept this self-signed cert.
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let err = snapforge_remote::push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, snapforge_remote::RemoteError::Http(_)),
        "expected a TLS/transport error, got: {err}"
    );

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_scope_token_cannot_push() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    build_snapshot(&src, "snap", &[("a.img", blob(3, 100_000))], 65_536).await;

    let mut tokens = TokenSet::none();
    tokens.insert("ro", Scope::Read);
    let (addr, handle) = start_plain(&remote, tokens).await;
    let endpoint = format!("http://{addr}");

    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let err = snapforge_remote::push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: RemoteSession {
                token: Some("ro".into()),
                ..RemoteSession::default()
            },
            ..PushOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.is_unauthorized(), "unexpected error: {err}");
    assert!(!remote.join("snapshots").join("snap").exists());

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_scope_token_can_push_and_read() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    build_snapshot(&src, "snap", &[("a.img", blob(4, 100_000))], 65_536).await;

    let mut tokens = TokenSet::none();
    tokens.insert("rw", Scope::Write);
    let (addr, handle) = start_plain(&remote, tokens).await;
    let endpoint = format!("http://{addr}");
    let session = RemoteSession {
        token: Some("rw".into()),
        ..RemoteSession::default()
    };

    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let pushed = snapforge_remote::push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: session.clone(),
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pushed.chunks_uploaded > 0);

    // A write token also satisfies read.
    prepare_store(&dst);
    let pulled = pull_snapshot(
        &endpoint,
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions {
            session,
            ..PullOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pulled.chunks_downloaded > 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_needs_no_token_even_when_auth_is_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote");
    let mut tokens = TokenSet::none();
    tokens.insert("secret", Scope::Write);
    let (addr, handle) = start_plain(&remote, tokens).await;

    let body = snapforge_remote::health(&format!("http://{addr}"), &RemoteSession::default())
        .await
        .unwrap();
    assert!(body.contains("ok"), "unexpected health body: {body}");

    handle.abort();
}
