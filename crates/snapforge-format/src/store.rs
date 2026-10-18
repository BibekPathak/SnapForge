//! Manifest persistence and atomic snapshot publication.
//!
//! Snapshot publication is atomic: content is written into a staging directory,
//! validated, then renamed into place. A valid published snapshot is never
//! overwritten by a partial result.

use std::path::{Path, PathBuf};

use crate::error::{FormatError, Result};
use crate::manifest::Manifest;
use crate::validate::{self, ValidManifest, ValidateOptions};

/// File name used for the manifest within a snapshot directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Serialize a manifest to pretty JSON bytes.
pub fn to_bytes(manifest: &Manifest) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(manifest)?)
}

/// Parse a manifest from bytes.
pub fn from_bytes(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    Ok(manifest)
}

/// Load a manifest from a snapshot directory (expects `manifest.json`).
pub fn load(snapshot_dir: &Path) -> Result<Manifest> {
    let path = snapshot_dir.join(MANIFEST_FILE);
    let bytes = std::fs::read(&path).map_err(|e| FormatError::io_at(path.clone(), e))?;
    from_bytes(&bytes)
}

/// Write a manifest into a directory as `manifest.json` (non-atomic; used for
/// staging, which is then published atomically).
pub fn write_to_dir(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| FormatError::io_at(dir.to_path_buf(), e))?;
    let path = dir.join(MANIFEST_FILE);
    let bytes = to_bytes(manifest)?;
    std::fs::write(&path, &bytes).map_err(|e| FormatError::io_at(path.clone(), e))?;
    Ok(path)
}

/// Load and fully validate a manifest from a snapshot directory.
pub fn load_validated(snapshot_dir: &Path, opts: &ValidateOptions) -> Result<ValidManifest> {
    let manifest = load(snapshot_dir)?;
    validate::validate(&manifest, opts)
}

/// Publish a snapshot directory atomically.
///
/// `staging` must already contain the complete snapshot contents (manifest and,
/// in later phases, artifact/chunk data). This function:
///
/// 1. Loads and validates the staged manifest.
/// 2. Refuses to overwrite an existing destination snapshot id.
/// 3. Renames `staging` to `dest_parent/<snapshot_id>` atomically.
///
/// On any failure the destination is left untouched.
pub fn publish(staging: &Path, dest_parent: &Path, opts: &ValidateOptions) -> Result<PathBuf> {
    // Validate staged manifest before touching the destination.
    let validated = load_validated(staging, opts)?;
    let snapshot_id = validated.manifest().snapshot_id.clone();

    std::fs::create_dir_all(dest_parent)
        .map_err(|e| FormatError::io_at(dest_parent.to_path_buf(), e))?;

    let dest = dest_parent.join(&snapshot_id);
    if dest.exists() {
        return Err(FormatError::Publication(format!(
            "refusing to overwrite existing snapshot `{snapshot_id}` at {}",
            dest.display()
        )));
    }

    // Same-filesystem rename is atomic. If the rename target's parent differs
    // by filesystem, this fails loudly rather than copying partially.
    std::fs::rename(staging, &dest).map_err(|e| {
        FormatError::Publication(format!(
            "atomic rename of {} -> {} failed: {e}",
            staging.display(),
            dest.display()
        ))
    })?;

    Ok(dest)
}

/// Remove a staging directory, ignoring absence. Used to clean up after a
/// failed publication.
pub fn cleanup_staging(staging: &Path) {
    if staging.exists() {
        let _ = std::fs::remove_dir_all(staging);
    }
}
