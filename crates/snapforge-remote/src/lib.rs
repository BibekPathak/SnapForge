//! HTTP transport adapter for SnapForge snapshot transfer.
//!
//! Two halves:
//!
//! - [`push_snapshot`] sends a published snapshot from a local store to a
//!   remote SnapForge server. It preserves the local transfer guarantees:
//!   bounded concurrency, integrity-checked chunks, idempotent skipping,
//!   bounded retries, cancellation, and manifest publish last.
//! - [`serve`] exposes a local store over the same small HTTP protocol,
//!   validating chunk content ids and manifest integrity before accepting
//!   data and publishing manifests atomically.
//!
//! The transport is plain HTTP. Put TLS in front of it (a reverse proxy) when
//! crossing untrusted networks, and set a bearer token with [`ServerOptions`]
//! / [`RemoteOptions`] to authenticate peers.

mod client;
mod error;
mod protocol;
mod server;

pub use client::{health, push_snapshot, RemoteOptions, RemoteOutcome};
pub use error::{RemoteError, Result};
pub use protocol::{chunk_url, health_url, is_hex64, snapshot_url, PROTOCOL_VERSION};
pub use server::{serve, ServerOptions, DEFAULT_MAX_BODY_BYTES};
