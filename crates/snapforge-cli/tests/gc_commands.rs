//! End-to-end tests for the CLI `gc` and `compact` commands.

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
        "gc-cli".to_string(),
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
fn gc_reports_then_removes_orphan_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 300_000, 7);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "s", None);

    let orphan = store.join("cas").join("f".repeat(64));
    std::fs::write(&orphan, b"orphan-bytes").unwrap();

    // Dry run reports but does not delete.
    let dry = Command::new(&bin)
        .args(["--store", store.to_str().unwrap(), "--json", "gc"])
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "gc dry run failed: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["snapshots"], 1);
    assert_eq!(v["unreferenced_chunks"], 1);
    assert_eq!(v["removed_chunks"], 0);
    assert!(orphan.exists(), "dry run must not delete");

    // Apply deletes the orphan but keeps referenced chunks.
    let applied = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "--json",
            "gc",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "gc apply failed: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["removed_chunks"], 1);
    assert!(!orphan.exists());

    // The snapshot is still fully restorable.
    let verify = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "verify",
            "s",
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
fn compact_dry_run_preserves_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 200_000, 1);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "root", None);
    write_file(&src, "b.img", 200_000, 2);
    store_snapshot(&bin, &store, &src, "child", Some("root"));

    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "--json",
            "compact",
            "child",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "compact dry run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["removed_snapshots"], serde_json::json!(["root"]));

    assert!(store.join("snapshots").join("root").exists());
    let child: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.join("snapshots").join("child").join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(child["parent"], "root");
}

#[test]
fn compact_apply_flattens_chain_and_keeps_tip_restorable() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    let out_dir = tmp.path().join("restored");
    std::fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.img", 200_000, 1);

    let bin = snapforge_bin();
    store_snapshot(&bin, &store, &src, "root", None);
    write_file(&src, "b.img", 200_000, 2);
    store_snapshot(&bin, &store, &src, "child", Some("root"));

    let out = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "--json",
            "compact",
            "child",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "compact apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["removed_snapshots"], serde_json::json!(["root"]));

    assert!(!store.join("snapshots").join("root").exists());
    let child: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.join("snapshots").join("child").join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert!(child.get("parent").is_none() || child["parent"].is_null());

    let verify = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "verify",
            "child",
            "--require-chunks",
        ])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    let extract = Command::new(&bin)
        .args([
            "--store",
            store.to_str().unwrap(),
            "extract",
            "child",
            "--output",
            out_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        extract.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&extract.stderr)
    );
    for name in ["a.img", "b.img"] {
        assert_eq!(
            std::fs::read(out_dir.join(name)).unwrap(),
            std::fs::read(src.join(name)).unwrap(),
            "{name}"
        );
    }
}
