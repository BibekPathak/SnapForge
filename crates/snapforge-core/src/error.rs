//! Error types for SnapForge.

use std::path::PathBuf;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors surfaced by SnapForge.
///
/// These are intentionally explicit: the project must never collapse a real
/// failure (for example a CRIU error) into a silent success.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("i/o error: {0}")]
    PlainIo(#[from] std::io::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("host capability missing: {0}")]
    CapabilityMissing(String),

    #[error("external command `{program}` failed (exit {status}): {stderr}")]
    CommandFailed {
        program: String,
        status: String,
        stderr: String,
    },

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Attach a path to a bare I/O error for better diagnostics.
    pub fn io_at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}
