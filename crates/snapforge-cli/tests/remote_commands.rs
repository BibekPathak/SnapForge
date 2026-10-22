//! End-to-end tests for remote transfer and the HTTP server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

fn snapforge_bin() -> PathBuf {
    let mut dir = std::env::current_exe().unwrap();
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let candidate = dir.join("snapforge");
    assert!(candidate.exists(), "build snapforge-cli first");
    candidate
}

fn write_file(dir: &Path, name: &str, len: usize, seed: u8) {
    let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_add(seed)).collect();
    std::fs::write(dir.join(name), bytes).unwrap();
}

fn store_snapshot(bin: &Path, store: &Path, src: &Path, id: &str) {
    let out = Command::new(bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "--chunk-size",
            "65536",
            "store",
            src.to_str().unwrap(),
            "--id",
            id,
            "--workload",
            "remote-cli",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Start an in-process server on an ephemeral port in a background thread.
fn start_server(store: PathBuf, token: Option<&str>) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    let token = token.map(String::from);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut opts = snapforge_remote::ServerOptions::new(store);
            if let Some(token) = token {
                opts = opts.with_token(token);
            }
            snapforge_remote::serve(listener, opts).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

fn start_server_scoped(store: PathBuf, tokens: snapforge_remote::TokenSet) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let opts = snapforge_remote::ServerOptions::new(store).with_tokens(tokens);
            snapforge_remote::serve(listener, opts).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

#[test]
fn remote_push_respects_scoped_auth() {
    use snapforge_remote::{Scope, TokenSet};

    let tmp = tempfile::tempdir().unwrap();
    let files = tmp.path().join("files");
    let src = tmp.path().join("src-store");
    let served = tmp.path().join("served");
    std::fs::create_dir_all(&files).unwrap();
    write_file(&files, "a.img", 120_000, 5);

    let bin = snapforge_bin();
    store_snapshot(&bin, &src, &files, "snap");

    let mut tokens = TokenSet::none();
    tokens.insert("ro", Scope::Read);
    tokens.insert("rw", Scope::Write);
    let addr = start_server_scoped(served.clone(), tokens);
    let endpoint = format!("http://{addr}");

    // A read-only token cannot push.
    let denied = Command::new(&bin)
        .args([
            "--store",
            src.to_str().unwrap(),
            "transfer",
            "snap",
            &endpoint,
            "--token",
            "ro",
        ])
        .output()
        .unwrap();
    assert!(!denied.status.success(), "read token must not push");
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("authentication"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    // A write token succeeds.
    let allowed = Command::new(&bin)
        .args([
            "--store",
            src.to_str().unwrap(),
            "transfer",
            "snap",
            &endpoint,
            "--token",
            "rw",
        ])
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "write token push failed: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(served.join("snapshots").join("snap").exists());
}

#[test]
fn remote_pull_fetches_and_extracts_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let served = tmp.path().join("served");
    let dst = tmp.path().join("dst");
    let restored = tmp.path().join("restored");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "core.img", 250_000, 3);
    write_file(&src, "mm.img", 120_000, 8);

    let bin = snapforge_bin();
    store_snapshot(&bin, &served, &src, "snap");
    let addr = start_server(served.clone(), None);
    let endpoint = format!("http://{addr}");

    let out = Command::new(&bin)
        .args([
            "--store",
            dst.to_str().unwrap(),
            "--json",
            "pull",
            "snap",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["transport"], "http");
    assert_eq!(v["snapshot"], "snap");
    assert!(v["chunks_downloaded"].as_u64().unwrap() > 0);
    assert_eq!(v["chunks_skipped"], 0);

    // The pulled snapshot is complete enough to extract, byte-for-byte.
    let extract = Command::new(&bin)
        .args([
            "--store",
            dst.to_str().unwrap(),
            "extract",
            "snap",
            "--output",
            restored.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        extract.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&extract.stderr)
    );
    for name in ["core.img", "mm.img"] {
        assert_eq!(
            std::fs::read(restored.join(name)).unwrap(),
            std::fs::read(src.join(name)).unwrap(),
            "{name}"
        );
    }

    // A second pull is an idempotent no-op.
    let again = Command::new(&bin)
        .args([
            "--store",
            dst.to_str().unwrap(),
            "--json",
            "pull",
            "snap",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(again.status.success());
    let v: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(v["chunks_downloaded"], 0);
    assert_eq!(v["bytes_transferred"], 0);
}

#[test]
fn remote_transfer_pushes_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let src_store = tmp.path().join("src-store");
    let dst_store = tmp.path().join("dst-store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "core.img", 250_000, 3);
    write_file(&src, "mm.img", 120_000, 8);

    let bin = snapforge_bin();
    store_snapshot(&bin, &src_store, &src, "snap");
    let addr = start_server(dst_store.clone(), None);
    let endpoint = format!("http://{addr}");

    let first = Command::new(&bin)
        .args([
            "--store",
            src_store.to_str().unwrap(),
            "--json",
            "transfer",
            "snap",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "remote transfer failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(v["transport"], "http");
    assert_eq!(v["snapshot"], "snap");
    assert!(v["chunks_uploaded"].as_u64().unwrap() > 0);
    assert_eq!(v["chunks_skipped"], 0);

    // The remote store is complete and independently verifiable.
    let verify = Command::new(&bin)
        .args([
            "--store",
            dst_store.to_str().unwrap(),
            "verify",
            "snap",
            "--require-chunks",
        ])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "remote verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    // Re-running is an idempotent no-op.
    let second = Command::new(&bin)
        .args([
            "--store",
            src_store.to_str().unwrap(),
            "--json",
            "transfer",
            "snap",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(second.status.success());
    let v: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(v["chunks_uploaded"], 0);
    assert_eq!(v["bytes_transferred"], 0);
    assert_eq!(v["chunks_skipped"], v["chunks_total"]);
}

#[test]
fn remote_transfer_requires_configured_token() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let src_store = tmp.path().join("src-store");
    let dst_store = tmp.path().join("dst-store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 150_000, 2);

    let bin = snapforge_bin();
    store_snapshot(&bin, &src_store, &src, "snap");
    let addr = start_server(dst_store, Some("tok"));
    let endpoint = format!("http://{addr}");

    let denied = Command::new(&bin)
        .args([
            "--store",
            src_store.to_str().unwrap(),
            "transfer",
            "snap",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(!denied.status.success(), "should reject a missing token");
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("authentication"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    let allowed = Command::new(&bin)
        .args([
            "--store",
            src_store.to_str().unwrap(),
            "transfer",
            "snap",
            &endpoint,
            "--token",
            "tok",
        ])
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "authenticated transfer failed: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}
