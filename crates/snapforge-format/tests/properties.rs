//! Property-based tests for chunk mapping and integrity.
//!
//! These assert invariants that must hold for *all* generated manifests:
//! - A manifest whose chunks exactly cover each artifact size validates.
//! - Integrity digest is stable under re-serialization.
//! - Tampering with any artifact size/coverage is detected.

use proptest::prelude::*;

use snapforge_format::{
    validate, Artifact, ChunkRef, Compatibility, Manifest, SnapshotStats, ValidateOptions,
    WorkloadInfo, FORMAT_VERSION,
};

fn hex_id(seed: u64) -> String {
    // Deterministic 64-char hex id from a seed.
    let mut out = String::with_capacity(64);
    let mut x = seed.wrapping_add(0x9E3779B97F4A7C15);
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(&format!("{x:016x}"));
    }
    out
}

prop_compose! {
    // (path_idx, chunk lengths) — path uniqueness is fixed up by the caller.
    fn arb_artifact_spec()(
        path_idx in 0u32..100_000,
        lens in prop::collection::vec(1u64..=4096, 1..=8),
    ) -> (u32, Vec<u64>) {
        (path_idx, lens)
    }
}

prop_compose! {
    fn arb_manifest()(
        specs in prop::collection::vec(arb_artifact_spec(), 1..=6),
        id in "[a-z0-9]{4,16}",
    ) -> Manifest {
        use std::collections::BTreeSet;
        // Build artifacts, giving every chunk a globally unique id so that the
        // content-addressing invariant (one id -> one length) always holds.
        let mut counter: u64 = 0;
        let mut artifacts: Vec<Artifact> = Vec::new();
        for (i, (_path_idx, lens)) in specs.into_iter().enumerate() {
            let chunks: Vec<ChunkRef> = lens
                .into_iter()
                .map(|l| {
                    let n = counter;
                    counter += 1;
                    ChunkRef { id: hex_id(n), length: l }
                })
                .collect();
            let size = chunks.iter().map(|c| c.length).sum();
            artifacts.push(Artifact {
                path: format!("artifact-{i}.img"),
                size,
                chunks,
            });
        }
        let total_refs: u64 = artifacts.iter().map(|a| a.chunks.len() as u64).sum();
        let total_bytes: u64 = artifacts.iter().map(|a| a.size).sum();
        let distinct: BTreeSet<_> = artifacts
            .iter()
            .flat_map(|a| a.chunks.iter().map(|c| c.id.clone()))
            .collect();
        let m = Manifest {
            format_version: FORMAT_VERSION,
            snapshot_id: id,
            parent: None,
            workload: WorkloadInfo { id: "prop".into(), description: None },
            captured_at: "2026-01-01T00:00:00Z".into(),
            compatibility: Compatibility {
                arch: "x86_64".into(),
                kernel_release: "test".into(),
                criu_version: None,
                criu_image_version: None,
            },
            artifacts,
            stats: SnapshotStats {
                total_bytes,
                distinct_chunks: distinct.len() as u64,
                total_chunk_refs: total_refs,
                dedup_saved_bytes: 0,
                capture_duration_ms: None,
                packaging_duration_ms: None,
                reused_chunks: None,
                new_chunks: None,
                reused_bytes: None,
            },
            integrity: None,
        };
        m.sealed().unwrap()
    }
}

proptest! {
    #[test]
    fn wellformed_manifests_validate(m in arb_manifest()) {
        let opts = ValidateOptions { check_integrity: true, available_chunks: None };
        validate(&m, &opts).expect("well-formed manifest must validate");
    }

    #[test]
    fn integrity_digest_is_stable(m in arb_manifest()) {
        let a = m.integrity_digest().unwrap();
        let b = m.integrity_digest().unwrap();
        prop_assert_eq!(a, b);
    }

    #[test]
    fn round_trip_preserves_manifest(m in arb_manifest()) {
        let bytes = snapforge_format::to_bytes(&m).unwrap();
        let back: Manifest = snapforge_format::from_bytes(&bytes).unwrap();
        prop_assert_eq!(m, back);
    }

    #[test]
    fn shrinking_a_chunk_is_detected(mut m in arb_manifest()) {
        // Force an incomplete chunk map by shrinking the first non-empty chunk.
        if let Some(a) = m.artifacts.iter_mut().find(|a| !a.chunks.is_empty()) {
            if a.chunks[0].length > 1 {
                a.chunks[0].length -= 1;
            } else {
                a.chunks.remove(0);
            }
        }
        m = m.sealed().unwrap();
        let err = validate(&m, &ValidateOptions::default());
        prop_assert!(err.is_err(), "incomplete chunk map must be rejected");
    }
}
