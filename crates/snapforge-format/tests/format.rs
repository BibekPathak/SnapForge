//! Integration tests for manifest validation and atomic publication.
//!
//! These exercise the format layer as a whole: building manifests, validating
//! them (including adversarial cases), and publishing them atomically.

use std::collections::BTreeSet;

use snapforge_format::{
    cleanup_staging, load, load_validated, publish, to_bytes, validate, validate_artifact_path,
    validate_snapshot_id, Artifact, ChunkRef, Compatibility, FormatError, Manifest, SnapshotStats,
    ValidateOptions, WorkloadInfo, FORMAT_VERSION,
};

/// Build a small, valid, sealed manifest.
fn sample_manifest(id: &str) -> Manifest {
    // Two artifacts; "shared" chunk appears in both (dedup) and one is unique.
    let shared = "a".repeat(64);
    let unique = "b".repeat(64);
    let mut m = Manifest {
        format_version: FORMAT_VERSION,
        snapshot_id: id.to_string(),
        parent: None,
        workload: WorkloadInfo {
            id: "cpu-memory".into(),
            description: Some("integration test workload".into()),
        },
        captured_at: "2026-01-01T00:00:00Z".into(),
        compatibility: Compatibility {
            arch: "x86_64".into(),
            kernel_release: "test".into(),
            criu_version: Some("4.2.1".into()),
            criu_image_version: None,
        },
        artifacts: vec![
            Artifact {
                path: "core-1.img".into(),
                size: 2048,
                chunks: vec![
                    ChunkRef {
                        id: shared.clone(),
                        length: 1024,
                    },
                    ChunkRef {
                        id: unique.clone(),
                        length: 1024,
                    },
                ],
            },
            Artifact {
                path: "mm-1.img".into(),
                size: 1024,
                chunks: vec![ChunkRef {
                    id: shared.clone(),
                    length: 1024,
                }],
            },
        ],
        stats: SnapshotStats {
            total_bytes: 3072,
            distinct_chunks: 2,
            total_chunk_refs: 3,
            dedup_saved_bytes: 1024,
            capture_duration_ms: Some(42),
            packaging_duration_ms: Some(7),
        },
        integrity: None,
    };
    m = m.sealed().unwrap();
    m
}

fn available_for(m: &Manifest) -> BTreeSet<String> {
    m.chunk_lengths().into_keys().collect()
}

#[test]
fn valid_manifest_passes_strict_validation() {
    let m = sample_manifest("snap-1");
    let opts = ValidateOptions {
        check_integrity: true,
        available_chunks: Some(available_for(&m)),
    };
    let v = validate(&m, &opts).unwrap();
    assert_eq!(v.manifest().snapshot_id, "snap-1");
}

#[test]
fn manifest_round_trips_through_json() {
    let m = sample_manifest("snap-2");
    let bytes = to_bytes(&m).unwrap();
    let back: Manifest = snapforge_format::from_bytes(&bytes).unwrap();
    assert_eq!(m, back);
    // Round-tripped manifest still validates.
    validate(&back, &ValidateOptions::default()).unwrap();
}

#[test]
fn rejects_unsupported_version() {
    let mut m = sample_manifest("snap-3");
    m.format_version = 999;
    m = m.sealed().unwrap();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::UnsupportedVersion { .. }));
}

#[test]
fn rejects_missing_integrity() {
    let mut m = sample_manifest("snap-4");
    m.integrity = None;
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::IntegrityMismatch { .. }));
}

#[test]
fn rejects_tampered_integrity() {
    let mut m = sample_manifest("snap-5");
    m.integrity = Some("f".repeat(64));
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::IntegrityMismatch { .. }));
}

#[test]
fn rejects_path_traversal() {
    let mut m = sample_manifest("snap-6");
    m.artifacts[0].path = "../escape.img".into();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::PathTraversal(_)));
}

#[test]
fn rejects_absolute_path() {
    let mut m = sample_manifest("snap-7");
    m.artifacts[0].path = "/etc/passwd".into();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::AbsolutePath(_)));
}

#[test]
fn rejects_duplicate_artifact_paths() {
    let mut m = sample_manifest("snap-8");
    m.artifacts[1].path = m.artifacts[0].path.clone();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::DuplicateArtifact(_)));
}

#[test]
fn rejects_incomplete_chunk_map() {
    let mut m = sample_manifest("snap-9");
    m = m.sealed().unwrap();
    // Drop a chunk so coverage < size.
    m.artifacts[0].chunks.pop();
    m = m.sealed().unwrap();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::IncompleteChunkMap { .. }));
}

#[test]
fn rejects_chunk_out_of_bounds() {
    let mut m = sample_manifest("snap-10");
    m.artifacts[1].chunks[0].length = 4096; // size is 1024
    m = m.sealed().unwrap();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::ChunkOutOfBounds { .. }));
}

#[test]
fn rejects_zero_length_chunk() {
    let mut m = sample_manifest("snap-11");
    m.artifacts[0].chunks[0].length = 0;
    m = m.sealed().unwrap();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::InvalidSpan { .. }));
}

#[test]
fn rejects_non_hex_chunk_id() {
    let mut m = sample_manifest("snap-12");
    m.artifacts[0].chunks[0].id = "not-a-hash".into();
    m = m.sealed().unwrap();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::DuplicateChunk(_)));
}

#[test]
fn rejects_missing_chunk_in_store() {
    let m = sample_manifest("snap-13");
    let mut available = available_for(&m);
    available.remove(&"a".repeat(64));
    let opts = ValidateOptions {
        check_integrity: true,
        available_chunks: Some(available),
    };
    let err = validate(&m, &opts).unwrap_err();
    assert!(matches!(err, FormatError::UnknownChunk { .. }));
}

#[test]
fn rejects_self_parent() {
    let mut m = sample_manifest("snap-14");
    m.parent = Some("snap-14".into());
    m = m.sealed().unwrap();
    let err = validate(&m, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::SelfParent(_)));
}

#[test]
fn snapshot_id_validation_rules() {
    assert!(validate_snapshot_id("snap_1.2-3").is_ok());
    assert!(validate_snapshot_id("").is_err());
    assert!(validate_snapshot_id("../x").is_err());
    assert!(validate_snapshot_id("a/b").is_err());
    assert!(validate_snapshot_id(&"x".repeat(200)).is_err());
}

#[test]
fn artifact_path_validation_rules() {
    assert!(validate_artifact_path("core-1.img").is_ok());
    assert!(validate_artifact_path("sub/dir/file.img").is_ok());
    assert!(validate_artifact_path("/abs").is_err());
    assert!(validate_artifact_path("../x").is_err());
    assert!(validate_artifact_path("./x").is_err());
    assert!(validate_artifact_path("a/../b").is_err());
}

#[test]
fn parent_cycle_detection() {
    // a -> b -> c -> a
    let edges = |id: &str| -> Option<String> {
        match id {
            "a" => Some("b".into()),
            "b" => Some("c".into()),
            "c" => Some("a".into()),
            _ => None,
        }
    };
    let cycle = snapforge_format::validate::detect_parent_cycle("a", edges);
    assert!(cycle.is_some());

    // No cycle: x -> y -> None
    let acyclic = |id: &str| -> Option<String> {
        match id {
            "x" => Some("y".into()),
            _ => None,
        }
    };
    assert!(snapforge_format::validate::detect_parent_cycle("x", acyclic).is_none());
}

// ---- Atomic publication ----

#[test]
fn publish_is_atomic_and_refuses_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let dest_parent = tmp.path().join("snapshots");

    let m = sample_manifest("snap-pub");
    snapforge_format::write_to_dir(&staging, &m).unwrap();

    let published = publish(&staging, &dest_parent, &ValidateOptions::default()).unwrap();
    assert!(published.join("manifest.json").exists());
    assert!(!staging.exists(), "staging should be renamed away");

    // Published manifest loads and validates.
    let loaded = load_validated(&published, &ValidateOptions::default()).unwrap();
    assert_eq!(loaded.manifest().snapshot_id, "snap-pub");

    // A second publication of the same id must be refused.
    let staging2 = tmp.path().join("staging2");
    snapforge_format::write_to_dir(&staging2, &m).unwrap();
    let err = publish(&staging2, &dest_parent, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::Publication(_)));
    // The original snapshot is intact.
    assert!(published.join("manifest.json").exists());
}

#[test]
fn publish_rejects_invalid_staged_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let dest_parent = tmp.path().join("snapshots");

    let mut m = sample_manifest("snap-bad");
    m.artifacts[0].path = "../evil".into();
    // Write without sealing so validation fails on traversal (before integrity).
    snapforge_format::write_to_dir(&staging, &m).unwrap();

    let err = publish(&staging, &dest_parent, &ValidateOptions::default()).unwrap_err();
    assert!(matches!(err, FormatError::PathTraversal(_)));
    // Destination must not exist.
    assert!(!dest_parent.join("snap-bad").exists());
    // Staging remains for inspection/cleanup by the caller.
    assert!(staging.exists());
    cleanup_staging(&staging);
    assert!(!staging.exists());
}

#[test]
fn load_reports_missing_manifest_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let err = load(tmp.path()).unwrap_err();
    assert!(matches!(err, FormatError::Io { .. }));
}
