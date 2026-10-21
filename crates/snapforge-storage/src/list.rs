//! Snapshot inventory: enumerate and summarize published snapshots.
//!
//! This is the read-only counterpart to [`crate::gc`]. It answers "what is in
//! this store?" without mutating anything: every published snapshot with its
//! size, lineage, and the storage that flattening its chain would reclaim.
//!
//! A snapshot that cannot be loaded and validated is reported with an error
//! rather than skipped, so an operator sees corrupt state instead of a store
//! that merely looks smaller than it is.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use snapforge_format::{Manifest, ValidateOptions};

use crate::error::{Result, StorageError};

/// One row of a snapshot inventory.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    /// Snapshot id (its directory name).
    pub id: String,
    /// Snapshot directory path.
    pub path: PathBuf,
    /// Parent snapshot id, if any.
    pub parent: Option<String>,
    /// Number of snapshots from root to this one (inclusive). 1 for a root.
    pub chain_depth: usize,
    /// Snapshot ids in this snapshot's chain, root-first.
    pub chain: Vec<String>,
    /// The tip of this snapshot's chain (may be this snapshot itself).
    pub chain_tip: String,
    /// Whether this snapshot is the tip of its chain.
    pub is_tip: bool,
    /// Number of artifacts.
    pub artifacts: u64,
    /// Total logical bytes (sum of artifact sizes).
    pub total_bytes: u64,
    /// Distinct chunks referenced by this snapshot.
    pub distinct_chunks: u64,
    /// Chunks physically stored under the CAS.
    pub cas_chunks: u64,
    /// Bytes physically stored under the CAS.
    pub cas_bytes: u64,
    /// Chunks reclaimable by flattening this snapshot's chain to its tip.
    pub reclaimable_chunks: u64,
    /// Bytes reclaimable by flattening this snapshot's chain to its tip.
    pub reclaimable_bytes: u64,
    /// True if the manifest validated structurally and its integrity digest
    /// matched.
    pub valid: bool,
    /// Validation error, when `valid` is false.
    pub error: Option<String>,
}

/// A snapshot inventory plus store-wide totals.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotList {
    /// Snapshots, sorted by id.
    pub snapshots: Vec<SnapshotSummary>,
    /// Number of published snapshots.
    pub count: u64,
    /// Sum of every snapshot's logical bytes.
    pub total_logical_bytes: u64,
    /// Chunks physically present under the CAS (including unreferenced ones).
    pub cas_chunks: u64,
    /// Bytes physically present under the CAS (including unreferenced ones).
    pub cas_bytes: u64,
    /// Distinct chunks referenced by at least one snapshot.
    pub referenced_chunks: u64,
    /// Chunks present under the CAS but referenced by no snapshot.
    pub unreferenced_chunks: u64,
    /// Bytes of the unreferenced chunks.
    pub unreferenced_bytes: u64,
}

/// A loaded snapshot row: id, optional manifest (None when invalid), directory,
/// and the validation error when the manifest failed to load.
type Loaded = (String, Option<Manifest>, PathBuf, Option<String>);

/// Enumerate every published snapshot under `snapshots_dir` with a summary.
///
/// A snapshot that fails to load or validate is reported with `valid = false`
/// and an `error` string; the rest of the inventory is still produced.
pub fn list_snapshots(snapshots_dir: &Path) -> Result<SnapshotList> {
    let mut loaded: Vec<Loaded> = Vec::new();
    if snapshots_dir.exists() {
        let rd = std::fs::read_dir(snapshots_dir)
            .map_err(|e| StorageError::io_at(snapshots_dir.to_path_buf(), e))?;
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            match snapforge_format::load_validated(&path, &ValidateOptions::default()) {
                Ok(v) => loaded.push((name, Some(v.into_inner()), path, None)),
                Err(e) => loaded.push((name, None, path, Some(e.to_string()))),
            }
        }
    }
    loaded.sort_by(|a, b| a.0.cmp(&b.0));

    // Resolve chains from the manifests that loaded cleanly.
    let manifests: BTreeMap<String, &Manifest> = loaded
        .iter()
        .filter_map(|(id, m, _, _)| m.as_ref().map(|m| (id.clone(), m)))
        .collect();

    // Map each snapshot id to the tip of the chain it belongs to, so every
    // member of a chain reports the same reclaimable figures. Process the
    // longest chains first: a snapshot that is itself a root is also a member
    // of longer chains rooted below it, and the outermost tip wins.
    let mut chain_by_id: Vec<(String, Vec<String>)> = manifests
        .keys()
        .map(|id| (id.clone(), resolve_chain_ids(id, &manifests)))
        .collect();
    chain_by_id.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    let mut tip_of: BTreeMap<String, String> = BTreeMap::new();
    for (_, chain) in &chain_by_id {
        if let Some(tip) = chain.last() {
            for member in chain {
                tip_of.entry(member.clone()).or_insert_with(|| tip.clone());
            }
        }
    }

    // Group chain members by tip for compaction analysis.
    let mut chain_members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (member, tip) in &tip_of {
        chain_members
            .entry(tip.clone())
            .or_default()
            .push(member.clone());
    }
    let mut reclaimable: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (tip, members) in chain_members {
        let Some(tip_manifest) = manifests.get(&tip) else {
            continue;
        };
        let tip_chunks = tip_manifest.chunk_lengths();
        let mut chain_chunks: BTreeMap<String, u64> = BTreeMap::new();
        for member in &members {
            if let Some(m) = manifests.get(member) {
                for (chunk, len) in m.chunk_lengths() {
                    chain_chunks.entry(chunk).or_insert(len);
                }
            }
        }
        let mut chunks = 0u64;
        let mut bytes = 0u64;
        for (chunk, len) in &chain_chunks {
            if !tip_chunks.contains_key(chunk) {
                chunks += 1;
                bytes += *len;
            }
        }
        reclaimable.insert(tip, (chunks, bytes));
    }

    // Physical CAS contents.
    let mut cas_chunks = 0u64;
    let mut cas_bytes = 0u64;
    let cas_dir = snapshots_dir
        .parent()
        .map(|root| root.join("cas"))
        .unwrap_or_default();
    let mut present_ids: BTreeMap<String, u64> = BTreeMap::new();
    if cas_dir.is_dir() {
        let rd =
            std::fs::read_dir(&cas_dir).map_err(|e| StorageError::io_at(cas_dir.clone(), e))?;
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            cas_chunks += 1;
            cas_bytes += size;
            present_ids.insert(name, size);
        }
    }

    let mut summarized: Vec<SnapshotSummary> = Vec::new();
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut total_logical = 0u64;
    for (id, manifest, path, error) in &loaded {
        let (valid, artifacts, total_bytes, distinct_chunks) = match manifest {
            Some(m) => {
                let chunks = m.chunk_lengths();
                for chunk in chunks.keys() {
                    referenced.insert(chunk.clone());
                }
                (
                    true,
                    m.artifacts.len() as u64,
                    m.total_bytes(),
                    chunks.len() as u64,
                )
            }
            None => (false, 0, 0, 0),
        };
        total_logical += total_bytes;

        let chain = resolve_chain_ids(id, &manifests);
        let tip = tip_of.get(id).cloned();
        let (reclaimable_chunks, reclaimable_bytes) = tip
            .as_ref()
            .and_then(|t| reclaimable.get(t))
            .copied()
            .unwrap_or((0, 0));

        summarized.push(SnapshotSummary {
            id: id.clone(),
            path: path.clone(),
            parent: manifest.as_ref().and_then(|m| m.parent.clone()),
            chain_depth: chain.len(),
            chain_tip: tip.clone().unwrap_or_else(|| id.clone()),
            is_tip: tip.as_deref() == Some(id.as_str()),
            chain,
            artifacts,
            total_bytes,
            distinct_chunks,
            cas_chunks,
            cas_bytes,
            reclaimable_chunks,
            reclaimable_bytes,
            valid,
            error: error.clone(),
        });
    }

    let unreferenced_bytes = present_ids
        .iter()
        .filter(|(id, _)| !referenced.contains(*id))
        .map(|(_, size)| *size)
        .sum();

    Ok(SnapshotList {
        count: summarized.len() as u64,
        snapshots: summarized,
        total_logical_bytes: total_logical,
        cas_chunks,
        cas_bytes,
        referenced_chunks: referenced.len() as u64,
        unreferenced_chunks: cas_chunks.saturating_sub(referenced.len() as u64),
        unreferenced_bytes,
    })
}

/// Walk `id`'s parent pointers root-first, tolerating a broken chain.
///
/// Missing links stop the walk rather than failing: `list` is a diagnostic, so
/// it reports what it can instead of refusing to list anything.
fn resolve_chain_ids(id: &str, manifests: &BTreeMap<String, &Manifest>) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut ids: Vec<String> = Vec::new();
    let mut current = id.to_string();
    for _ in 0..4096 {
        if !seen.insert(current.clone()) {
            break;
        }
        let Some(manifest) = manifests.get(&current) else {
            break;
        };
        ids.push(current.clone());
        match &manifest.parent {
            Some(p) => current = p.clone(),
            None => break,
        }
    }
    ids.reverse();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use snapforge_format::{Compatibility, WorkloadInfo, FORMAT_VERSION};

    fn manifest(id: &str, parent: Option<&str>) -> Manifest {
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
            artifacts: Vec::new(),
            stats: Default::default(),
            integrity: None,
        }
    }

    #[test]
    fn empty_directory_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let list = list_snapshots(&tmp.path().join("snapshots")).unwrap();
        assert_eq!(list.count, 0);
        assert!(list.snapshots.is_empty());
    }

    #[test]
    fn a_missing_parent_stops_the_walk_without_failing() {
        let leaf = manifest("leaf", Some("root"));
        let mut manifests: BTreeMap<String, &Manifest> = BTreeMap::new();
        manifests.insert("leaf".into(), &leaf);
        assert_eq!(resolve_chain_ids("leaf", &manifests), vec!["leaf"]);
    }

    #[test]
    fn unknown_id_resolves_to_an_empty_chain() {
        let manifests: BTreeMap<String, &Manifest> = BTreeMap::new();
        assert!(resolve_chain_ids("missing", &manifests).is_empty());
    }

    #[test]
    fn a_cycle_is_broken_rather_than_looping() {
        let a = manifest("a", Some("b"));
        let b = manifest("b", Some("a"));
        let mut manifests: BTreeMap<String, &Manifest> = BTreeMap::new();
        manifests.insert("a".into(), &a);
        manifests.insert("b".into(), &b);
        // Either rotation is acceptable; the point is that it terminates.
        let chain = resolve_chain_ids("a", &manifests);
        assert_eq!(chain.len(), 2);
    }
}
