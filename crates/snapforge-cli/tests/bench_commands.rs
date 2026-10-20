//! End-to-end tests for the CLI `bench` command.

use std::path::PathBuf;
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

#[test]
fn bench_json_is_labeled_storage_only() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let out = Command::new(snapforge_bin())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--chunk-size",
            "65536",
            "--concurrency",
            "2",
            "bench",
            "--size-mib",
            "2",
            "--bench-json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bench failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["kind"], "storage_only");
    assert_eq!(v["total_bytes"], 2 * 1024 * 1024);
    assert_eq!(v["chunk_size"], 65536);
    assert_eq!(v["concurrency"], 2);
    assert!(v["total_time_ns"].as_u64().unwrap() > 0);
    assert!(v["throughput_mib_s"].as_f64().unwrap() > 0.0);
    // The label must be explicit that this is not a process checkpoint.
    let label = v["label"].as_str().unwrap();
    assert!(label.contains("not a process checkpoint"), "{label}");
}

#[test]
fn bench_human_output_is_not_json() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let out = Command::new(snapforge_bin())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--chunk-size",
            "65536",
            "bench",
            "--size-mib",
            "1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("storage-only experiment"), "{stdout}");
    assert!(!stdout.trim_start().starts_with('{'), "{stdout}");
}
