//! HTTP transport adapter for SnapForge snapshot transfer.
//!
//! - [`push_snapshot`] sends a published snapshot from a local store to a
//!   remote SnapForge server.
//! - [`pull_snapshot`] fetches a snapshot from a remote server into a local
//!   store, resuming partially transferred chunks with range requests.
//! - [`serve`] exposes a local store over the same small HTTP protocol.
//!
//! Both directions preserve the local transfer guarantees: bounded
//! concurrency, integrity-checked chunks, idempotent skipping, bounded retries,
//! cancellation, and manifest publish last.
//!
//! The transport runs over plain HTTP or TLS. TLS is terminated **in-process**
//! with `rustls` (the `ring` provider): configure [`TlsServerConfig`] on the
//! server and [`TlsClientConfig`] on the client. Plain HTTP is still fine
//! behind a TLS-terminating reverse proxy. Access control uses scoped bearer
//! tokens ([`TokenSet`]): `Read` permits fetching, `Write` permits uploading
//! and publishing.

mod auth;
mod client;
mod error;
mod http;
mod protocol;
mod pull;
mod server;
mod tls;

pub use auth::{now_unix, Scope, TokenEntry, TokenSet};
pub use client::{
    fetch_manifest, health, push_snapshot, PushOptions, PushOutcome, RemoteOptions, RemoteOutcome,
};
pub use error::{RemoteError, Result};
pub use http::RemoteSession;
pub use protocol::{chunk_url, health_url, is_hex64, snapshot_url, PROTOCOL_VERSION};
pub use pull::{new_cancellation, pull_snapshot, verify_pulled, PullOptions, PullOutcome};
pub use server::{serve, ServerOptions, DEFAULT_MAX_BODY_BYTES};
pub use tls::{
    certificate_fingerprint, ensure_crypto_provider as install_crypto_provider, is_https,
    TlsClientConfig, TlsServerConfig,
};
