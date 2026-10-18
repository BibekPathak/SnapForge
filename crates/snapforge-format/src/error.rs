//! Errors raised by the snapshot format layer.

use std::path::PathBuf;

/// Result alias for format operations.
pub type Result<T> = std::result::Result<T, FormatError>;

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("unsupported manifest format version {found} (supported: {supported})")]
    UnsupportedVersion { found: u32, supported: u32 },

    #[error("invalid snapshot id: {0}")]
    InvalidSnapshotId(String),

    #[error("artifact path is absolute, which is not allowed: {0}")]
    AbsolutePath(String),

    #[error("artifact path escapes the snapshot root (path traversal): {0}")]
    PathTraversal(String),

    #[error("artifact path contains an empty or `.` component: {0}")]
    InvalidPathComponent(String),

    #[error("duplicate artifact path: {0}")]
    DuplicateArtifact(String),

    #[error("duplicate chunk id: {0}")]
    DuplicateChunk(String),

    #[error("artifact `{path}` has an invalid span: offset={offset} length={length} size={size}")]
    InvalidSpan {
        path: String,
        offset: u64,
        length: u64,
        size: u64,
    },

    #[error("artifact `{path}` references an unknown chunk: {chunk}")]
    UnknownChunk { path: String, chunk: String },

    #[error("chunk `{id}` length {length} does not fit within artifact `{path}` (size {size})")]
    ChunkOutOfBounds {
        id: String,
        length: u64,
        path: String,
        size: u64,
    },

    #[error("snapshot declares {declared} artifacts but lists {listed}")]
    ArtifactCountMismatch { declared: u64, listed: u64 },

    #[error("chunk map is incomplete for artifact `{path}`: covers {covered} of {size} bytes")]
    IncompleteChunkMap {
        path: String,
        covered: u64,
        size: u64,
    },

    #[error("integrity mismatch for `{what}`: expected {expected}, computed {actual}")]
    IntegrityMismatch {
        what: String,
        expected: String,
        actual: String,
    },

    #[error("parent snapshot reference cycle detected involving: {chain}")]
    ParentCycle { chain: String },

    #[error("self-referential parent snapshot id: {0}")]
    SelfParent(String),

    #[error("snapshot staging/publication error: {0}")]
    Publication(String),

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("manifest is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl FormatError {
    pub fn io_at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        FormatError::Io {
            path: path.into(),
            source,
        }
    }
}
