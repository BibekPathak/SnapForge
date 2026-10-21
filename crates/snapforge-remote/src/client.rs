//! HTTP client that pushes a local snapshot to a remote store.
//!
//! The client mirrors the guarantees of local transfer: bounded concurrency,
//! integrity-checked source chunks, idempotent skipping of chunks the remote
//! already has, bounded retries with backoff, cancellation, and manifest
//! publication only after every chunk has been accepted.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use snapforge_format::{Manifest, ValidateOptions};
use snapforge_storage::{CancellationToken, Cas};
use tokio::sync::Semaphore;

use crate::error::{RemoteError, Result};
use crate::protocol::{chunk_url, health_url, snapshot_url};

/// Options controlling a remote push.
#[derive(Debug, Clone)]
pub struct RemoteOptions {
    /// Maximum number of chunk uploads in flight.
    pub concurrency: usize,
    /// Maximum retry attempts per request (0 means a single attempt).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_backoff: Duration,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Optional bearer token sent with every request.
    pub token: Option<String>,
    /// Fetch and hash chunks the remote reports as already present.
    pub verify_existing: bool,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        RemoteOptions {
            concurrency: 4,
            max_retries: 3,
            retry_backoff: Duration::from_millis(50),
            request_timeout: Duration::from_secs(30),
            token: None,
            verify_existing: false,
            cancel: CancellationToken::new(),
        }
    }
}

impl RemoteOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(RemoteError::Config("concurrency must be >= 1".into()));
        }
        if self.request_timeout.is_zero() {
            return Err(RemoteError::Config(
                "request_timeout must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

/// The result of a remote push.
#[derive(Debug, Clone)]
pub struct RemoteOutcome {
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

/// Push a published snapshot from a local store to a remote SnapForge server.
pub async fn push_snapshot(
    source_cas: Arc<Cas>,
    source_snapshots_dir: &Path,
    endpoint: &str,
    snapshot_id: &str,
    opts: RemoteOptions,
) -> Result<RemoteOutcome> {
    opts.validate()?;
    opts.cancel.check()?;
    let started = Instant::now();
    let base = normalize_endpoint(endpoint)?;

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

    let client = Client::builder()
        .timeout(opts.request_timeout)
        .user_agent(concat!("snapforge-remote/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let retries = Arc::new(AtomicU64::new(0));

    // Early conflict check. When `verify_existing` is set we deliberately
    // continue even for an identical manifest so every chunk is re-checked.
    match retry_get_manifest(&client, &base, snapshot_id, &opts, &retries).await? {
        Some(existing) if existing == manifest => {
            if !opts.verify_existing {
                return Ok(RemoteOutcome {
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
            let mut present = retry_head(&client, &chunk_url(&base, &id), &opts, &retries).await?;

            if present && opts.verify_existing {
                match retry_get_chunk(&client, &chunk_url(&base, &id), &opts, &retries).await? {
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

            retry_put_chunk(&client, &chunk_url(&base, &id), &data, &opts, &retries).await?;
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
                opts.cancel.cancel();
                return Err(e);
            }
            Err(e) => {
                opts.cancel.cancel();
                return Err(RemoteError::Join(e.to_string()));
            }
        }
    }

    opts.cancel.check()?;

    let body = snapforge_format::to_bytes(&manifest)?;
    retry_put_manifest(
        &client,
        &snapshot_url(&base, snapshot_id),
        &body,
        &opts,
        &retries,
    )
    .await?;

    Ok(RemoteOutcome {
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
pub async fn health(endpoint: &str, opts: &RemoteOptions) -> Result<String> {
    let base = normalize_endpoint(endpoint)?;
    let client = Client::builder().timeout(opts.request_timeout).build()?;
    let resp = with_auth(client.get(health_url(&base)), opts.token.as_deref())
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(status_error(resp, &health_url(&base)).await);
    }
    Ok(resp.text().await?)
}

fn normalize_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(RemoteError::Config("endpoint must not be empty".into()));
    }
    if trimmed.starts_with("https://") {
        return Err(RemoteError::Config(
            "TLS is not built in; expose the server over http:// behind a TLS-terminating proxy"
                .into(),
        ));
    }
    if !trimmed.starts_with("http://") {
        return Err(RemoteError::Config(
            "endpoint must start with http://".into(),
        ));
    }
    Ok(trimmed.to_string())
}

fn with_auth(req: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    match token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

async fn status_error(resp: Response, endpoint: &str) -> RemoteError {
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        return RemoteError::Unauthorized;
    }
    let body = resp.text().await.unwrap_or_default();
    let body = if body.len() > 512 {
        format!("{}…", &body[..512])
    } else {
        body
    };
    RemoteError::Status {
        status: status.as_u16(),
        endpoint: endpoint.to_string(),
        body,
    }
}

fn backoff(opts: &RemoteOptions, attempt: u32) -> Duration {
    opts.retry_backoff.saturating_mul(1u32 << attempt.min(6))
}

async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> Result<()> {
    let deadline = Instant::now() + dur;
    loop {
        cancel.check()?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        let step = (deadline - now).min(Duration::from_millis(50));
        tokio::time::sleep(step).await;
    }
}

async fn retry_head(
    client: &Client,
    url: &str,
    opts: &RemoteOptions,
    retries: &AtomicU64,
) -> Result<bool> {
    let mut attempt = 0u32;
    loop {
        opts.cancel.check()?;
        match head_once(client, url, opts.token.as_deref()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < opts.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(backoff(opts, attempt), &opts.cancel).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn head_once(client: &Client, url: &str, token: Option<&str>) -> Result<bool> {
    let resp = with_auth(client.head(url), token).send().await?;
    match resp.status() {
        StatusCode::OK | StatusCode::NO_CONTENT => Ok(true),
        StatusCode::NOT_FOUND => Ok(false),
        _ => Err(status_error(resp, url).await),
    }
}

enum ChunkProbe {
    Present(Vec<u8>),
    Missing,
    Corrupt,
}

async fn retry_get_chunk(
    client: &Client,
    url: &str,
    opts: &RemoteOptions,
    retries: &AtomicU64,
) -> Result<ChunkProbe> {
    let mut attempt = 0u32;
    loop {
        opts.cancel.check()?;
        match get_chunk_once(client, url, opts.token.as_deref()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < opts.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(backoff(opts, attempt), &opts.cancel).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn get_chunk_once(client: &Client, url: &str, token: Option<&str>) -> Result<ChunkProbe> {
    let resp = with_auth(client.get(url), token).send().await?;
    match resp.status() {
        StatusCode::OK => Ok(ChunkProbe::Present(resp.bytes().await?.to_vec())),
        StatusCode::NOT_FOUND => Ok(ChunkProbe::Missing),
        StatusCode::UNPROCESSABLE_ENTITY => Ok(ChunkProbe::Corrupt),
        _ => Err(status_error(resp, url).await),
    }
}

async fn retry_put_chunk(
    client: &Client,
    url: &str,
    data: &[u8],
    opts: &RemoteOptions,
    retries: &AtomicU64,
) -> Result<()> {
    let mut attempt = 0u32;
    loop {
        opts.cancel.check()?;
        match put_chunk_once(client, url, data, opts.token.as_deref()).await {
            Ok(()) => return Ok(()),
            Err(e) if e.is_retryable() && attempt < opts.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(backoff(opts, attempt), &opts.cancel).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn put_chunk_once(
    client: &Client,
    url: &str,
    data: &[u8],
    token: Option<&str>,
) -> Result<()> {
    let req = client
        .put(url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(data.to_vec());
    let resp = with_auth(req, token).send().await?;
    match resp.status() {
        StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => Ok(()),
        _ => Err(status_error(resp, url).await),
    }
}

async fn retry_put_manifest(
    client: &Client,
    url: &str,
    body: &[u8],
    opts: &RemoteOptions,
    retries: &AtomicU64,
) -> Result<()> {
    let mut attempt = 0u32;
    loop {
        opts.cancel.check()?;
        match put_manifest_once(client, url, body, opts.token.as_deref()).await {
            Ok(()) => return Ok(()),
            Err(e) if e.is_retryable() && attempt < opts.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(backoff(opts, attempt), &opts.cancel).await?;
            }
            Err(e) => {
                // A 409 means a different snapshot already occupies the id;
                // surface it as a protocol conflict rather than a raw status.
                if matches!(e, RemoteError::Status { status: 409, .. }) {
                    return Err(RemoteError::Protocol(
                        "remote already has a different snapshot with this id".into(),
                    ));
                }
                return Err(e);
            }
        }
    }
}

async fn put_manifest_once(
    client: &Client,
    url: &str,
    body: &[u8],
    token: Option<&str>,
) -> Result<()> {
    let req = client
        .put(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec());
    let resp = with_auth(req, token).send().await?;
    match resp.status() {
        StatusCode::OK | StatusCode::CREATED => Ok(()),
        _ => Err(status_error(resp, url).await),
    }
}

async fn retry_get_manifest(
    client: &Client,
    base: &str,
    snapshot_id: &str,
    opts: &RemoteOptions,
    retries: &AtomicU64,
) -> Result<Option<Manifest>> {
    let url = snapshot_url(base, snapshot_id);
    let mut attempt = 0u32;
    loop {
        opts.cancel.check()?;
        match get_manifest_once(client, &url, opts.token.as_deref()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < opts.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(backoff(opts, attempt), &opts.cancel).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn get_manifest_once(
    client: &Client,
    url: &str,
    token: Option<&str>,
) -> Result<Option<Manifest>> {
    let resp = with_auth(client.get(url), token).send().await?;
    match resp.status() {
        StatusCode::OK => {
            let bytes = resp.bytes().await?;
            Ok(Some(snapforge_format::from_bytes(&bytes)?))
        }
        StatusCode::NOT_FOUND => Ok(None),
        _ => Err(status_error(resp, url).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_endpoint_strips_trailing_slash() {
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:8080/").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert!(normalize_endpoint("https://example.test").is_err());
    }

    #[test]
    fn normalize_endpoint_rejects_bad_scheme() {
        assert!(normalize_endpoint("").is_err());
        assert!(normalize_endpoint("/tmp/store").is_err());
        assert!(normalize_endpoint("ftp://example.test").is_err());
    }

    #[test]
    fn options_reject_zero_concurrency_and_timeout() {
        let mut o = RemoteOptions {
            concurrency: 0,
            ..RemoteOptions::default()
        };
        assert!(o.validate().is_err());
        o.concurrency = 4;
        o.request_timeout = Duration::ZERO;
        assert!(o.validate().is_err());
    }
}
