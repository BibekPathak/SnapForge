//! Shared HTTP primitives for the remote client (pull and push).
//!
//! These are the retrying, auth-aware request helpers. Retry policy, backoff,
//! cancellation, and status classification live here so both directions of
//! transfer behave identically.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use snapforge_format::Manifest;
use snapforge_storage::CancellationToken;

use crate::error::{RemoteError, Result};
use crate::protocol::snapshot_url;
use crate::tls::TlsClientConfig;

/// Tuning for a remote session: retries, timeouts, auth, TLS, cancellation.
///
/// Shared by [`crate::push_snapshot`] and [`crate::pull_snapshot`]. The
/// direction-specific options (`PushOptions`, `PullOptions`) embed this.
#[derive(Debug, Clone)]
pub struct RemoteSession {
    /// Maximum retry attempts per request (0 means a single attempt).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_backoff: Duration,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Optional bearer token sent with every request.
    pub token: Option<String>,
    /// TLS trust configuration (used for `https://` endpoints).
    pub tls: TlsClientConfig,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

impl Default for RemoteSession {
    fn default() -> Self {
        RemoteSession {
            max_retries: 3,
            retry_backoff: Duration::from_millis(50),
            request_timeout: Duration::from_secs(30),
            token: None,
            tls: TlsClientConfig::default(),
            cancel: CancellationToken::new(),
        }
    }
}

impl RemoteSession {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.request_timeout.is_zero() {
            return Err(RemoteError::Config(
                "request_timeout must be non-zero".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn client(&self) -> Result<Client> {
        crate::tls::ensure_crypto_provider();
        let builder = Client::builder()
            .timeout(self.request_timeout)
            .user_agent(concat!("snapforge-remote/", env!("CARGO_PKG_VERSION")));
        Ok(self.tls.adds_root(builder)?.build()?)
    }
}

/// Normalize and validate a remote endpoint URL.
pub(crate) fn normalize_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(RemoteError::Config("endpoint must not be empty".into()));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(RemoteError::Config(
            "endpoint must start with http:// or https://".into(),
        ));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn with_auth(req: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    match token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

pub(crate) async fn status_error(resp: Response, endpoint: &str) -> RemoteError {
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

pub(crate) fn backoff(session: &RemoteSession, attempt: u32) -> Duration {
    session.retry_backoff.saturating_mul(1u32 << attempt.min(6))
}

pub(crate) async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> Result<()> {
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

/// Result of probing a remote chunk.
pub(crate) enum ChunkProbe {
    Present(Vec<u8>),
    Missing,
    Corrupt,
}

/// Run `op` with retries on retryable errors, honoring cancellation.
async fn with_retry<T, F, Fut>(session: &RemoteSession, retries: &AtomicU64, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        session.cancel.check()?;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < session.max_retries => {
                attempt += 1;
                retries.fetch_add(1, Ordering::SeqCst);
                sleep_or_cancel(backoff(session, attempt), &session.cancel).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) async fn retry_head(
    client: &Client,
    url: &str,
    session: &RemoteSession,
    retries: &AtomicU64,
) -> Result<bool> {
    with_retry(session, retries, || async {
        let resp = with_auth(client.head(url), session.token.as_deref())
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(status_error(resp, url).await),
        }
    })
    .await
}

pub(crate) async fn retry_get_chunk(
    client: &Client,
    url: &str,
    session: &RemoteSession,
    retries: &AtomicU64,
) -> Result<ChunkProbe> {
    with_retry(session, retries, || async {
        let resp = with_auth(client.get(url), session.token.as_deref())
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(ChunkProbe::Present(resp.bytes().await?.to_vec())),
            StatusCode::NOT_FOUND => Ok(ChunkProbe::Missing),
            StatusCode::UNPROCESSABLE_ENTITY => Ok(ChunkProbe::Corrupt),
            _ => Err(status_error(resp, url).await),
        }
    })
    .await
}

pub(crate) async fn retry_put_chunk(
    client: &Client,
    url: &str,
    data: &[u8],
    session: &RemoteSession,
    retries: &AtomicU64,
) -> Result<()> {
    with_retry(session, retries, || async {
        let req = client
            .put(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec());
        let resp = with_auth(req, session.token.as_deref()).send().await?;
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => Ok(()),
            _ => Err(status_error(resp, url).await),
        }
    })
    .await
}

pub(crate) async fn retry_put_manifest(
    client: &Client,
    url: &str,
    body: &[u8],
    session: &RemoteSession,
    retries: &AtomicU64,
) -> Result<()> {
    let result = with_retry(session, retries, || async {
        let req = client
            .put(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        let resp = with_auth(req, session.token.as_deref()).send().await?;
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(()),
            _ => Err(status_error(resp, url).await),
        }
    })
    .await;

    // A 409 means a different snapshot already occupies the id; surface it as a
    // protocol conflict rather than a raw status.
    match result {
        Err(RemoteError::Status { status: 409, .. }) => Err(RemoteError::Protocol(
            "remote already has a different snapshot with this id".into(),
        )),
        other => other,
    }
}

pub(crate) async fn retry_get_manifest(
    client: &Client,
    base: &str,
    snapshot_id: &str,
    session: &RemoteSession,
    retries: &AtomicU64,
) -> Result<Option<Manifest>> {
    let url = snapshot_url(base, snapshot_id);
    with_retry(session, retries, || async {
        let resp = with_auth(client.get(&url), session.token.as_deref())
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => {
                let bytes = resp.bytes().await?;
                Ok(Some(snapforge_format::from_bytes(&bytes)?))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(status_error(resp, &url).await),
        }
    })
    .await
}
