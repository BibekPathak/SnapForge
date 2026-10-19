//! End-to-end tests for incremental snapshots through the CLI.

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

/// Deterministic, non-repeating blob so chunks are distinct.
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

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(name), bytes).unwrap();
}

fn store_snapshot(
    bin: &Path,
    store: &Path,
    src: &Path,
    id: &str,
    parent: Option<&str>,
) -> std::process::Output {
    let mut args = vec![
        "--store".to_string(),
        store.to_str().unwrap().to_string(),
        "--chunk-size".to_string(),
        "16384".to_string(),
        "store".to_string(),
        src.to_str().unwrap().to_string(),
        "--id".to_string(),
        id.to_string(),
        "--workload".to_string(),
        "inc-cli".to_string(),
        "--json".to_string(),
    ];
    if let Some(p) = parent {
        args.push("--parent".to_string());
        args.push(p.to_string());
    }
    Command::new(bin).args(args).output().unwrap()
}

#[test]
fn incremental_chain_reports_reuse_and_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let base = tmp.path().join("base");
    let child = tmp.path().join("child");

    // Baseline: f1..f4.
    let f1 = blob(1, 65536);
    let f2 = blob(2, 65536);
    let f3 = blob(3, 65536);
    let f4 = blob(4, 65536);
    write(&base, "f1.img", &f1);
    write(&base, "f2.img", &f2);
    write(&base, "f3.img", &f3);
    write(&base, "f4.img", &f4);

    // Child: f4 changed.
    let f4_new = blob(44, 65536);
    write(&child, "f1.img", &f1);
    write(&child, "f2.img", &f2);
    write(&child, "f3.img", &f3);
    write(&child, "f4.img", &f4_new);

    let bin = snapforge_bin();
    let o1 = store_snapshot(&bin, &store, &base, "base", None);
    assert!(
        o1.status.success(),
        "{}",
        String::from_utf8_lossy(&o1.stderr)
    );
    let o2 = store_snapshot(&bin, &store, &child, "child", Some("base"));
    assert!(
        o2.status.success(),
        "{}",
        String::from_utf8_lossy(&o2.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&o2.stdout).unwrap();
    let reuse = &json["reuse"];
    assert_eq!(reuse["reused_chunks"], 12);
    assert_eq!(reuse["new_chunks"], 4);
    assert_eq!(reuse["reused_bytes"], 3 * 65536);

    // Inspect --chain reports both links.
    let inspect = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "inspect",
            "child",
            "--chain",
        ])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let text = String::from_utf8_lossy(&inspect.stdout);
    assert!(text.contains("chain:      base -> child"), "got: {text}");

    // Verify the chain with full CAS coverage.
    let verify = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "verify",
            "child",
            "--chain",
            "--require-chunks",
        ])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "chain verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(String::from_utf8_lossy(&verify.stdout).contains("chain OK"));
}

#[test]
fn incremental_rejects_missing_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let src = tmp.path().join("src");
    write(&src, "f1.img", &blob(1, 32768));

    let bin = snapforge_bin();
    let out = store_snapshot(&bin, &store, &src, "child", Some("ghost"));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}

#[test]
fn inspect_child_without_chain_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let src = tmp.path().join("src");
    write(&src, "f1.img", &blob(1, 32768));

    let bin = snapforge_bin();
    assert!(store_snapshot(&bin, &store, &src, "solo", None)
        .status
        .success());
    let inspect = Command::new(&bin)
        .args(["--store", store.to_str().unwrap(), "inspect", "solo"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    assert!(String::from_utf8_lossy(&inspect.stdout).contains("validation: OK"));
}
