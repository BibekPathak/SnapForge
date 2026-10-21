//! HTTP client that pulls a snapshot from a remote store into a local one.
//!
//! This is the mirror of [`crate::push_snapshot`]: it fetches the remote
//! manifest, then downloads every referenced chunk into the local CAS. Guarantees:
//!
//! - **Integrity**: every downloaded chunk is verified against its content id
//!   before it is written into the local CAS (which hashes on `put` anyway, and
//!   `Cas::get` re-verifies on read).
//! - **Idempotent resume**: chunks already valid locally are skipped, so a
//!   partially completed pull resumes cheaply.
//! - **Ranged resume**: an interrupted chunk download may leave a `.part` file;
//!   the next pull requests only the missing tail with a `Range` request.
//! - **Bounded concurrency and retries**, cancellation, and authoritative
//!   integrity checks (the manifest's integrity digest is validated on arrival).
//! - **Atomic publication**: the snapshot is published (staging + `rename`)
//!   only after every chunk is present and valid locally.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use reqwest::{Client, StatusCode};
use snapforge_format::{Manifest, ValidateOptions};
use snapforge_storage::{CancellationToken, Cas};
use tokio::sync::Semaphore;

use crate::error::{RemoteError, Result};
use crate::http::{
    normalize_endpoint, retry_get_manifest, sleep_or_cancel, status_error, with_auth, RemoteSession,
};
use crate::protocol::chunk_url;

/// Options controlling a remote pull.
#[derive(Debug, Clone)]
pub struct PullOptions {
    /// Maximum number of chunk downloads in flight.
    pub concurrency: usize,
    /// Resume an interrupted chunk download with an HTTP range request.
    pub resume_partial: bool,
    /// Re-fetch and re-verify chunks the local CAS already holds.
    pub verify_local: bool,
    /// Retries, timeout, auth, and cancellation.
    pub session: RemoteSession,
}

impl Default for PullOptions {
    fn default() -> Self {
        PullOptions {
            concurrency: 4,
            resume_partial: true,
            verify_local: false,
            session: RemoteSession::default(),
        }
    }
}

impl PullOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(RemoteError::Config("concurrency must be >= 1".into()));
        }
        self.session.validate()
    }
}

/// The result of a remote pull.
#[derive(Debug, Clone)]
pub struct PullOutcome {
    /// Snapshot id pulled.
    pub snapshot_id: String,
    /// Normalized remote endpoint.
    pub endpoint: String,
    /// Distinct chunks referenced by the snapshot.
    pub chunks_total: u64,
    /// Chunks downloaded in this run.
    pub chunks_downloaded: u64,
    /// Chunks already present locally and skipped.
    pub chunks_skipped: u64,
    /// Bytes downloaded in this run.
    pub bytes_transferred: u64,
    /// Chunks resumed from a partial `.part` file via a range request.
    pub chunks_resumed: u64,
    /// Total retries performed across all requests.
    pub retries: u64,
    /// Path to the published snapshot directory in the local store.
    pub destination: PathBuf,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Pull a snapshot from a remote endpoint into a local store.
///
/// `dest_cas` / `dest_snapshots_dir` / `dest_staging_dir` describe the local
/// store (the same layout `Config` uses). The snapshot is published atomically
/// after every chunk verifies.
pub async fn pull_snapshot(
    endpoint: &str,
    snapshot_id: &str,
    dest_cas: Arc<Cas>,
    dest_snapshots_dir: &Path,
    dest_staging_dir: &Path,
    opts: PullOptions,
) -> Result<PullOutcome> {
    opts.validate()?;
    opts.session.cancel.check()?;
    let started = Instant::now();
    let base = normalize_endpoint(endpoint)?;
    let client = opts.session.client()?;
    let retries = Arc::new(AtomicU64::new(0));

    // Fetch the remote manifest and validate its integrity before trusting it.
    let manifest = retry_get_manifest(&client, &base, snapshot_id, &opts.session, &retries)
        .await?
        .ok_or_else(|| RemoteError::Protocol(format!("remote has no snapshot `{snapshot_id}`")))?;
    snapforge_format::validate(
        &manifest,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: None,
        },
    )?;

    // Idempotency: a valid local snapshot with identical content is a no-op.
    let dest_snapshot_dir = dest_snapshots_dir.join(snapshot_id);
    if dest_snapshot_dir.is_dir() && !opts.verify_local {
        let available: std::collections::BTreeSet<String> = dest_cas.list()?.into_iter().collect();
        if let Ok(existing) = snapforge_format::load_validated(
            &dest_snapshot_dir,
            &ValidateOptions {
                check_integrity: true,
                available_chunks: Some(available),
            },
        ) {
            if existing.manifest() == &manifest {
                return Ok(PullOutcome {
                    snapshot_id: snapshot_id.to_string(),
                    endpoint: base,
                    chunks_total: manifest.chunk_lengths().len() as u64,
                    chunks_downloaded: 0,
                    chunks_skipped: manifest.chunk_lengths().len() as u64,
                    bytes_transferred: 0,
                    chunks_resumed: 0,
                    retries: retries.load(Ordering::SeqCst),
                    destination: dest_snapshot_dir,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
            return Err(RemoteError::Protocol(format!(
                "local store already has a different valid snapshot `{snapshot_id}`"
            )));
        }
    }

    let chunk_lengths = manifest.chunk_lengths();
    let chunks_total = chunk_lengths.len() as u64;

    let downloaded = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let resumed = Arc::new(AtomicU64::new(0));

    let semaphore = Arc::new(Semaphore::new(opts.concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for (id, length) in chunk_lengths {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| RemoteError::Join(e.to_string()))?;

        let client = client.clone();
        let dest_cas = dest_cas.clone();
        let base = base.clone();
        let opts = opts.clone();
        let retries = retries.clone();
        let downloaded = downloaded.clone();
        let skipped = skipped.clone();
        let bytes = bytes.clone();
        let resumed = resumed.clone();

        join_set.spawn(async move {
            let url = chunk_url(&base, &id);

            // Already present and valid locally? Skip (idempotent resume).
            if dest_cas.contains(&id) && !opts.verify_local && dest_cas.verify(&id).is_ok() {
                skipped.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                return Ok::<(), RemoteError>(());
            }

            let part = part_path(&dest_cas, &id);
            let mut resumed_this = 0u64;

            // Resume a partial download if one exists.
            let mut body: Vec<u8> = Vec::new();
            if opts.resume_partial {
                if let Ok(existing) = std::fs::read(&part) {
                    if !existing.is_empty() && existing.len() as u64 <= length {
                        body = existing;
                        resumed_this = body.len() as u64;
                    }
                }
            }

            let expected_len = length;
            while (body.len() as u64) < expected_len {
                let start = body.len() as u64;
                let chunk = retry_get_range(&client, &url, start, &opts.session, &retries).await?;
                if chunk.is_empty() {
                    return Err(RemoteError::Protocol(format!(
                        "remote returned an empty range for chunk {id} at offset {start}"
                    )));
                }
                body.extend_from_slice(&chunk);
                if body.len() as u64 > expected_len {
                    return Err(RemoteError::Protocol(format!(
                        "remote returned too many bytes for chunk {id}"
                    )));
                }
                // Persist progress so an interrupted pull can resume later.
                if opts.resume_partial && body.len() as u64 != expected_len {
                    let _ = std::fs::write(&part, &body);
                }
            }

            if body.len() as u64 != expected_len {
                return Err(RemoteError::Protocol(format!(
                    "chunk {id} has length {} but manifest declares {expected_len}",
                    body.len()
                )));
            }

            // Content-addressed integrity check before publishing into the CAS.
            let computed = blake3::hash(&body).to_hex().to_string();
            if computed != id {
                let _ = std::fs::remove_file(&part);
                return Err(RemoteError::Protocol(format!(
                    "remote chunk {id} is corrupt: computed {computed}"
                )));
            }

            let (_, created) = dest_cas.put(&body)?;
            debug_assert!(created || dest_cas.contains(&id));
            let _ = std::fs::remove_file(&part);

            downloaded.fetch_add(1, Ordering::SeqCst);
            bytes.fetch_add(expected_len, Ordering::SeqCst);
            if resumed_this > 0 {
                resumed.fetch_add(1, Ordering::SeqCst);
            }
            drop(permit);
            Ok(())
        });
    }

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                opts.session.cancel.cancel();
                return Err(e);
            }
            Err(e) => {
                opts.session.cancel.cancel();
                return Err(RemoteError::Join(e.to_string()));
            }
        }
    }

    opts.session.cancel.check()?;

    // Publish atomically: validate against the destination CAS, then rename.
    let staging = dest_staging_dir.join(snapshot_id);
    if staging.exists() {
        snapforge_format::cleanup_staging(&staging);
    }
    snapforge_format::write_to_dir(&staging, &manifest)?;

    let available: std::collections::BTreeSet<String> = dest_cas.list()?.into_iter().collect();
    if let Err(e) = snapforge_format::validate(
        &manifest,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: Some(available),
        },
    ) {
        snapforge_format::cleanup_staging(&staging);
        return Err(RemoteError::Format(e));
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
            return Err(RemoteError::Format(e));
        }
    };
    debug_assert_eq!(published, dest_snapshot_dir);

    Ok(PullOutcome {
        snapshot_id: snapshot_id.to_string(),
        endpoint: base,
        chunks_total,
        chunks_downloaded: downloaded.load(Ordering::SeqCst),
        chunks_skipped: skipped.load(Ordering::SeqCst),
        bytes_transferred: bytes.load(Ordering::SeqCst),
        chunks_resumed: resumed.load(Ordering::SeqCst),
        retries: retries.load(Ordering::SeqCst),
        destination: published,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Path of the `.part` scratch file used to resume a chunk download.
fn part_path(cas: &Cas, id: &str) -> PathBuf {
    cas.root().join(format!(".part-{id}"))
}

/// Fetch a chunk, or the tail of it starting at `start` (a range request).
async fn retry_get_range(
    client: &Client,
    url: &str,
    start: u64,
    session: &RemoteSession,
    retries: &AtomicU64,
) -> Result<Vec<u8>> {
    let mut attempt = 0u32;
    loop {
        session.cancel.check()?;
        match get_range_once(client, url, start, session.token.as_deref()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < session.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(crate::http::backoff(session, attempt), &session.cancel).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn get_range_once(
    client: &Client,
    url: &str,
    start: u64,
    token: Option<&str>,
) -> Result<Vec<u8>> {
    let req = if start == 0 {
        client.get(url)
    } else {
        client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={start}-"))
    };
    let resp = with_auth(req, token).send().await?;
    match resp.status() {
        StatusCode::OK => Ok(resp.bytes().await?.to_vec()),
        StatusCode::PARTIAL_CONTENT => Ok(resp.bytes().await?.to_vec()),
        StatusCode::NOT_FOUND => Err(RemoteError::MissingChunk(url.to_string())),
        _ => Err(status_error(resp, url).await),
    }
}

/// Ensure a pulled snapshot's local chunks are all present and valid.
pub fn verify_pulled(cas: &Cas, manifest: &Manifest) -> Result<u64> {
    let mut verified = 0u64;
    for id in manifest.chunk_lengths().keys() {
        cas.verify(id)?;
        verified += 1;
    }
    Ok(verified)
}

/// A cancellation token for a pull, for callers that want to abort.
pub fn new_cancellation() -> CancellationToken {
    CancellationToken::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_options_reject_zero_concurrency() {
        let o = PullOptions {
            concurrency: 0,
            ..PullOptions::default()
        };
        assert!(o.validate().is_err());
    }

    #[test]
    fn pull_options_reject_zero_timeout() {
        let mut o = PullOptions::default();
        o.session.request_timeout = std::time::Duration::ZERO;
        assert!(o.validate().is_err());
    }
}
