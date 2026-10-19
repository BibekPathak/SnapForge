//! Incremental snapshots (storage-layer, evidence-driven).
//!
//! # What this is — and is not
//!
//! This module implements **content-addressed reuse across compatible
//! captures**. Because chunks are addressed by their BLAKE3 hash, any bytes
//! that repeat between two snapshots map to the same chunk id and are stored
//! only once. A child snapshot records its `parent` id, which gives explicit
//! lineage and lets SnapForge validate a whole chain before anything is
//! restored.
//!
//! This is explicitly **not** a CRIU incremental checkpoint. CRIU produces a
//! self-contained set of images for a given dump; it does not support merging a
//! "parent" image set with a "child" image set to reconstruct arbitrary process
//! state. SnapForge therefore does not attempt to synthesize a valid CRIU image
//! by diffing memory. Instead:
//!
//! - Each snapshot is a complete, self-describing manifest over the shared CAS.
//! - Reuse is achieved by not duplicating identical chunks (deduplication).
//! - The `parent` link records lineage and is enforced to be acyclic and
//!   resolvable; it is *not* a claim that restore can start from the parent.
//!
//! The measured benefit is therefore a storage/packaging benefit (fewer new
//! bytes stored for a second compatible capture), which the tests quantify.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use snapforge_format::Manifest;

use crate::cas::Cas;
use crate::error::{Result, StorageError};

/// Measured reuse between a child snapshot and its parent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReuseReport {
    /// Snapshot id of the child.
    pub child: String,
    /// Snapshot id of the parent.
    pub parent: String,
    /// Total distinct chunks referenced by the child.
    pub child_chunks: u64,
    /// Child chunks whose id is also referenced by the parent (reused).
    pub reused_chunks: u64,
    /// Child chunks not present in the parent (newly introduced).
    pub new_chunks: u64,
    /// Child logical bytes covered by reused chunks.
    pub reused_bytes: u64,
    /// Child logical bytes covered by new chunks.
    pub new_bytes: u64,
    /// Reuse ratio by bytes: reused_bytes / total_bytes (0.0..=1.0).
    pub reuse_ratio: f64,
}

impl ReuseReport {
    /// Compute reuse of `child` relative to `parent`.
    pub fn compute(child: &Manifest, parent: &Manifest) -> ReuseReport {
        let child_chunks = child.chunk_lengths();
        let parent_chunks = parent.chunk_lengths();

        let mut reused_chunks = 0u64;
        let mut new_chunks = 0u64;
        let mut reused_bytes = 0u64;
        let mut new_bytes = 0u64;

        for (id, len) in &child_chunks {
            if parent_chunks.contains_key(id) {
                reused_chunks += 1;
                reused_bytes += *len;
            } else {
                new_chunks += 1;
                new_bytes += *len;
            }
        }

        let total = reused_bytes + new_bytes;
        let reuse_ratio = if total == 0 {
            1.0
        } else {
            reused_bytes as f64 / total as f64
        };

        ReuseReport {
            child: child.snapshot_id.clone(),
            parent: parent.snapshot_id.clone(),
            child_chunks: child_chunks.len() as u64,
            reused_chunks,
            new_chunks,
            reused_bytes,
            new_bytes,
            reuse_ratio,
        }
    }
}

/// A resolved parent chain, root-first, ending at the requested snapshot.
#[derive(Debug, Clone)]
pub struct Chain {
    /// Snapshot ids from root to tip (inclusive), each parent preceding child.
    pub ids: Vec<String>,
}

impl Chain {
    /// Depth of the chain (number of snapshots).
    pub fn depth(&self) -> usize {
        self.ids.len()
    }
}

/// Resolve and validate the parent chain ending at `id`.
///
/// `resolve` maps a snapshot id to its manifest. Returns an error if any link
/// is missing, if a self-reference is found, or if a cycle is detected.
pub fn resolve_chain<F>(id: &str, mut resolve: F) -> Result<Chain>
where
    F: FnMut(&str) -> Option<Manifest>,
{
    let mut ids: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut current = id.to_string();

    // Bounded walk to defend against adversarial input.
    for _ in 0..4096 {
        if !seen.insert(current.clone()) {
            return Err(StorageError::Other(format!(
                "parent chain cycle detected at `{current}`"
            )));
        }
        let manifest = resolve(&current).ok_or_else(|| {
            StorageError::Other(format!(
                "snapshot `{current}` referenced by the chain is missing"
            ))
        })?;
        ids.push(current.clone());
        match manifest.parent {
            Some(p) => current = p,
            None => {
                ids.reverse();
                return Ok(Chain { ids });
            }
        }
    }
    Err(StorageError::Other(
        "parent chain exceeds maximum depth".into(),
    ))
}

/// Verify that every chunk referenced by the snapshot (and, transitively, by
/// its parent chain) is present and valid in the CAS.
///
/// Returns the total number of distinct chunks verified across the chain.
pub fn verify_chain(cas: &Cas, manifests: &[Manifest]) -> Result<u64> {
    let mut verified: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for m in manifests {
        for artifact in &m.artifacts {
            for chunk in &artifact.chunks {
                if verified.insert(chunk.id.clone()) {
                    cas.verify(&chunk.id)?;
                }
            }
        }
    }
    Ok(verified.len() as u64)
}

/// Compact a chain by computing which chunks could be dropped if only the tip
/// were retained.
///
/// This is an *analysis* helper: it reports the byte savings of "flattening"
/// the chain to the tip snapshot alone (i.e. dropping parent manifests), which
/// is only safe when the parent snapshots are not needed independently. SnapForge
/// does not perform compaction automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionAnalysis {
    pub snapshots: usize,
    /// Distinct chunks referenced by the tip.
    pub tip_chunks: u64,
    /// Distinct chunks referenced by any snapshot in the chain.
    pub chain_chunks: u64,
    /// Chunks only reachable via non-tip snapshots.
    pub chunks_droppable: u64,
    /// Bytes only reachable via non-tip snapshots.
    pub bytes_droppable: u64,
}

/// Analyze the storage that could be reclaimed by flattening a chain to its tip.
pub fn analyze_compaction(chain: &[Manifest]) -> CompactionAnalysis {
    let Some(tip) = chain.last() else {
        return CompactionAnalysis {
            snapshots: 0,
            tip_chunks: 0,
            chain_chunks: 0,
            chunks_droppable: 0,
            bytes_droppable: 0,
        };
    };

    let tip_chunks = tip.chunk_lengths();
    let mut chain_chunks: BTreeMap<String, u64> = BTreeMap::new();
    for m in chain {
        for (id, len) in m.chunk_lengths() {
            chain_chunks.entry(id).or_insert(len);
        }
    }

    let mut chunks_droppable = 0u64;
    let mut bytes_droppable = 0u64;
    for (id, len) in &chain_chunks {
        if !tip_chunks.contains_key(id) {
            chunks_droppable += 1;
            bytes_droppable += *len;
        }
    }

    CompactionAnalysis {
        snapshots: chain.len(),
        tip_chunks: tip_chunks.len() as u64,
        chain_chunks: chain_chunks.len() as u64,
        chunks_droppable,
        bytes_droppable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snapforge_format::{
        Artifact, ChunkRef, Compatibility, SnapshotStats, WorkloadInfo, FORMAT_VERSION,
    };

    fn manifest(id: &str, parent: Option<&str>, chunks: &[(&str, u64)]) -> Manifest {
        let refs: Vec<ChunkRef> = chunks
            .iter()
            .map(|(id, len)| ChunkRef {
                id: id.to_string(),
                length: *len,
            })
            .collect();
        let size = refs.iter().map(|c| c.length).sum();
        Manifest {
            format_version: FORMAT_VERSION,
            snapshot_id: id.into(),
            parent: parent.map(String::from),
            workload: WorkloadInfo {
                id: "t".into(),
                description: None,
            },
            captured_at: "2026-01-01T00:00:00Z".into(),
            compatibility: Compatibility {
                arch: "x86_64".into(),
                kernel_release: "t".into(),
                criu_version: None,
                criu_image_version: None,
            },
            artifacts: vec![Artifact {
                path: "a.img".into(),
                size,
                chunks: refs,
            }],
            stats: SnapshotStats::default(),
            integrity: None,
        }
        .sealed()
        .unwrap()
    }

    #[test]
    fn reuse_report_counts_correctly() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let parent = manifest("p", None, &[(&a, 100), (&b, 100)]);
        // Child shares `a` and `b`, adds `c`.
        let child = manifest("c", Some("p"), &[(&a, 100), (&b, 100), (&c, 50)]);
        let report = ReuseReport::compute(&child, &parent);
        assert_eq!(report.child_chunks, 3);
        assert_eq!(report.reused_chunks, 2);
        assert_eq!(report.new_chunks, 1);
        assert_eq!(report.reused_bytes, 200);
        assert_eq!(report.new_bytes, 50);
        assert!((report.reuse_ratio - 0.8).abs() < 1e-9);
    }

    #[test]
    fn resolve_chain_orders_root_first() {
        let a = manifest("a", None, &[(&"a".repeat(64), 10)]);
        let b = manifest("b", Some("a"), &[(&"a".repeat(64), 10)]);
        let c = manifest("c", Some("b"), &[(&"a".repeat(64), 10)]);
        let store = [a, b, c];
        let chain = resolve_chain("c", |id| {
            store.iter().find(|m| m.snapshot_id == id).cloned()
        })
        .unwrap();
        assert_eq!(chain.ids, vec!["a", "b", "c"]);
        assert_eq!(chain.depth(), 3);
    }

    #[test]
    fn resolve_chain_detects_cycle() {
        let a = manifest("a", Some("b"), &[(&"a".repeat(64), 10)]);
        let b = manifest("b", Some("a"), &[(&"a".repeat(64), 10)]);
        let store = [a, b];
        let err = resolve_chain("a", |id| {
            store.iter().find(|m| m.snapshot_id == id).cloned()
        })
        .unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn resolve_chain_detects_missing_parent() {
        let a = manifest("a", Some("ghost"), &[(&"a".repeat(64), 10)]);
        let err =
            resolve_chain("a", |id| if id == "a" { Some(a.clone()) } else { None }).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn compaction_analysis_reports_droppable() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let parent = manifest("p", None, &[(&a, 100), (&b, 100)]);
        let child = manifest("c", Some("p"), &[(&a, 100)]);
        let analysis = analyze_compaction(&[parent, child]);
        assert_eq!(analysis.snapshots, 2);
        assert_eq!(analysis.tip_chunks, 1);
        assert_eq!(analysis.chain_chunks, 2);
        assert_eq!(analysis.chunks_droppable, 1);
        assert_eq!(analysis.bytes_droppable, 100);
    }
}
