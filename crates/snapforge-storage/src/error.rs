//! Errors raised by the storage layer.

use std::path::PathBuf;

/// Result alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid storage configuration: {0}")]
    Config(String),

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("chunk {id} is missing from the content-addressed store")]
    MissingChunk { id: String },

    #[error("chunk {id} is corrupt: expected {expected}, computed {actual}")]
    CorruptChunk {
        id: String,
        expected: String,
        actual: String,
    },

    #[error("snapshot manifest error: {0}")]
    Format(#[from] snapforge_format::FormatError),

    #[error("storage task failed: {0}")]
    Join(String),

    #[error("{0}")]
    Other(String),
}

impl StorageError {
    pub fn io_at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        StorageError::Io {
            path: path.into(),
            source,
        }
    }
}
