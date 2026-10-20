//! Reliable local snapshot transfer between stores.
//!
//! Transfers copy a snapshot's manifest and all referenced chunks from a
//! source store to a destination store. The design guarantees:
//!
//! - **Bounded concurrency**: at most `concurrency` chunk copies in flight.
//! - **Integrity**: each chunk is verified against its content id after copy.
//! - **Idempotent retries**: chunks already present at the destination are
//!   skipped, so re-running a failed transfer resumes cheaply.
//! - **Retries**: transient failures are retried with bounded backoff.
//! - **Cancellation**: a [`CancellationToken`] stops the transfer promptly.
//! - **Atomic publication**: the destination snapshot directory is only
//!   published after every chunk and the manifest verify.
//!
//! The initial implementation is local-filesystem only; it is fully testable
//! without any cloud account.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use snapforge_format::{Manifest, ValidateOptions};
use tokio::sync::{Mutex, Semaphore};

use crate::cas::Cas;
use crate::error::{Result, StorageError};

/// A simple cooperative cancellation token.
///
/// Cloning shares the same underlying flag. `cancel()` sets it; workers check
/// `is_cancelled()` between steps and abort promptly.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(StorageError::Other("transfer cancelled".into()))
        } else {
            Ok(())
        }
    }
}

/// Options controlling a transfer.
#[derive(Debug, Clone)]
pub struct TransferOptions {
    /// Maximum number of chunk copies in flight.
    pub concurrency: usize,
    /// Maximum retry attempts per chunk (0 means a single attempt).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_backoff: Duration,
    /// Verify each copied chunk against its content id.
    pub verify: bool,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

impl Default for TransferOptions {
    fn default() -> Self {
        TransferOptions {
            concurrency: 4,
            max_retries: 3,
            retry_backoff: Duration::from_millis(50),
            verify: true,
            cancel: CancellationToken::new(),
        }
    }
}

impl TransferOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(StorageError::Config("concurrency must be >= 1".into()));
        }
        Ok(())
    }
}

/// Progress information emitted during a transfer.
#[derive(Debug, Clone, Default)]
pub struct TransferProgress {
    /// Chunks successfully copied (or found already present).
    pub chunks_done: u64,
    /// Total chunks to transfer.
    pub chunks_total: u64,
    /// Bytes transferred (newly copied; excludes already-present chunks).
    pub bytes_transferred: u64,
}

/// The result of a completed transfer.
#[derive(Debug, Clone)]
pub struct TransferOutcome {
    pub snapshot_id: String,
    pub chunks_total: u64,
    pub chunks_copied: u64,
    pub chunks_skipped: u64,
    pub bytes_transferred: u64,
    pub retries: u64,
    /// Path to the published snapshot directory in the destination store.
    pub destination: PathBuf,
}

/// Transfer a snapshot from `source` store to `dest` store.
///
/// `snapshot_id` names the snapshot in `source`; the manifest is read from
/// `source_snapshots_dir/<id>`. All referenced chunks are copied into `dest_cas`,
/// then the snapshot manifest is published atomically under
/// `dest_snapshots_dir/<id>` using `dest_staging_dir` as the staging area.
pub async fn transfer_snapshot(
    source_cas: Arc<Cas>,
    source_snapshots_dir: &Path,
    dest_cas: Arc<Cas>,
    dest_snapshots_dir: &Path,
    dest_staging_dir: &Path,
    snapshot_id: &str,
    opts: TransferOptions,
) -> Result<TransferOutcome> {
    opts.validate()?;
    opts.cancel.check()?;

    // Load and validate the source manifest (structure + integrity; chunk
    // availability is checked as we copy).
    let source_dir = source_snapshots_dir.join(snapshot_id);
    let manifest = snapforge_format::load(&source_dir)?;
    let validated = snapforge_format::validate(
        &manifest,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: None,
        },
    )
    .map_err(StorageError::Format)?;
    let manifest = validated.into_inner();

    // Distinct chunk ids with their expected lengths.
    let chunk_lengths = manifest.chunk_lengths();
    let chunks_total = chunk_lengths.len() as u64;

    // Idempotency: if the destination already holds a valid snapshot with the
    // same id, treat the transfer as an already-completed no-op. If it exists
    // but is invalid, refuse (never silently replace it).
    let dest_snapshot_dir = dest_snapshots_dir.join(snapshot_id);
    if dest_snapshot_dir.is_dir() {
        let available: std::collections::BTreeSet<String> = dest_cas.list()?.into_iter().collect();
        let valid = snapforge_format::load_validated(
            &dest_snapshot_dir,
            &ValidateOptions {
                check_integrity: true,
                available_chunks: Some(available),
            },
        );
        match valid {
            Ok(existing) => {
                if existing.manifest() == &manifest {
                    tracing::info!(
                        snapshot = snapshot_id,
                        "destination already has an identical valid snapshot; skipping"
                    );
                    return Ok(TransferOutcome {
                        snapshot_id: snapshot_id.to_string(),
                        chunks_total,
                        chunks_copied: 0,
                        chunks_skipped: chunks_total,
                        bytes_transferred: 0,
                        retries: 0,
                        destination: dest_snapshot_dir,
                    });
                }
                return Err(StorageError::Format(
                    snapforge_format::FormatError::Publication(format!(
                        "destination already has a different valid snapshot `{snapshot_id}`"
                    )),
                ));
            }
            Err(_) => {
                return Err(StorageError::Format(
                    snapforge_format::FormatError::Publication(format!(
                        "destination snapshot `{snapshot_id}` exists but is invalid; refusing to overwrite"
                    )),
                ));
            }
        }
    }

    let progress = Arc::new(Mutex::new(TransferProgress {
        chunks_total,
        ..Default::default()
    }));
    let copied = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let bytes_xferred = Arc::new(AtomicU64::new(0));
    let retries = Arc::new(AtomicU64::new(0));

    let semaphore = Arc::new(Semaphore::new(opts.concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for (id, length) in chunk_lengths {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| StorageError::Join(e.to_string()))?;

        let source_cas = source_cas.clone();
        let dest_cas = dest_cas.clone();
        let progress = progress.clone();
        let copied = copied.clone();
        let skipped = skipped.clone();
        let bytes_xferred = bytes_xferred.clone();
        let retries = retries.clone();
        let cancel = opts.cancel.clone();
        let max_retries = opts.max_retries;
        let backoff = opts.retry_backoff;
        let verify = opts.verify;

        join_set.spawn(async move {
            let res = copy_one_chunk(
                &source_cas,
                &dest_cas,
                &id,
                length,
                verify,
                max_retries,
                backoff,
                &cancel,
                &retries,
            )
            .await?;

            {
                let mut p = progress.lock().await;
                p.chunks_done += 1;
                if res.skipped {
                    skipped.fetch_add(1, Ordering::SeqCst);
                } else {
                    copied.fetch_add(1, Ordering::SeqCst);
                    p.bytes_transferred += length;
                    bytes_xferred.fetch_add(length, Ordering::SeqCst);
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
                // Cancel remaining work and surface the first error.
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

    // Publish the snapshot atomically into the destination store.
    let staging = dest_staging_dir.join(snapshot_id);
    if staging.exists() {
        std::fs::remove_dir_all(&staging).ok();
    }
    snapforge_format::write_to_dir(&staging, &manifest)?;

    // Validate against the destination CAS before publishing.
    let available: std::collections::BTreeSet<String> = dest_cas.list()?.into_iter().collect();
    if let Err(e) = snapforge_format::validate(
        &manifest,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: Some(available),
        },
    ) {
        snapforge_format::cleanup_staging(&staging);
        return Err(StorageError::Format(e));
    }

    let published = match snapforge_format::publish(
        &staging,
        dest_snapshots_dir,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: None,
        },
    ) {
        Ok(p) => p,
        Err(e) => {
            snapforge_format::cleanup_staging(&staging);
            return Err(StorageError::Format(e));
        }
    };
    debug_assert_eq!(published, dest_snapshot_dir);

    Ok(TransferOutcome {
        snapshot_id: snapshot_id.to_string(),
        chunks_total,
        chunks_copied: copied.load(Ordering::SeqCst),
        chunks_skipped: skipped.load(Ordering::SeqCst),
        bytes_transferred: bytes_xferred.load(Ordering::SeqCst),
        retries: retries.load(Ordering::SeqCst),
        destination: published,
    })
}

struct ChunkCopyResult {
    skipped: bool,
}

#[allow(clippy::too_many_arguments)]
async fn copy_one_chunk(
    source_cas: &Cas,
    dest_cas: &Cas,
    id: &str,
    length: u64,
    verify: bool,
    max_retries: u32,
    backoff: Duration,
    cancel: &CancellationToken,
    retries: &AtomicU64,
) -> Result<ChunkCopyResult> {
    // Idempotent: if the destination already has a valid copy, skip.
    if dest_cas.contains(id) {
        if !verify {
            return Ok(ChunkCopyResult { skipped: true });
        }
        // Confirm the existing chunk is actually valid.
        match dest_cas.verify(id) {
            Ok(()) => return Ok(ChunkCopyResult { skipped: true }),
            Err(_) => {
                // Corrupt/mismatched existing chunk: fall through and overwrite.
            }
        }
    }

    let mut attempt = 0u32;
    loop {
        cancel.check()?;
        let source_cas = source_cas.clone();
        let dest_cas_inner = dest_cas.clone();
        let id_owned = id.to_string();

        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            // Read (verifies source integrity) then write atomically.
            let bytes = source_cas.get(&id_owned)?;
            if bytes.len() as u64 != length {
                return Err(StorageError::CorruptChunk {
                    id: id_owned.clone(),
                    expected: id_owned.clone(),
                    actual: format!("length {} != expected {length}", bytes.len()),
                });
            }
            let (written_id, _) = dest_cas_inner.put(&bytes)?;
            debug_assert_eq!(written_id, id_owned);
            Ok(())
        })
        .await
        .map_err(|e| StorageError::Join(e.to_string()))?;

        match result {
            Ok(()) => {
                if verify {
                    dest_cas.verify(id)?;
                }
                return Ok(ChunkCopyResult { skipped: false });
            }
            Err(e) => {
                if attempt >= max_retries {
                    return Err(e);
                }
                retries.fetch_add(1, Ordering::SeqCst);
                attempt += 1;
                // Exponential backoff, bounded.
                let delay = backoff.saturating_mul(1u32 << attempt.min(6));
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Compute the total logical size of a snapshot's artifacts (informational).
pub fn snapshot_logical_size(manifest: &Manifest) -> u64 {
    manifest.total_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_token_works() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
        let t2 = t.clone();
        t.cancel();
        assert!(t2.is_cancelled());
        assert!(t2.check().is_err());
    }

    #[test]
    fn options_reject_zero_concurrency() {
        let o = TransferOptions {
            concurrency: 0,
            ..TransferOptions::default()
        };
        assert!(o.validate().is_err());
    }
}
