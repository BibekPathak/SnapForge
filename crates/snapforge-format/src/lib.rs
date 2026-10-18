//! Versioned, validated snapshot manifest format with atomic publication.
//!
//! This crate defines the immutable snapshot manifest, strict validation of
//! potentially-untrusted manifests, and atomic snapshot publication. It is
//! storage-agnostic: content-addressed chunk storage lives in
//! `snapforge-storage`, which builds on the [`ChunkRef`] identity defined here.

pub mod error;
pub mod manifest;
pub mod store;
pub mod validate;

pub use error::{FormatError, Result};
pub use manifest::{
    Artifact, ChunkRef, Compatibility, Manifest, SnapshotStats, WorkloadInfo, FORMAT_VERSION,
};
pub use store::{
    cleanup_staging, from_bytes, load, load_validated, publish, to_bytes, write_to_dir,
    MANIFEST_FILE,
};
pub use validate::{
    detect_parent_cycle, validate, validate_artifact_path, validate_snapshot_id, ValidManifest,
    ValidateOptions,
};
