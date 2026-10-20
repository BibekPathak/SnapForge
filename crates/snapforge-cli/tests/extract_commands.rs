//! End-to-end tests for the CLI `extract` command.

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
            "extract-cli",
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
fn extract_restores_files_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    let out_dir = tmp.path().join("restored");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "core-1.img", 400_000, 3);
    write_file(&src, "mm-1.img", 250_000, 9);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "ckpt-cli");

    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "extract",
            "ckpt-cli",
            "--output",
            out_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["snapshot"], "ckpt-cli");
    assert_eq!(v["artifacts"], 2);
    assert_eq!(v["bytes_written"], 650_000);

    // Files must match exactly.
    for name in ["core-1.img", "mm-1.img"] {
        assert_eq!(
            std::fs::read(out_dir.join(name)).unwrap(),
            std::fs::read(src.join(name)).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn extract_refuses_non_empty_destination_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    let out_dir = tmp.path().join("restored");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 50_000, 1);
    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "s");

    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("existing"), b"keep").unwrap();

    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "extract",
            "s",
            "--output",
            out_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "should refuse a non-empty destination"
    );
    // Existing file untouched.
    assert_eq!(std::fs::read(out_dir.join("existing")).unwrap(), b"keep");

    // With --force it succeeds.
    let forced = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "extract",
            "s",
            "--output",
            out_dir.to_str().unwrap(),
            "--force",
        ])
        .output()
        .unwrap();
    assert!(forced.status.success());
    assert!(out_dir.join("a.img").exists());
}

#[test]
fn extract_fails_on_missing_chunk_without_writing_output() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    let out_dir = tmp.path().join("restored");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 200_000, 5);
    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "s");

    // Remove one chunk from the CAS.
    let cas = store.join("cas");
    let victim = std::fs::read_dir(&cas)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
        .expect("at least one chunk");
    std::fs::remove_file(&victim).unwrap();

    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "extract",
            "s",
            "--output",
            out_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "extract must fail on a missing chunk"
    );
    assert!(
        !out_dir.join("a.img").exists(),
        "no artifact should be written on failure"
    );
}
