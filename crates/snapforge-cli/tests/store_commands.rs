//! End-to-end tests for the CLI `store` command (content-addressed storage).
//!
//! These build artifact directories, ingest them through the compiled binary,
//! and confirm the published snapshot is valid, deduplicates identical content,
//! and fails cleanly on empty/missing sources.

use std::path::{Path, PathBuf};
use std::process::Command;

fn snapforge_bin() -> PathBuf {
    let mut dir = std::env::current_exe().unwrap();
    dir.pop(); // deps
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

#[test]
fn store_inspect_verify_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "core-1.img", 400_000, 1);
    write_file(&src, "mm-1.img", 250_000, 2);

    let bin = snapforge_bin();
    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "--chunk-size",
            "65536",
            "store",
            src.to_str().unwrap(),
            "--id",
            "e2e-1",
            "--workload",
            "test",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let verify = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "verify",
            "e2e-1",
            "--require-chunks",
        ])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
}

#[test]
fn store_deduplicates_identical_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    // Two identical 1 MiB files.
    write_file(&src, "a.img", 1024 * 1024, 7);
    std::fs::copy(src.join("a.img"), src.join("b.img")).unwrap();

    let bin = snapforge_bin();
    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "--chunk-size",
            "262144",
            "store",
            src.to_str().unwrap(),
            "--id",
            "dedup-1",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let stats = &json["stats"];
    assert_eq!(stats["total_bytes"], 2 * 1024 * 1024);
    assert!(
        stats["dedup_saved_bytes"].as_u64().unwrap() >= 1024 * 1024 - 262144,
        "expected substantial dedup savings, got {stats}"
    );

    // The CAS should contain far fewer files than the number of chunk refs.
    let cas_files = std::fs::read_dir(store.join("cas")).unwrap().count();
    assert!(
        cas_files < 12,
        "expected dedup, found {cas_files} cas files"
    );
}

#[test]
fn store_fails_on_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("empty");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();

    let bin = snapforge_bin();
    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "store",
            src.to_str().unwrap(),
            "--id",
            "nope",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no files"));
}

#[test]
fn store_refuses_duplicate_snapshot_id() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "x.img", 1000, 0);

    let bin = snapforge_bin();
    let args = [
        "--store",
        store.to_str().unwrap(),
        "store",
        src.to_str().unwrap(),
        "--id",
        "same",
    ];
    let first = Command::new(&bin).args(args).output().unwrap();
    assert!(first.status.success());
    let second = Command::new(&bin).args(args).output().unwrap();
    assert!(!second.status.success(), "second publish must be refused");
}
