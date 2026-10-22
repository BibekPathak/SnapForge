//! Tests for mutual TLS and token expiry/rotation.

use std::path::Path;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use snapforge_format::{
    publish, Compatibility, Manifest, SnapshotStats, ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};
use snapforge_remote::{
    now_unix, push_snapshot, serve, PullOptions, PushOptions, RemoteSession, Scope, ServerOptions,
    TlsClientConfig, TlsServerConfig, TokenSet,
};
use snapforge_storage::{store_reader, Cas, PipelineOptions};
use std::net::SocketAddr;
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
            id: "phase14".into(),
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

/// A CA plus a server certificate and a client certificate, all PEM.
struct Pki {
    ca_pem: Vec<u8>,
    server_cert_pem: Vec<u8>,
    server_key_pem: Vec<u8>,
    client_cert_pem: Vec<u8>,
    client_key_pem: Vec<u8>,
}

/// Build a small PKI: one CA, a server leaf for 127.0.0.1, and a client leaf.
fn build_pki() -> Pki {
    // CA
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "SnapForge Test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_pem = ca_cert.pem().into_bytes();

    // Server leaf signed by the CA.
    let server_key = KeyPair::generate().unwrap();
    let mut server_params =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    server_params
        .distinguished_name
        .push(DnType::CommonName, "snapforge-server");
    server_params.is_ca = IsCa::NoCa;
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .unwrap();

    // Client leaf signed by the CA.
    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(vec!["snapforge-client".to_string()]).unwrap();
    client_params
        .distinguished_name
        .push(DnType::CommonName, "snapforge-client");
    client_params.is_ca = IsCa::NoCa;
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .unwrap();

    Pki {
        ca_pem,
        server_cert_pem: server_cert.pem().into_bytes(),
        server_key_pem: server_key.serialize_pem().into_bytes(),
        client_cert_pem: client_cert.pem().into_bytes(),
        client_key_pem: client_key.serialize_pem().into_bytes(),
    }
}

async fn start_mtls(store: &Path, pki: &Pki, tokens: TokenSet) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = TlsServerConfig {
        cert_pem: pki.server_cert_pem.clone(),
        key_pem: pki.server_key_pem.clone(),
        client_ca_pem: Some(pki.ca_pem.clone()),
    };
    let opts = ServerOptions::new(store).with_tokens(tokens).with_tls(tls);
    let handle = tokio::spawn(async move { serve(listener, opts).await.unwrap() });
    (addr, handle)
}

fn mtls_session(pki: &Pki) -> RemoteSession {
    RemoteSession {
        tls: TlsClientConfig::with_ca_pem(pki.ca_pem.clone())
            .with_identity(pki.client_cert_pem.clone(), pki.client_key_pem.clone()),
        ..RemoteSession::default()
    }
}

fn prepare_store(dir: &Path) {
    std::fs::create_dir_all(dir.join("cas")).unwrap();
    std::fs::create_dir_all(dir.join("snapshots")).unwrap();
    std::fs::create_dir_all(dir.join("staging")).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_tls_push_and_pull_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    let dst = tmp.path().join("dst");
    let blobs = vec![("a.img", blob(1, 200_000))];
    build_snapshot(&src, "snap", &blobs, 65_536).await;

    let pki = build_pki();
    let (addr, handle) = start_mtls(&remote, &pki, TokenSet::none()).await;
    let endpoint = format!("https://{addr}");

    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let pushed = push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: mtls_session(&pki),
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pushed.chunks_uploaded > 0);

    prepare_store(&dst);
    let pulled = snapforge_remote::pull_snapshot(
        &endpoint,
        "snap",
        Arc::new(Cas::open(dst.join("cas")).unwrap()),
        &dst.join("snapshots"),
        &dst.join("staging"),
        PullOptions {
            session: mtls_session(&pki),
            ..PullOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pulled.chunks_downloaded > 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_tls_rejects_a_client_without_a_certificate() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    build_snapshot(&src, "snap", &[("a.img", blob(2, 100_000))], 65_536).await;

    let pki = build_pki();
    let (addr, handle) = start_mtls(&remote, &pki, TokenSet::none()).await;
    let endpoint = format!("https://{addr}");

    // Trust the server CA but present no client certificate.
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let err = push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: RemoteSession {
                tls: TlsClientConfig::with_ca_pem(pki.ca_pem.clone()),
                ..RemoteSession::default()
            },
            ..PushOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, snapforge_remote::RemoteError::Http(_)),
        "expected a TLS handshake failure, got: {err}"
    );
    assert!(!remote.join("snapshots").join("snap").exists());

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_composes_with_bearer_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    build_snapshot(&src, "snap", &[("a.img", blob(3, 100_000))], 65_536).await;

    let pki = build_pki();
    let mut tokens = TokenSet::none();
    tokens.insert("rw", Scope::Write);
    tokens.insert("ro", Scope::Read);
    let (addr, handle) = start_mtls(&remote, &pki, tokens).await;
    let endpoint = format!("https://{addr}");

    // Correct cert but read-only token: push is refused.
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());
    let mut session = mtls_session(&pki);
    session.token = Some("ro".into());
    let err = push_snapshot(
        cas.clone(),
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: session.clone(),
            ..PushOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.is_unauthorized(), "unexpected error: {err}");

    // Correct cert and write token: allowed. A fresh session is required
    // because a failed transfer cancels its own cancellation token.
    let mut session = mtls_session(&pki);
    session.token = Some("rw".into());
    let pushed = push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session,
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pushed.chunks_uploaded > 0);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_token_is_rejected_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote");
    build_snapshot(&src, "snap", &[("a.img", blob(4, 100_000))], 65_536).await;

    // A token that expired one second ago, and a live one.
    let mut tokens = TokenSet::none();
    tokens.insert_with_expiry("stale", Scope::Write, now_unix().saturating_sub(1));
    tokens.insert("live", Scope::Write);
    let (addr, handle) = start_plain(&remote, tokens).await;
    let endpoint = format!("http://{addr}");
    let cas = Arc::new(Cas::open(src.join("cas")).unwrap());

    let err = push_snapshot(
        cas.clone(),
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: RemoteSession {
                token: Some("stale".into()),
                ..RemoteSession::default()
            },
            ..PushOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.is_unauthorized(), "unexpected error: {err}");
    assert!(!remote.join("snapshots").join("snap").exists());

    let pushed = push_snapshot(
        cas,
        &src.join("snapshots"),
        &endpoint,
        "snap",
        PushOptions {
            session: RemoteSession {
                token: Some("live".into()),
                ..RemoteSession::default()
            },
            ..PushOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(pushed.chunks_uploaded > 0);

    handle.abort();
}

async fn start_plain(store: &Path, tokens: TokenSet) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServerOptions::new(store).with_tokens(tokens);
    let handle = tokio::spawn(async move { serve(listener, opts).await.unwrap() });
    (addr, handle)
}
