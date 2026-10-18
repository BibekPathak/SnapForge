//! End-to-end tests for the CLI `transfer` command.

use std::path::{Path, PathBuf};
use std::process::Command;

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
            "transfer-cli",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn transfer_copies_snapshot_between_stores() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store_a = tmp.path().join("store-a");
    let store_b = tmp.path().join("store-b");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "core-1.img", 500_000, 3);
    write_file(&src, "mm-1.img", 300_000, 9);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store_a, &src, "xfer-cli");

    // Transfer A -> B.
    let out = Command::new(&bin)
        .args([
            "--store",
            store_a.to_str().unwrap(),
            "transfer",
            "xfer-cli",
            store_b.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "transfer failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify at B with full CAS coverage.
    let verify = Command::new(&bin)
        .args([
            "--store",
            store_b.to_str().unwrap(),
            "verify",
            "xfer-cli",
            "--require-chunks",
        ])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "verify at destination failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
}

#[test]
fn transfer_rerun_is_idempotent_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store_a = tmp.path().join("store-a");
    let store_b = tmp.path().join("store-b");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 200_000, 1);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store_a, &src, "idem");

    let args = [
        "--store",
        store_a.to_str().unwrap(),
        "transfer",
        "idem",
        store_b.to_str().unwrap(),
        "--json",
    ];
    let first = Command::new(&bin).args(args).output().unwrap();
    assert!(first.status.success());
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert!(first_json["chunks_copied"].as_u64().unwrap() > 0);

    let second = Command::new(&bin).args(args).output().unwrap();
    assert!(
        second.status.success(),
        "rerun failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["chunks_copied"], 0);
    assert_eq!(second_json["bytes_transferred"], 0);
}

#[test]
fn transfer_fails_when_source_snapshot_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let store_a = tmp.path().join("store-a");
    let store_b = tmp.path().join("store-b");
    std::fs::create_dir_all(&store_a).unwrap();

    let bin = snapforge_bin();
    let out = Command::new(&bin)
        .args([
            "--store",
            store_a.to_str().unwrap(),
            "transfer",
            "nonexistent",
            store_b.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}
