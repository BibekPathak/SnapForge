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
//! cancellation, and manifest publish last. The transport is plain HTTP. Put
//! TLS in front of it (a reverse proxy) when crossing untrusted networks, and
//! set a bearer token with [`ServerOptions`] / [`RemoteSession`] to
//! authenticate peers.

mod client;
mod error;
mod http;
mod protocol;
mod pull;
mod server;

pub use client::{
    fetch_manifest, health, push_snapshot, PushOptions, PushOutcome, RemoteOptions, RemoteOutcome,
};
pub use error::{RemoteError, Result};
pub use http::RemoteSession;
pub use protocol::{chunk_url, health_url, is_hex64, snapshot_url, PROTOCOL_VERSION};
pub use pull::{new_cancellation, pull_snapshot, verify_pulled, PullOptions, PullOutcome};
pub use server::{serve, ServerOptions, DEFAULT_MAX_BODY_BYTES};
