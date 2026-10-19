//! Snapshot manifest data model.
//!
//! The manifest is stored *separately* from artifact data. It is immutable
//! once published. All fields are serialized with Serde under a stable,
//! versioned schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current manifest format version.
pub const FORMAT_VERSION: u32 = 1;

/// A content-addressed chunk descriptor.
///
/// `id` is the hex-encoded BLAKE3 hash of the chunk's bytes. This makes chunk
/// identity intrinsic: identical bytes have identical ids, enabling
/// deduplication and integrity verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    /// Hex-encoded BLAKE3 digest.
    pub id: String,
    /// Chunk length in bytes.
    pub length: u64,
}

/// A logical artifact (for example a CRIU image file) stored as an ordered
/// sequence of chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Relative path within the snapshot. Must be a safe, normalized relative
    /// path (no `..`, no absolute paths).
    pub path: String,
    /// Total size in bytes (must equal the sum of chunk lengths).
    pub size: u64,
    /// Ordered chunks composing this artifact.
    pub chunks: Vec<ChunkRef>,
}

impl Artifact {
    /// Verify that the chunk map covers exactly `size` bytes.
    pub fn chunk_coverage(&self) -> u64 {
        self.chunks.iter().map(|c| c.length).sum()
    }
}

/// Where a snapshot's artifacts came from. Informational unless a restore
/// compatibility check depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadInfo {
    /// Stable workload identifier (for example `cpu-memory`).
    pub id: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Compatibility metadata. These fields describe the environment that must be
/// reproduced for a restore to be valid; they are **required** for restore
/// compatibility checks, not merely informational.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compatibility {
    /// CPU architecture (for example `x86_64`).
    pub arch: String,
    /// Source kernel release string.
    pub kernel_release: String,
    /// Source CRIU version string, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criu_version: Option<String>,
    /// CRIU image format version this snapshot was produced with, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criu_image_version: Option<String>,
}

/// Aggregate packaging statistics. Informational.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapshotStats {
    /// Total bytes across all artifacts before chunking.
    pub total_bytes: u64,
    /// Number of distinct chunks after deduplication.
    pub distinct_chunks: u64,
    /// Number of chunk references before deduplication.
    pub total_chunk_refs: u64,
    /// Bytes saved by deduplicating identical chunks.
    pub dedup_saved_bytes: u64,
    /// Wall-clock capture duration reported by the engine, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_duration_ms: Option<u64>,
    /// Wall-clock packaging duration, if measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packaging_duration_ms: Option<u64>,
    /// For incremental snapshots: distinct child chunks also present in the
    /// parent. Informational; computed at packaging time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_chunks: Option<u64>,
    /// For incremental snapshots: child chunks not present in the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_chunks: Option<u64>,
    /// For incremental snapshots: bytes covered by reused chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_bytes: Option<u64>,
}

/// The complete, immutable snapshot manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest schema version. Required for forward/backward compatibility.
    pub format_version: u32,
    /// Unique snapshot identifier.
    pub snapshot_id: String,
    /// Optional parent snapshot id (for incremental snapshots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Source workload information.
    pub workload: WorkloadInfo,
    /// RFC 3339 capture timestamp (string to avoid a time dependency here).
    pub captured_at: String,
    /// Restore-compatibility metadata.
    pub compatibility: Compatibility,
    /// Artifacts in the snapshot.
    pub artifacts: Vec<Artifact>,
    /// Packaging statistics.
    #[serde(default)]
    pub stats: SnapshotStats,
    /// Hex-encoded BLAKE3 integrity digest over the canonical manifest body
    /// (see [`Manifest::integrity_digest`]). Set on publish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
}

impl Manifest {
    /// Compute the integrity digest over a canonical view of the manifest,
    /// excluding the `integrity` field itself.
    ///
    /// The canonical view is JSON-serialized with sorted keys. Because struct
    /// field order is fixed and the `integrity` field is omitted when `None`,
    /// this is deterministic for a given manifest body.
    pub fn integrity_digest(&self) -> crate::Result<String> {
        let mut clone = self.clone();
        clone.integrity = None;
        let bytes = serde_json::to_vec(&clone)?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    /// Set and return the integrity digest.
    pub fn sealed(mut self) -> crate::Result<Self> {
        let digest = self.integrity_digest()?;
        self.integrity = Some(digest);
        Ok(self)
    }

    /// Total artifact bytes declared by the manifest.
    pub fn total_bytes(&self) -> u64 {
        self.artifacts.iter().map(|a| a.size).sum()
    }

    /// Return a map of chunk id to a representative reference. Errors if the
    /// same id appears with conflicting lengths.
    pub fn chunk_lengths(&self) -> BTreeMap<String, u64> {
        let mut map = BTreeMap::new();
        for a in &self.artifacts {
            for c in &a.chunks {
                map.entry(c.id.clone()).or_insert(c.length);
            }
        }
        map
    }
}
