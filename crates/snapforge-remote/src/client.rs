//! HTTP client that pushes a local snapshot to a remote store.
//!
//! The client mirrors the guarantees of local transfer: bounded concurrency,
//! integrity-checked source chunks, idempotent skipping of chunks the remote
//! already has, bounded retries with backoff, cancellation, and manifest
//! publication only after every chunk has been accepted.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use snapforge_format::{Manifest, ValidateOptions};
use snapforge_storage::Cas;
use tokio::sync::Semaphore;

use crate::error::{RemoteError, Result};
use crate::http::{
    retry_get_chunk, retry_get_manifest, retry_head, retry_put_chunk, retry_put_manifest,
    status_error, with_auth, ChunkProbe, RemoteSession,
};
use crate::protocol::{chunk_url, health_url};

/// Options controlling a remote push.
#[derive(Debug, Clone)]
pub struct PushOptions {
    /// Maximum number of chunk uploads in flight.
    pub concurrency: usize,
    /// Fetch and hash chunks the remote reports as already present.
    pub verify_existing: bool,
    /// Retries, timeout, auth, and cancellation.
    pub session: RemoteSession,
}

impl Default for PushOptions {
    fn default() -> Self {
        PushOptions {
            concurrency: 4,
            verify_existing: false,
            session: RemoteSession::default(),
        }
    }
}

impl PushOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(RemoteError::Config("concurrency must be >= 1".into()));
        }
        self.session.validate()
    }
}

/// Backward-compatible alias for the Phase 10 push options type.
pub type RemoteOptions = PushOptions;

/// The result of a remote push.
#[derive(Debug, Clone)]
pub struct PushOutcome {
    /// Snapshot id pushed.
    pub snapshot_id: String,
    /// Normalized remote endpoint.
    pub endpoint: String,
    /// Distinct chunks referenced by the snapshot.
    pub chunks_total: u64,
    /// Chunks uploaded in this run.
    pub chunks_uploaded: u64,
    /// Chunks the remote already had (skipped).
    pub chunks_skipped: u64,
    /// Bytes uploaded in this run.
    pub bytes_transferred: u64,
    /// Total retries performed across all requests.
    pub retries: u64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Backward-compatible alias for the Phase 10 push outcome type.
pub type RemoteOutcome = PushOutcome;

/// Push a published snapshot from a local store to a remote SnapForge server.
pub async fn push_snapshot(
    source_cas: Arc<Cas>,
    source_snapshots_dir: &Path,
    endpoint: &str,
    snapshot_id: &str,
    opts: PushOptions,
) -> Result<PushOutcome> {
    opts.validate()?;
    opts.session.cancel.check()?;
    let started = Instant::now();
    let base = crate::http::normalize_endpoint(endpoint)?;

    let manifest = snapforge_format::load(&source_snapshots_dir.join(snapshot_id))?;
    snapforge_format::validate(
        &manifest,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: None,
        },
    )?;

    let chunk_lengths = manifest.chunk_lengths();
    let chunks_total = chunk_lengths.len() as u64;

    let client = opts.session.client()?;
    let retries = Arc::new(AtomicU64::new(0));

    // Early conflict check. When `verify_existing` is set we deliberately
    // continue even for an identical manifest so every chunk is re-checked.
    match retry_get_manifest(&client, &base, snapshot_id, &opts.session, &retries).await? {
        Some(existing) if existing == manifest => {
            if !opts.verify_existing {
                return Ok(PushOutcome {
                    snapshot_id: snapshot_id.to_string(),
                    endpoint: base,
                    chunks_total,
                    chunks_uploaded: 0,
                    chunks_skipped: chunks_total,
                    bytes_transferred: 0,
                    retries: retries.load(Ordering::SeqCst),
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
        }
        Some(_) => {
            return Err(RemoteError::Protocol(format!(
                "remote already has a different snapshot `{snapshot_id}`"
            )));
        }
        None => {}
    }

    let uploaded = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let semaphore = Arc::new(Semaphore::new(opts.concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for (id, length) in chunk_lengths {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| RemoteError::Join(e.to_string()))?;

        let client = client.clone();
        let source_cas = source_cas.clone();
        let base = base.clone();
        let opts = opts.clone();
        let retries = retries.clone();
        let uploaded = uploaded.clone();
        let skipped = skipped.clone();
        let bytes = bytes.clone();

        join_set.spawn(async move {
            let url = chunk_url(&base, &id);
            let mut present = retry_head(&client, &url, &opts.session, &retries).await?;

            if present && opts.verify_existing {
                match retry_get_chunk(&client, &url, &opts.session, &retries).await? {
                    ChunkProbe::Present(remote) => {
                        present = blake3::hash(&remote).to_hex().to_string() == id;
                    }
                    // Missing or corrupt: uploading repairs it.
                    ChunkProbe::Missing | ChunkProbe::Corrupt => present = false,
                }
            }

            if present {
                skipped.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                return Ok::<(), RemoteError>(());
            }

            let cas = source_cas.clone();
            let id_for_task = id.clone();
            let data = tokio::task::spawn_blocking(move || cas.get(&id_for_task))
                .await
                .map_err(|e| RemoteError::Join(e.to_string()))??;
            if data.len() as u64 != length {
                return Err(RemoteError::Protocol(format!(
                    "local chunk {id} length {} != manifest length {length}",
                    data.len()
                )));
            }

            retry_put_chunk(&client, &url, &data, &opts.session, &retries).await?;
            uploaded.fetch_add(1, Ordering::SeqCst);
            bytes.fetch_add(length, Ordering::SeqCst);
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

    let body = snapforge_format::to_bytes(&manifest)?;
    retry_put_manifest(
        &client,
        &crate::protocol::snapshot_url(&base, snapshot_id),
        &body,
        &opts.session,
        &retries,
    )
    .await?;

    Ok(PushOutcome {
        snapshot_id: snapshot_id.to_string(),
        endpoint: base,
        chunks_total,
        chunks_uploaded: uploaded.load(Ordering::SeqCst),
        chunks_skipped: skipped.load(Ordering::SeqCst),
        bytes_transferred: bytes.load(Ordering::SeqCst),
        retries: retries.load(Ordering::SeqCst),
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Probe a remote endpoint's `/v1/health` route.
pub async fn health(endpoint: &str, session: &RemoteSession) -> Result<String> {
    let base = crate::http::normalize_endpoint(endpoint)?;
    let client = session.client()?;
    let url = health_url(&base);
    let resp = with_auth(client.get(&url), session.token.as_deref())
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(status_error(resp, &url).await);
    }
    Ok(resp.text().await?)
}

/// Fetch and deserialize a snapshot's manifest from a remote endpoint.
pub async fn fetch_manifest(
    endpoint: &str,
    snapshot_id: &str,
    session: &RemoteSession,
) -> Result<Manifest> {
    let base = crate::http::normalize_endpoint(endpoint)?;
    let client = session.client()?;
    let retries = AtomicU64::new(0);
    retry_get_manifest(&client, &base, snapshot_id, session, &retries)
        .await?
        .ok_or_else(|| RemoteError::Protocol(format!("remote has no snapshot `{snapshot_id}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_options_reject_zero_concurrency() {
        let o = PushOptions {
            concurrency: 0,
            ..PushOptions::default()
        };
        assert!(o.validate().is_err());
    }

    #[test]
    fn push_options_reject_zero_timeout() {
        let mut o = PushOptions::default();
        o.session.request_timeout = std::time::Duration::ZERO;
        assert!(o.validate().is_err());
    }
}
