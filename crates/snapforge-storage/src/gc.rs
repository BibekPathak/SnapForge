//! Storage garbage collection and chain compaction.
//!
//! The content-addressed store keeps every chunk it has ever written; nothing
//! removes chunks when a snapshot is deleted or a chain is flattened. This
//! module provides two explicit, opt-in maintenance operations:
//!
//! - [`gc`] performs a **mark-and-sweep** over the whole store. It marks every
//!   chunk referenced by a published snapshot manifest and removes CAS files
//!   that no manifest references.
//! - [`compact_chain`] **flattens** a snapshot chain to its tip: it rewrites the
//!   tip as a standalone root (clearing `parent`), removes the ancestor
//!   snapshots, and then sweeps the now-unreferenced chunks.
//!
//! Both are **non-destructive by default** (`dry_run`), because deleting
//! content-addressed data is irreversible. Both abort if any snapshot manifest
//! cannot be loaded and validated, so a manifest that failed to read can never
//! cause its chunks to be mistaken for garbage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use snapforge_format::{Manifest, ValidateOptions};
use tokio::sync::Semaphore;

use crate::cas::Cas;
use crate::error::{Result, StorageError};
use crate::incremental::{resolve_chain, Chain};
use crate::transfer::CancellationToken;

/// Options controlling garbage collection.
#[derive(Debug, Clone)]
pub struct GcOptions {
    /// Report only; do not delete anything.
    pub dry_run: bool,
    /// Maximum concurrent chunk deletions.
    pub concurrency: usize,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            dry_run: true,
            concurrency: 4,
            cancel: CancellationToken::new(),
        }
    }
}

impl GcOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(StorageError::Config("concurrency must be >= 1".into()));
        }
        Ok(())
    }
}

/// The result of a garbage-collection pass.
#[derive(Debug, Clone)]
pub struct GcOutcome {
    /// Number of published snapshots consulted.
    pub snapshots: u64,
    /// Number of distinct chunks referenced by those snapshots.
    pub referenced_chunks: u64,
    /// Number of chunk files present in the CAS.
    pub cas_chunks: u64,
    /// Number of unreferenced chunk files found.
    pub unreferenced_chunks: u64,
    /// Bytes covered by unreferenced chunk files.
    pub unreferenced_bytes: u64,
    /// Number of chunks actually removed (0 in a dry run).
    pub removed_chunks: u64,
    /// Bytes actually removed (0 in a dry run).
    pub removed_bytes: u64,
    /// Number of leftover temporary files removed (0 in a dry run).
    pub removed_temp_files: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// The result of a chain compaction.
#[derive(Debug, Clone)]
pub struct CompactOutcome {
    /// The tip snapshot that was flattened.
    pub tip: String,
    /// The resolved chain, root-first.
    pub chain: Vec<String>,
    /// Ancestor snapshots removed (or that would be removed in a dry run).
    pub removed_snapshots: Vec<String>,
    /// Chunks reclaimed (actual in an apply run, estimated for a dry run).
    pub reclaimed_chunks: u64,
    /// Bytes reclaimed (actual in an apply run, estimated for a dry run).
    pub reclaimed_bytes: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Options controlling chain compaction.
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// Report only; do not modify anything.
    pub dry_run: bool,
    /// Maximum concurrent chunk deletions during the sweep.
    pub concurrency: usize,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

impl Default for CompactOptions {
    fn default() -> Self {
        CompactOptions {
            dry_run: true,
            concurrency: 4,
            cancel: CancellationToken::new(),
        }
    }
}

/// A published snapshot: its directory name, manifest, and path.
type Loaded = (String, Manifest, PathBuf);

/// Load and validate every published snapshot under `snapshots_dir`.
///
/// Any snapshot that cannot be parsed or fails structural/integrity validation
/// aborts the operation: garbage collection must never delete chunks that a
/// snapshot we failed to read might reference.
fn load_all_manifests(snapshots_dir: &Path) -> Result<Vec<Loaded>> {
    let mut out = Vec::new();
    if !snapshots_dir.exists() {
        return Ok(out);
    }
    let rd = std::fs::read_dir(snapshots_dir)
        .map_err(|e| StorageError::io_at(snapshots_dir.to_path_buf(), e))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let manifest = snapforge_format::load_validated(&path, &ValidateOptions::default())
            .map_err(|e| {
                StorageError::Gc(format!(
                    "refusing to proceed: snapshot `{name}` is unreadable or invalid: {e}"
                ))
            })?
            .into_inner();
        out.push((name, manifest, path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Union of all chunk ids referenced by the given manifests, with their lengths.
fn live_chunks(manifests: &[Loaded]) -> BTreeMap<String, u64> {
    let mut live = BTreeMap::new();
    for (_, m, _) in manifests {
        for (id, len) in m.chunk_lengths() {
            live.insert(id, len);
        }
    }
    live
}

/// Garbage-collect unreferenced chunks from the CAS.
pub async fn gc(cas: Arc<Cas>, snapshots_dir: &Path, opts: GcOptions) -> Result<GcOutcome> {
    opts.validate()?;
    opts.cancel.check()?;
    let started = Instant::now();

    let manifests = load_all_manifests(snapshots_dir)?;
    let live = live_chunks(&manifests);

    let mut unreferenced: Vec<(String, u64)> = Vec::new();
    let mut temp_files: Vec<PathBuf> = Vec::new();
    let mut cas_chunks = 0u64;

    if cas.root().exists() {
        let rd = std::fs::read_dir(cas.root())
            .map_err(|e| StorageError::io_at(cas.root().to_path_buf(), e))?;
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                // Leftover temporary file from an interrupted write.
                temp_files.push(path);
                continue;
            }
            cas_chunks += 1;
            if !live.contains_key(&name) {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                unreferenced.push((name, len));
            }
        }
    }
    unreferenced.sort_by(|a, b| a.0.cmp(&b.0));

    let unreferenced_bytes: u64 = unreferenced.iter().map(|(_, len)| *len).sum();

    let mut outcome = GcOutcome {
        snapshots: manifests.len() as u64,
        referenced_chunks: live.len() as u64,
        cas_chunks,
        unreferenced_chunks: unreferenced.len() as u64,
        unreferenced_bytes,
        removed_chunks: 0,
        removed_bytes: 0,
        removed_temp_files: 0,
        dry_run: opts.dry_run,
        duration_ms: 0,
    };

    if !opts.dry_run {
        let (removed_chunks, removed_bytes) =
            delete_chunks(&cas, &unreferenced, opts.concurrency, &opts.cancel).await?;
        outcome.removed_chunks = removed_chunks;
        outcome.removed_bytes = removed_bytes;

        let mut removed_temp = 0u64;
        for path in temp_files {
            opts.cancel.check()?;
            match std::fs::remove_file(&path) {
                Ok(()) => removed_temp += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(StorageError::io_at(path, e)),
            }
        }
        outcome.removed_temp_files = removed_temp;
    }

    outcome.duration_ms = started.elapsed().as_millis() as u64;
    Ok(outcome)
}

/// Delete chunks with bounded concurrency. Returns `(count, bytes)`.
async fn delete_chunks(
    cas: &Cas,
    items: &[(String, u64)],
    concurrency: usize,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for (id, len) in items.iter().cloned() {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| StorageError::Join(e.to_string()))?;
        let cas = cas.clone();
        let cancel = cancel.clone();
        join_set.spawn(async move {
            cancel.check()?;
            let path = cas.chunk_path(&id);
            tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(StorageError::io_at(path, e)),
            })
            .await
            .map_err(|e| StorageError::Join(e.to_string()))??;
            drop(permit);
            Ok::<u64, StorageError>(len)
        });
    }

    let mut count = 0u64;
    let mut bytes = 0u64;
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(len)) => {
                count += 1;
                bytes += len;
            }
            Ok(Err(e)) => {
                cancel.cancel();
                return Err(e);
            }
            Err(e) => {
                cancel.cancel();
                return Err(StorageError::Join(e.to_string()));
            }
        }
    }
    Ok((count, bytes))
}

/// Flatten a snapshot chain to its tip, then reclaim the freed storage.
///
/// The tip's manifest is rewritten as a standalone root (`parent` cleared and
/// re-sealed) and the ancestor snapshot directories are removed. The operation
/// is refused if any snapshot outside the chain still depends on an ancestor
/// being removed, since that would break its lineage.
pub async fn compact_chain(
    cas: Arc<Cas>,
    snapshots_dir: &Path,
    tip: &str,
    opts: CompactOptions,
) -> Result<CompactOutcome> {
    if opts.concurrency == 0 {
        return Err(StorageError::Config("concurrency must be >= 1".into()));
    }
    opts.cancel.check()?;
    let started = Instant::now();

    let manifests = load_all_manifests(snapshots_dir)?;
    let by_id: BTreeMap<String, Manifest> = manifests
        .iter()
        .map(|(id, m, _)| (id.clone(), m.clone()))
        .collect();

    if !by_id.contains_key(tip) {
        return Err(StorageError::Gc(format!("snapshot `{tip}` not found")));
    }

    let chain: Chain = resolve_chain(tip, |id| by_id.get(id).cloned())?;
    let ancestor_ids: Vec<String> = chain.ids[..chain.ids.len().saturating_sub(1)].to_vec();
    let ancestor_set: BTreeSet<String> = ancestor_ids.iter().cloned().collect();

    // Safety: a snapshot outside the chain must not depend on an ancestor we
    // would remove.
    for (id, m, _) in &manifests {
        if chain.ids.contains(id) {
            continue;
        }
        if let Some(parent) = &m.parent {
            if ancestor_set.contains(parent) {
                return Err(StorageError::Gc(format!(
                    "cannot compact `{tip}`: snapshot `{id}` still depends on ancestor `{parent}`"
                )));
            }
        }
    }

    if ancestor_ids.is_empty() {
        // Already a root; nothing to flatten.
        return Ok(CompactOutcome {
            tip: tip.to_string(),
            chain: chain.ids,
            removed_snapshots: Vec::new(),
            reclaimed_chunks: 0,
            reclaimed_bytes: 0,
            dry_run: opts.dry_run,
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    // Chunks referenced only by the ancestors that will disappear.
    let before = live_chunks(&manifests);
    let mut after: BTreeMap<String, u64> = BTreeMap::new();
    for (id, m, _) in &manifests {
        if ancestor_set.contains(id) {
            continue;
        }
        for (cid, len) in m.chunk_lengths() {
            after.insert(cid, len);
        }
    }
    let mut reclaimable_chunks = 0u64;
    let mut reclaimable_bytes = 0u64;
    for (cid, len) in &before {
        if !after.contains_key(cid) {
            reclaimable_chunks += 1;
            reclaimable_bytes += *len;
        }
    }

    if opts.dry_run {
        return Ok(CompactOutcome {
            tip: tip.to_string(),
            chain: chain.ids,
            removed_snapshots: ancestor_ids,
            reclaimed_chunks: reclaimable_chunks,
            reclaimed_bytes: reclaimable_bytes,
            dry_run: true,
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    opts.cancel.check()?;

    // 1. Rewrite the tip as a root before removing anything, so it is never
    //    left pointing at a missing parent.
    let tip_manifest = by_id.get(tip).expect("tip checked above");
    let mut root = tip_manifest.clone();
    root.parent = None;
    let root = root.sealed().map_err(StorageError::Format)?;
    rewrite_manifest(snapshots_dir, tip, &root)?;

    // 2. Remove the ancestor snapshot directories.
    let mut removed_snapshots = Vec::new();
    for id in &ancestor_ids {
        opts.cancel.check()?;
        let dir = snapshots_dir.join(id);
        std::fs::remove_dir_all(&dir).map_err(|e| StorageError::io_at(dir.clone(), e))?;
        removed_snapshots.push(id.clone());
    }

    // 3. Sweep the now-unreferenced chunks.
    let gc_outcome = gc(
        cas,
        snapshots_dir,
        GcOptions {
            dry_run: false,
            concurrency: opts.concurrency,
            cancel: opts.cancel.clone(),
        },
    )
    .await?;

    Ok(CompactOutcome {
        tip: tip.to_string(),
        chain: chain.ids,
        removed_snapshots,
        reclaimed_chunks: gc_outcome.removed_chunks,
        reclaimed_bytes: gc_outcome.removed_bytes,
        dry_run: false,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Atomically replace a snapshot's `manifest.json`.
fn rewrite_manifest(snapshots_dir: &Path, id: &str, manifest: &Manifest) -> Result<()> {
    let dir = snapshots_dir.join(id);
    let bytes = snapforge_format::to_bytes(manifest)?;
    let tmp = dir.join(".manifest.json.new");
    std::fs::write(&tmp, &bytes).map_err(|e| StorageError::io_at(tmp.clone(), e))?;
    let final_path = dir.join(snapforge_format::MANIFEST_FILE);
    std::fs::rename(&tmp, &final_path).map_err(|e| StorageError::io_at(final_path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_options_reject_zero_concurrency() {
        let o = GcOptions {
            concurrency: 0,
            ..GcOptions::default()
        };
        assert!(o.validate().is_err());
    }

    #[test]
    fn live_chunks_unions_manifests() {
        // Build two minimal manifests that share one chunk id.
        let shared = "a".repeat(64);
        let only_a = "b".repeat(64);
        let only_b = "c".repeat(64);
        let a = manifest("a", &[(&shared, 10), (&only_a, 20)]);
        let b = manifest("b", &[(&shared, 10), (&only_b, 30)]);
        let loaded = vec![
            ("a".to_string(), a, PathBuf::from("/a")),
            ("b".to_string(), b, PathBuf::from("/b")),
        ];
        let live = live_chunks(&loaded);
        assert_eq!(live.len(), 3);
        assert_eq!(live[&shared], 10);
    }

    fn manifest(id: &str, chunks: &[(&str, u64)]) -> Manifest {
        use snapforge_format::{
            Artifact, ChunkRef, Compatibility, SnapshotStats, WorkloadInfo, FORMAT_VERSION,
        };
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
            parent: None,
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
                path: "a".into(),
                size,
                chunks: refs,
            }],
            stats: SnapshotStats::default(),
            integrity: None,
        }
        .sealed()
        .unwrap()
    }
}
