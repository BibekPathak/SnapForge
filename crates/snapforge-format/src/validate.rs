//! Strict manifest validation.
//!
//! Manifests and artifact directories are treated as potentially malformed or
//! untrusted input. Validation rejects: unsupported versions, path traversal,
//! absolute paths, duplicate artifacts/chunks, invalid offsets/lengths,
//! incomplete chunk maps, count mismatches, self/cyclic parents, and (when
//! enabled) integrity-digest mismatches.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use crate::error::{FormatError, Result};
use crate::manifest::{Manifest, FORMAT_VERSION};

/// Controls how much validation to perform.
#[derive(Debug, Clone)]
pub struct ValidateOptions {
    /// Require a present, correct integrity digest.
    pub check_integrity: bool,
    /// Verify that every referenced chunk exists in the provided set of
    /// available chunk ids (usually the content-addressed store contents).
    pub available_chunks: Option<BTreeSet<String>>,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        ValidateOptions {
            check_integrity: true,
            available_chunks: None,
        }
    }
}

/// A fully-validated manifest. Constructing this type is the only way to assert
/// that a manifest has passed structural validation.
#[derive(Debug, Clone)]
pub struct ValidManifest(Manifest);

impl ValidManifest {
    /// Borrow the underlying manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.0
    }
    /// Consume and return the manifest.
    pub fn into_inner(self) -> Manifest {
        self.0
    }
}

/// Validate a manifest structurally and (optionally) against available chunks.
pub fn validate(manifest: &Manifest, opts: &ValidateOptions) -> Result<ValidManifest> {
    // 1. Format version.
    if manifest.format_version != FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion {
            found: manifest.format_version,
            supported: FORMAT_VERSION,
        });
    }

    // 2. Snapshot id shape.
    validate_snapshot_id(&manifest.snapshot_id)?;

    // 3. Parent reference rules.
    if let Some(parent) = &manifest.parent {
        validate_snapshot_id(parent)?;
        if parent == &manifest.snapshot_id {
            return Err(FormatError::SelfParent(parent.clone()));
        }
    }

    // 4. Artifacts: paths, duplicates, chunk refs, sizes.
    let mut seen_paths: BTreeSet<&str> = BTreeSet::new();
    let mut seen_chunks: BTreeSet<&str> = BTreeSet::new();

    for artifact in &manifest.artifacts {
        validate_artifact_path(&artifact.path)?;
        if !seen_paths.insert(&artifact.path) {
            return Err(FormatError::DuplicateArtifact(artifact.path.clone()));
        }

        let mut covered: u64 = 0;
        for chunk in &artifact.chunks {
            // Chunks are content-addressed by a hex BLAKE3 digest (64 hex chars).
            if !is_hex64(&chunk.id) {
                return Err(FormatError::DuplicateChunk(format!(
                    "chunk id `{}` in `{}` is not a 64-char hex digest",
                    chunk.id, artifact.path
                )));
            }
            // A zero-length chunk is meaningless and indicates corruption.
            if chunk.length == 0 {
                return Err(FormatError::InvalidSpan {
                    path: artifact.path.clone(),
                    offset: covered,
                    length: 0,
                    size: artifact.size,
                });
            }
            // Duplicate chunk ids within a single artifact are rejected here;
            // the same id may legitimately recur across *different* artifacts
            // (that is deduplication), so we scope this per artifact.
            covered =
                covered
                    .checked_add(chunk.length)
                    .ok_or_else(|| FormatError::InvalidSpan {
                        path: artifact.path.clone(),
                        offset: covered,
                        length: chunk.length,
                        size: artifact.size,
                    })?;

            if covered > artifact.size {
                return Err(FormatError::ChunkOutOfBounds {
                    id: chunk.id.clone(),
                    length: chunk.length,
                    path: artifact.path.clone(),
                    size: artifact.size,
                });
            }

            // Track global chunk ids to detect intra-manifest length conflicts.
            seen_chunks.insert(chunk.id.as_str());
        }

        if covered != artifact.size {
            return Err(FormatError::IncompleteChunkMap {
                path: artifact.path.clone(),
                covered,
                size: artifact.size,
            });
        }
    }

    // 5. Chunk length consistency: the same chunk id must always declare the
    //    same length (content addressing implies a unique length).
    let mut lengths: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for artifact in &manifest.artifacts {
        for chunk in &artifact.chunks {
            match lengths.get(chunk.id.as_str()) {
                Some(&l) if l != chunk.length => {
                    return Err(FormatError::ChunkOutOfBounds {
                        id: chunk.id.clone(),
                        length: chunk.length,
                        path: artifact.path.clone(),
                        size: artifact.size,
                    });
                }
                Some(_) => {}
                None => {
                    lengths.insert(chunk.id.as_str(), chunk.length);
                }
            }
        }
    }

    // 6. Available chunk coverage.
    if let Some(available) = &opts.available_chunks {
        for artifact in &manifest.artifacts {
            for chunk in &artifact.chunks {
                if !available.contains(&chunk.id) {
                    return Err(FormatError::UnknownChunk {
                        path: artifact.path.clone(),
                        chunk: chunk.id.clone(),
                    });
                }
            }
        }
    }

    // 7. Integrity digest.
    if opts.check_integrity {
        let expected =
            manifest
                .integrity
                .as_ref()
                .ok_or_else(|| FormatError::IntegrityMismatch {
                    what: "manifest.integrity".into(),
                    expected: "<present>".into(),
                    actual: "<missing>".into(),
                })?;
        let actual = manifest.integrity_digest()?;
        if &actual != expected {
            return Err(FormatError::IntegrityMismatch {
                what: "manifest".into(),
                expected: expected.clone(),
                actual,
            });
        }
    }

    Ok(ValidManifest(manifest.clone()))
}

/// Validate a snapshot id: non-empty, limited charset, no path separators.
pub fn validate_snapshot_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        return Err(FormatError::InvalidSnapshotId(id.into()));
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok || id.contains("..") {
        return Err(FormatError::InvalidSnapshotId(id.into()));
    }
    Ok(())
}

/// Validate an artifact path: relative, normalized, no traversal.
pub fn validate_artifact_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(FormatError::InvalidPathComponent(path.into()));
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(FormatError::AbsolutePath(path.into()));
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir => {
                return Err(FormatError::InvalidPathComponent(path.into()));
            }
            Component::ParentDir => {
                return Err(FormatError::PathTraversal(path.into()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(FormatError::AbsolutePath(path.into()));
            }
        }
    }
    Ok(())
}

/// Detect a parent reference cycle given a function that resolves a snapshot id
/// to its parent id.
///
/// Returns the offending chain (as `a -> b -> a`) on a cycle.
pub fn detect_parent_cycle<F>(start: &str, mut parent_of: F) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut chain = vec![start.to_string()];
    let mut current = start.to_string();
    // Bound the walk to avoid an unbounded loop on adversarial input.
    for _ in 0..4096 {
        match parent_of(&current) {
            Some(next) => {
                if chain.contains(&next) {
                    chain.push(next);
                    return Some(chain.join(" -> "));
                }
                chain.push(next.clone());
                current = next;
            }
            None => return None,
        }
    }
    Some(chain.join(" -> "))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}
