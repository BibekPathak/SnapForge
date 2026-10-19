//! End-to-end tests for the `snapforge` CLI's format-related commands.
//!
//! These build a published snapshot fixture with `snapforge-format` and then
//! exercise `inspect` and `verify` through the actual compiled binary, so exit
//! codes and diagnostics are covered, not just library behaviour.

use std::path::PathBuf;
use std::process::Command;

use snapforge_format::{
    publish, Artifact, ChunkRef, Compatibility, Manifest, SnapshotStats, ValidateOptions,
    WorkloadInfo, FORMAT_VERSION,
};

/// Return the path to the built `snapforge` binary, building it if needed.
fn snapforge_bin() -> PathBuf {
    // The integration test binary lives in target/<profile>/deps/, so the CLI
    // binary is two levels up in target/<profile>/snapforge.
    let mut dir = std::env::current_exe().unwrap();
    dir.pop(); // deps
    if dir.ends_with("deps") {
        dir.pop();
    }
    let candidate = dir.join("snapforge");
    assert!(
        candidate.exists(),
        "snapforge binary not found at {}; run `cargo build -p snapforge-cli` first",
        candidate.display()
    );
    candidate
}

/// Create a store containing a valid published snapshot named `fixture-1`.
fn create_fixture(store: &std::path::Path) {
    let cas = store.join("cas");
    let snapshots = store.join("snapshots");
    let staging = store.join("staging").join("fixture-1");
    std::fs::create_dir_all(&cas).unwrap();
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::create_dir_all(&staging).unwrap();

    let mk = |seed: u64, len: usize| -> String {
        let mut bytes = vec![0u8; len];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((seed.wrapping_add(i as u64)).wrapping_mul(2654435761) >> 13) as u8;
        }
        let id = blake3::hash(&bytes).to_hex().to_string();
        std::fs::write(cas.join(&id), &bytes).unwrap();
        id
    };

    let a0 = mk(1, 4096);
    let a1 = mk(2, 4096);
    let artifact = Artifact {
        path: "core-1.img".into(),
        size: 8192,
        chunks: vec![
            ChunkRef {
                id: a0,
                length: 4096,
            },
            ChunkRef {
                id: a1,
                length: 4096,
            },
        ],
    };

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        snapshot_id: "fixture-1".into(),
        parent: None,
        workload: WorkloadInfo {
            id: "fixture".into(),
            description: None,
        },
        captured_at: "2026-01-01T00:00:00Z".into(),
        compatibility: Compatibility {
            arch: "x86_64".into(),
            kernel_release: "test".into(),
            criu_version: None,
            criu_image_version: None,
        },
        artifacts: vec![artifact],
        stats: SnapshotStats {
            total_bytes: 8192,
            distinct_chunks: 2,
            total_chunk_refs: 2,
            dedup_saved_bytes: 0,
            capture_duration_ms: None,
            packaging_duration_ms: None,
            reused_chunks: None,
            new_chunks: None,
            reused_bytes: None,
        },
        integrity: None,
    }
    .sealed()
    .unwrap();

    snapforge_format::write_to_dir(&staging, &manifest).unwrap();
    publish(&staging, &snapshots, &ValidateOptions::default()).unwrap();
}

#[test]
fn inspect_and_verify_succeed_on_valid_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    create_fixture(tmp.path());
    let bin = snapforge_bin();
    let store = tmp.path().to_str().unwrap();

    let inspect = Command::new(&bin)
        .args(["--store", store, "inspect", "fixture-1"])
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    assert!(stdout.contains("snapshot:   fixture-1"));
    assert!(stdout.contains("validation: OK"));

    let verify = Command::new(&bin)
        .args(["--store", store, "verify", "fixture-1", "--require-chunks"])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(String::from_utf8_lossy(&verify.stdout).contains("OK"));
}

#[test]
fn verify_fails_on_tampered_integrity() {
    let tmp = tempfile::tempdir().unwrap();
    create_fixture(tmp.path());
    let bin = snapforge_bin();
    let store = tmp.path().to_str().unwrap();

    // Tamper the manifest's integrity digest.
    let manifest_path = tmp.path().join("snapshots/fixture-1/manifest.json");
    let mut m = snapforge_format::load(&tmp.path().join("snapshots/fixture-1")).unwrap();
    m.integrity = Some("0".repeat(64));
    std::fs::write(&manifest_path, snapforge_format::to_bytes(&m).unwrap()).unwrap();

    let verify = Command::new(&bin)
        .args(["--store", store, "verify", "fixture-1"])
        .output()
        .unwrap();
    assert!(!verify.status.success());
}

#[test]
fn verify_require_chunks_detects_missing_chunk() {
    let tmp = tempfile::tempdir().unwrap();
    create_fixture(tmp.path());
    let bin = snapforge_bin();
    let store = tmp.path().to_str().unwrap();

    // Remove one chunk from the CAS store.
    let cas = tmp.path().join("cas");
    let victim = std::fs::read_dir(&cas)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .next()
        .unwrap();
    std::fs::remove_file(victim).unwrap();

    let verify = Command::new(&bin)
        .args(["--store", store, "verify", "fixture-1", "--require-chunks"])
        .output()
        .unwrap();
    assert!(!verify.status.success());
    let stderr = String::from_utf8_lossy(&verify.stderr);
    assert!(stderr.contains("unknown chunk"), "stderr: {stderr}");
}

#[test]
fn inspect_fails_cleanly_when_snapshot_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = snapforge_bin();
    let store = tmp.path().to_str().unwrap();
    let out = Command::new(&bin)
        .args(["--store", store, "inspect", "does-not-exist"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to load snapshot"));
}
