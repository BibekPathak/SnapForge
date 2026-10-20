//! Snapshot extraction: reconstruct a stored snapshot's artifacts on disk.
//!
//! Extraction is the inverse of the storage pipeline: it reads a snapshot's
//! manifest, verifies every referenced chunk against its content id in the CAS,
//! and reassembles each artifact at its declared relative path under an output
//! directory.
//!
//! Guarantees, mirrored from the rest of the storage layer:
//!
//! - **Integrity.** Every chunk is read through the CAS, which recomputes its
//!   BLAKE3 digest; a mismatch is an error, never a silent pass. The
//!   reconstructed artifact length is checked against the manifest.
//! - **Bounded concurrency and memory.** At most `concurrency` artifacts are
//!   extracted at once, and each worker holds at most one chunk buffer.
//! - **Atomic per-file publication.** Each artifact is written to a temporary
//!   file in its destination directory and `rename`d into place, so a partially
//!   written artifact is never observable at its final path.
//! - **Safe destinations.** Artifact paths are re-validated as relative,
//!   normalized paths before use, so extraction cannot escape the output
//!   directory. A non-empty destination is refused unless `overwrite` is set.
//! - **Cancellation.** A [`CancellationToken`] stops extraction promptly.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use snapforge_format::{Artifact, ValidateOptions};
use tokio::sync::{Mutex, Semaphore};

use crate::cas::Cas;
use crate::error::{Result, StorageError};
use crate::transfer::CancellationToken;

/// Options controlling an extraction.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Maximum number of artifacts reconstructed concurrently.
    pub concurrency: usize,
    /// Allow overwriting existing files in a non-empty destination directory.
    pub overwrite: bool,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        ExtractOptions {
            concurrency: 4,
            overwrite: false,
            cancel: CancellationToken::new(),
        }
    }
}

impl ExtractOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(StorageError::Config("concurrency must be >= 1".into()));
        }
        Ok(())
    }
}

/// The result of a completed extraction.
#[derive(Debug, Clone)]
pub struct ExtractOutcome {
    /// Snapshot id extracted.
    pub snapshot_id: String,
    /// Directory the artifacts were written into.
    pub destination: PathBuf,
    /// Number of artifacts written.
    pub artifacts: u64,
    /// Total bytes written across all artifacts.
    pub bytes_written: u64,
    /// Number of chunk reads performed (including repeated chunks).
    pub chunks_read: u64,
    /// Number of distinct chunks referenced.
    pub distinct_chunks: u64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Extract a snapshot from `snapshot_dir` into `output`.
///
/// The manifest is loaded and fully validated (structure, integrity, and chunk
/// availability) before any file is written. The destination directory is
/// created if missing; a pre-existing non-empty directory is refused unless
/// `opts.overwrite` is set.
pub async fn extract_snapshot(
    cas: Arc<Cas>,
    snapshot_dir: &Path,
    output: &Path,
    opts: ExtractOptions,
) -> Result<ExtractOutcome> {
    opts.validate()?;
    opts.cancel.check()?;

    // Load and validate the manifest, including that every chunk exists.
    let manifest = snapforge_format::load(snapshot_dir)?;
    let available: BTreeSet<String> = cas.list()?.into_iter().collect();
    let validated = snapforge_format::validate(
        &manifest,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: Some(available),
        },
    )
    .map_err(StorageError::Format)?;
    let manifest = validated.into_inner();

    prepare_output(output, opts.overwrite)?;

    let started = std::time::Instant::now();
    let bytes_written = Arc::new(AtomicU64::new(0));
    let chunks_read = Arc::new(AtomicU64::new(0));
    let distinct: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let counter = Arc::new(AtomicU64::new(0));

    let semaphore = Arc::new(Semaphore::new(opts.concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for artifact in manifest.artifacts.clone() {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| StorageError::Join(e.to_string()))?;

        let cas = cas.clone();
        let cancel = opts.cancel.clone();
        let output = output.to_path_buf();
        let bytes_written = bytes_written.clone();
        let chunks_read = chunks_read.clone();
        let distinct = distinct.clone();
        let counter = counter.clone();

        join_set.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                extract_one(&cas, &output, &artifact, &cancel, &counter)
            })
            .await
            .map_err(|e| StorageError::Join(e.to_string()))?;

            let written = result?;
            let (bytes, reads, ids) = written;
            bytes_written.fetch_add(bytes, Ordering::SeqCst);
            chunks_read.fetch_add(reads, Ordering::SeqCst);
            {
                let mut d = distinct.lock().await;
                for id in ids {
                    d.insert(id);
                }
            }
            drop(permit);
            Ok::<(), StorageError>(())
        });
    }

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                opts.cancel.cancel();
                return Err(e);
            }
            Err(e) => {
                opts.cancel.cancel();
                return Err(StorageError::Join(e.to_string()));
            }
        }
    }

    opts.cancel.check()?;

    let distinct_chunks = distinct.lock().await.len() as u64;

    Ok(ExtractOutcome {
        snapshot_id: manifest.snapshot_id,
        destination: output.to_path_buf(),
        artifacts: manifest.artifacts.len() as u64,
        bytes_written: bytes_written.load(Ordering::SeqCst),
        chunks_read: chunks_read.load(Ordering::SeqCst),
        distinct_chunks,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Ensure the destination directory is usable for extraction.
fn prepare_output(output: &Path, overwrite: bool) -> Result<()> {
    if output.exists() {
        if !output.is_dir() {
            return Err(StorageError::Extract(format!(
                "output path {} exists and is not a directory",
                output.display()
            )));
        }
        if !overwrite {
            let mut rd = std::fs::read_dir(output)
                .map_err(|e| StorageError::io_at(output.to_path_buf(), e))?;
            if rd.next().is_some() {
                return Err(StorageError::Extract(format!(
                    "output directory {} is not empty; pass --force to overwrite existing files",
                    output.display()
                )));
            }
        }
    } else {
        std::fs::create_dir_all(output)
            .map_err(|e| StorageError::io_at(output.to_path_buf(), e))?;
    }
    Ok(())
}

/// Reconstruct one artifact atomically, returning `(bytes, reads, chunk ids)`.
fn extract_one(
    cas: &Cas,
    output: &Path,
    artifact: &Artifact,
    cancel: &CancellationToken,
    counter: &AtomicU64,
) -> Result<(u64, u64, Vec<String>)> {
    cancel.check()?;

    // Defensive re-validation: extraction must never escape `output`.
    snapforge_format::validate_artifact_path(&artifact.path).map_err(StorageError::Format)?;
    let target = output.join(&artifact.path);
    let parent = target.parent().unwrap_or(output).to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| StorageError::io_at(parent.clone(), e))?;

    let n = counter.fetch_add(1, Ordering::SeqCst);
    let tmp = parent.join(format!(".snapforge-extract-{}-{n}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);

    let mut written = 0u64;
    let mut reads = 0u64;
    let mut ids = Vec::with_capacity(artifact.chunks.len());

    let build = (|| -> Result<()> {
        let mut file =
            std::fs::File::create(&tmp).map_err(|e| StorageError::io_at(tmp.clone(), e))?;
        for chunk in &artifact.chunks {
            cancel.check()?;
            // `Cas::get` recomputes the digest, so corruption is an error.
            let bytes = cas.get(&chunk.id)?;
            if bytes.len() as u64 != chunk.length {
                return Err(StorageError::CorruptChunk {
                    id: chunk.id.clone(),
                    expected: chunk.id.clone(),
                    actual: format!("length {} != expected {}", bytes.len(), chunk.length),
                });
            }
            use std::io::Write;
            file.write_all(&bytes)
                .map_err(|e| StorageError::io_at(tmp.clone(), e))?;
            written += bytes.len() as u64;
            reads += 1;
            ids.push(chunk.id.clone());
        }
        file.flush()
            .map_err(|e| StorageError::io_at(tmp.clone(), e))?;
        if written != artifact.size {
            return Err(StorageError::Extract(format!(
                "artifact `{}` reconstructed {written} bytes but the manifest declares {}",
                artifact.path, artifact.size
            )));
        }
        Ok(())
    })();

    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        StorageError::io_at(target.clone(), e)
    })?;

    Ok((written, reads, ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_reject_zero_concurrency() {
        let o = ExtractOptions {
            concurrency: 0,
            ..ExtractOptions::default()
        };
        assert!(o.validate().is_err());
    }

    #[test]
    fn prepare_output_refuses_non_empty_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("dest");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("existing"), b"x").unwrap();
        assert!(prepare_output(&out, false).is_err());
        assert!(prepare_output(&out, true).is_ok());
    }

    #[test]
    fn prepare_output_creates_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("nested/dest");
        prepare_output(&out, false).unwrap();
        assert!(out.is_dir());
    }
}
