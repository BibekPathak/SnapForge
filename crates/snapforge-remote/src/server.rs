//! HTTP server that exposes a local SnapForge store for remote transfer.
//!
//! The server is intentionally small: it validates chunk content ids and
//! manifest structure/integrity, refuses to overwrite a different snapshot
//! with the same id, and publishes manifests atomically. It never trusts
//! client-supplied hashes or paths.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use snapforge_format::{FormatError, Manifest, ValidateOptions};
use snapforge_storage::{Cas, StorageError};
use tokio::net::TcpListener;

use crate::error::{RemoteError, Result};
use crate::protocol::{is_hex64, PROTOCOL_VERSION};

/// Default maximum request body (chunks or manifest) accepted by the server.
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Options controlling the remote server.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Root store directory (contains `cas/`, `snapshots/`, `staging/`).
    pub store_dir: PathBuf,
    /// Optional bearer token required on every `/v1` route except health.
    pub token: Option<String>,
    /// Maximum accepted request body size.
    pub max_body_bytes: usize,
}

impl ServerOptions {
    pub fn new(store_dir: impl Into<PathBuf>) -> Self {
        ServerOptions {
            store_dir: store_dir.into(),
            token: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.max_body_bytes == 0 {
            return Err(RemoteError::Config("max_body_bytes must be >= 1".into()));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    cas: Arc<Cas>,
    snapshots: PathBuf,
    staging: PathBuf,
    token: Option<String>,
    max_body_bytes: usize,
    counter: Arc<AtomicU64>,
}

impl AppState {
    fn new(opts: &ServerOptions) -> Result<Self> {
        let cas = Arc::new(Cas::open(opts.store_dir.join("cas"))?);
        let snapshots = opts.store_dir.join("snapshots");
        let staging = opts.store_dir.join("staging");
        std::fs::create_dir_all(&snapshots)
            .map_err(|e| RemoteError::Storage(StorageError::io_at(snapshots.clone(), e)))?;
        std::fs::create_dir_all(&staging)
            .map_err(|e| RemoteError::Storage(StorageError::io_at(staging.clone(), e)))?;
        Ok(AppState {
            cas,
            snapshots,
            staging,
            token: opts.token.clone(),
            max_body_bytes: opts.max_body_bytes,
            counter: Arc::new(AtomicU64::new(0)),
        })
    }

    fn authorize(&self, headers: &HeaderMap) -> std::result::Result<(), ApiError> {
        let Some(expected) = &self.token else {
            return Ok(());
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid bearer token",
            ))
        }
    }

    fn staging_for(&self, id: &str) -> PathBuf {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        self.staging
            .join(format!(".incoming-{id}-{}-{n}", std::process::id()))
    }
}

/// Compare two byte strings without short-circuiting on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    fn internal(e: impl std::fmt::Display) -> Self {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    }

    fn not_found(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    fn bad_request(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::CONFLICT, "conflict", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.message, "code": self.code })),
        )
            .into_response()
    }
}

impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::MissingChunk { id } => {
                ApiError::not_found(format!("chunk {id} not found"))
            }
            StorageError::CorruptChunk { .. } => ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "corrupt_chunk",
                e.to_string(),
            ),
            StorageError::Config(_) => ApiError::bad_request(e.to_string()),
            StorageError::Format(err) => ApiError::from(err),
            other => ApiError::internal(other),
        }
    }
}

impl From<FormatError> for ApiError {
    fn from(e: FormatError) -> Self {
        ApiError::bad_request(e.to_string())
    }
}

/// Serve the remote protocol on `listener` until the process is interrupted.
pub async fn serve(listener: TcpListener, opts: ServerOptions) -> Result<()> {
    opts.validate()?;
    let state = AppState::new(&opts)?;
    let app = Router::new()
        .route("/v1/health", get(health))
        .route(
            "/v1/chunks/{id}",
            get(get_chunk).head(head_chunk).put(put_chunk),
        )
        .route("/v1/snapshots/{id}", get(get_manifest).put(put_manifest))
        .with_state(state.clone())
        .layer(DefaultBodyLimit::max(state.max_body_bytes));

    tracing::info!(
        address = %listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into()),
        store = %opts.store_dir.display(),
        "serving snapforge remote protocol"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| RemoteError::Protocol(format!("server error: {e}")))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "protocol": PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn head_chunk(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> std::result::Result<StatusCode, ApiError> {
    state.authorize(&headers)?;
    if !is_hex64(&id) || !state.cas.contains(&id) {
        return Err(ApiError::not_found(format!("chunk {id} not found")));
    }
    Ok(StatusCode::OK)
}

async fn get_chunk(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> std::result::Result<([(header::HeaderName, &'static str); 1], Vec<u8>), ApiError> {
    state.authorize(&headers)?;
    let cas = state.cas.clone();
    let id_for_task = id.clone();
    let bytes = tokio::task::spawn_blocking(move || cas.get(&id_for_task))
        .await
        .map_err(|e| ApiError::internal(format!("join error: {e}")))??;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

async fn put_chunk(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<StatusCode, ApiError> {
    state.authorize(&headers)?;
    if !is_hex64(&id) {
        return Err(ApiError::bad_request(
            "chunk id must be 64 lowercase hexadecimal characters",
        ));
    }
    let cas = state.cas.clone();
    let id_for_task = id.clone();
    let created = tokio::task::spawn_blocking(move || -> std::result::Result<bool, ApiError> {
        let computed = blake3::hash(&body).to_hex().to_string();
        if computed != id_for_task {
            return Err(ApiError::bad_request(format!(
                "chunk id mismatch: body hashes to {computed}"
            )));
        }
        // Deduplicate, but never keep a corrupt file that happens to have the
        // right name: verify an existing chunk and repair it if it is bad.
        if cas.contains(&id_for_task) {
            if cas.verify(&id_for_task).is_ok() {
                return Ok(false);
            }
            match std::fs::remove_file(cas.chunk_path(&id_for_task)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ApiError::internal(format!("repairing corrupt chunk: {e}"))),
            }
        }
        let (_, created) = cas.put(&body)?;
        Ok(created)
    })
    .await
    .map_err(|e| ApiError::internal(format!("join error: {e}")))??;

    Ok(if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    })
}

async fn get_manifest(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> std::result::Result<Json<Manifest>, ApiError> {
    state.authorize(&headers)?;
    if snapforge_format::validate_snapshot_id(&id).is_err() {
        return Err(ApiError::not_found(format!("snapshot {id} not found")));
    }
    let dir = state.snapshots.join(&id);
    let manifest = tokio::task::spawn_blocking(move || snapforge_format::load(&dir))
        .await
        .map_err(|e| ApiError::internal(format!("join error: {e}")))?
        .map_err(|e| match e {
            FormatError::Io { .. } => ApiError::not_found(format!("snapshot {id} not found")),
            other => ApiError::from(other),
        })?;
    Ok(Json(manifest))
}

async fn put_manifest(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<StatusCode, ApiError> {
    state.authorize(&headers)?;
    snapforge_format::validate_snapshot_id(&id)?;
    let manifest = snapforge_format::from_bytes(&body)?;
    if manifest.snapshot_id != id {
        return Err(ApiError::bad_request(format!(
            "manifest snapshot_id `{}` does not match URL id `{id}`",
            manifest.snapshot_id
        )));
    }

    let state_for_task = state.clone();
    tokio::task::spawn_blocking(move || publish_manifest(&state_for_task, &id, manifest))
        .await
        .map_err(|e| ApiError::internal(format!("join error: {e}")))?
}

fn publish_manifest(
    state: &AppState,
    id: &str,
    manifest: Manifest,
) -> std::result::Result<StatusCode, ApiError> {
    let dest = state.snapshots.join(id);
    let available: BTreeSet<String> = state.cas.list()?.into_iter().collect();
    let validate_opts = ValidateOptions {
        check_integrity: true,
        available_chunks: Some(available),
    };

    // Idempotent: an identical already-published snapshot is a success.
    if dest.is_dir() {
        match snapforge_format::load_validated(&dest, &validate_opts) {
            Ok(existing) if existing.manifest() == &manifest => return Ok(StatusCode::OK),
            Ok(_) => {
                return Err(ApiError::conflict(format!(
                    "snapshot `{id}` already exists with different contents"
                )))
            }
            Err(_) => {
                return Err(ApiError::conflict(format!(
                    "snapshot `{id}` exists but is invalid; refusing to overwrite"
                )))
            }
        }
    }

    // Reject before writing anything if a referenced chunk is absent or the
    // manifest is structurally invalid.
    snapforge_format::validate(&manifest, &validate_opts)?;

    let staging = state.staging_for(id);
    if staging.exists() {
        snapforge_format::cleanup_staging(&staging);
    }
    if let Err(e) = snapforge_format::write_to_dir(&staging, &manifest) {
        snapforge_format::cleanup_staging(&staging);
        return Err(ApiError::from(e));
    }

    match snapforge_format::publish(
        &staging,
        &state.snapshots,
        &ValidateOptions {
            check_integrity: true,
            available_chunks: None,
        },
    ) {
        Ok(_) => Ok(StatusCode::CREATED),
        Err(e) => {
            snapforge_format::cleanup_staging(&staging);
            // A concurrent request may have published the same snapshot first.
            if dest.is_dir() {
                if let Ok(existing) = snapforge_format::load_validated(&dest, &validate_opts) {
                    if existing.manifest() == &manifest {
                        return Ok(StatusCode::OK);
                    }
                }
                return Err(ApiError::conflict(format!(
                    "snapshot `{id}` was published concurrently with different contents"
                )));
            }
            Err(ApiError::from(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn server_options_reject_zero_body_limit() {
        let mut o = ServerOptions::new("/tmp/x");
        o.max_body_bytes = 0;
        assert!(o.validate().is_err());
    }
}
