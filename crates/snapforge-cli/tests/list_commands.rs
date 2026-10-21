//! End-to-end tests for the CLI `list` command.

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

fn store_snapshot(bin: &Path, store: &Path, src: &Path, id: &str, parent: Option<&str>) {
    let mut args = vec![
        "--store".to_string(),
        store.to_str().unwrap().to_string(),
        "--chunk-size".to_string(),
        "65536".to_string(),
        "store".to_string(),
        src.to_str().unwrap().to_string(),
        "--id".to_string(),
        id.to_string(),
        "--workload".to_string(),
        "list-cli".to_string(),
    ];
    if let Some(p) = parent {
        args.push("--parent".to_string());
        args.push(p.to_string());
    }
    let out = Command::new(bin).args(&args).output().unwrap();
    assert!(
        out.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn list_reports_empty_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).unwrap();

    let out = Command::new(snapforge_bin())
        .args(["--store", store.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no snapshots"),
        "unexpected output: {stdout}"
    );
}

#[test]
fn list_json_summarizes_chain_and_reclaimable_storage() {
    let tmp = tempfile::tempdir().unwrap();
    let child_src = tmp.path().join("child-src");
    let root_src = tmp.path().join("root-src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&child_src).unwrap();
    std::fs::create_dir_all(&root_src).unwrap();

    // The root has a file the child does not, so flattening to the child frees
    // those root-only chunks.
    write_file(&root_src, "dropped.img", 200_000, 1);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &root_src, "root", None);
    write_file(&child_src, "kept.img", 200_000, 2);
    store_snapshot(&bin, &store, &child_src, "child", Some("root"));

    let out = Command::new(&bin)
        .args(["--store", store.to_str().unwrap(), "--json", "list"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["count"], 2);

    let snapshots = v["snapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), 2);
    let child = snapshots.iter().find(|s| s["id"] == "child").unwrap();
    let root = snapshots.iter().find(|s| s["id"] == "root").unwrap();
    assert_eq!(root["id"], "root");
    assert_eq!(root["chain_depth"], 1);
    assert_eq!(root["is_tip"], false);
    assert_eq!(child["id"], "child");
    assert_eq!(child["chain_depth"], 2);
    assert_eq!(child["is_tip"], true);
    assert_eq!(child["chain"], serde_json::json!(["root", "child"]));
    assert_eq!(root["chain_tip"], "child");
    assert_eq!(child["chain_tip"], "child");
    // Flattening the chain to `child` reclaims root-only chunks.
    assert!(child["reclaimable_bytes"].as_u64().unwrap() > 0);
    assert_eq!(root["reclaimable_bytes"], child["reclaimable_bytes"]);
}

#[test]
fn list_flags_invalid_manifests_without_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 100_000, 4);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "good", None);
    store_snapshot(&bin, &store, &src, "bad", None);
    std::fs::write(
        store.join("snapshots").join("bad").join("manifest.json"),
        b"{not json",
    )
    .unwrap();

    let out = Command::new(&bin)
        .args(["--store", store.to_str().unwrap(), "--json", "list"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "list must not fail on a corrupt snapshot: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["count"], 2);
    let bad = v["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "bad")
        .unwrap();
    assert_eq!(bad["valid"], false);
    assert!(bad["error"].is_string());
}

#[test]
fn list_reports_unreferenced_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 100_000, 9);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "snap", None);
    std::fs::write(store.join("cas").join("f".repeat(64)), b"orphan").unwrap();

    let out = Command::new(&bin)
        .args(["--store", store.to_str().unwrap(), "--json", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["unreferenced_chunks"], 1);
    assert_eq!(v["unreferenced_bytes"], 6);
}
