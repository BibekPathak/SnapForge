//! Create a small, valid, published snapshot fixture.
//!
//! This is a development/verification helper: it builds a manifest whose
//! chunks are backed by real files in a content-addressed store, then publishes
//! the snapshot atomically. It does **not** perform any CRIU operation.
//!
//! Usage: `cargo run -p snapforge-format --example make_fixture -- <store-dir>`

use std::collections::BTreeMap;
use std::path::PathBuf;

use snapforge_format::{
    publish, to_bytes, Artifact, ChunkRef, Compatibility, Manifest, SnapshotStats, ValidateOptions,
    WorkloadInfo, FORMAT_VERSION,
};

fn main() -> anyhow::Result<()> {
    let store: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/snapforge".into())
        .into();

    let cas = store.join("cas");
    let snapshots = store.join("snapshots");
    let staging = store.join("staging").join("fixture-1");
    std::fs::create_dir_all(&cas)?;
    std::fs::create_dir_all(&snapshots)?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    // Build chunk data with a deterministic pattern, writing content-addressed
    // files into the CAS store.
    let mut chunk_lengths: BTreeMap<String, u64> = BTreeMap::new();
    let make_chunk = |seed: u64, len: usize| -> String {
        let mut bytes = vec![0u8; len];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((seed.wrapping_add(i as u64)).wrapping_mul(2654435761) >> 13) as u8;
        }
        let id = blake3::hash(&bytes).to_hex().to_string();
        let path = cas.join(&id);
        if !path.exists() {
            std::fs::write(&path, &bytes).unwrap();
        }
        id
    };

    // Artifact A: two chunks (256 KiB + 256 KiB).
    let a0 = make_chunk(1, 256 * 1024);
    let a1 = make_chunk(2, 256 * 1024);
    let artifact_a = Artifact {
        path: "core-1.img".into(),
        size: 512 * 1024,
        chunks: vec![
            ChunkRef {
                id: a0.clone(),
                length: 256 * 1024,
            },
            ChunkRef {
                id: a1.clone(),
                length: 256 * 1024,
            },
        ],
    };

    // Artifact B: reuses a0 (dedup) plus a new chunk.
    let b0 = make_chunk(3, 128 * 1024);
    let artifact_b = Artifact {
        path: "mm-1.img".into(),
        size: 256 * 1024 + 128 * 1024,
        chunks: vec![
            ChunkRef {
                id: a0.clone(),
                length: 256 * 1024,
            },
            ChunkRef {
                id: b0.clone(),
                length: 128 * 1024,
            },
        ],
    };

    chunk_lengths.insert(a0.clone(), 256 * 1024);
    chunk_lengths.insert(a1.clone(), 256 * 1024);
    chunk_lengths.insert(b0.clone(), 128 * 1024);

    let total_refs = 4u64;
    let distinct = chunk_lengths.len() as u64;
    let total_bytes = artifact_a.size + artifact_b.size;

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        snapshot_id: "fixture-1".into(),
        parent: None,
        workload: WorkloadInfo {
            id: "fixture".into(),
            description: Some("format fixture (not a real checkpoint)".into()),
        },
        captured_at: "2026-01-01T00:00:00Z".into(),
        compatibility: Compatibility {
            arch: std::env::consts::ARCH.into(),
            kernel_release: "fixture".into(),
            criu_version: None,
            criu_image_version: None,
        },
        artifacts: vec![artifact_a, artifact_b],
        stats: SnapshotStats {
            total_bytes,
            distinct_chunks: distinct,
            total_chunk_refs: total_refs,
            dedup_saved_bytes: 256 * 1024,
            capture_duration_ms: None,
            packaging_duration_ms: None,
        },
        integrity: None,
    }
    .sealed()?;

    // Stage manifest, then publish atomically.
    snapforge_format::write_to_dir(&staging, &manifest)?;
    let dest = publish(&staging, &snapshots, &ValidateOptions::default())?;

    // Sanity: re-load and validate.
    let reloaded = snapforge_format::load_validated(&dest, &ValidateOptions::default())?;
    let bytes = to_bytes(reloaded.manifest())?;
    println!(
        "published fixture at {} ({} manifest bytes)",
        dest.display(),
        bytes.len()
    );
    Ok(())
}
