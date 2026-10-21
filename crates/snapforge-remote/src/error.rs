//! Errors raised by the remote transport.

use snapforge_format::FormatError;
use snapforge_storage::StorageError;

/// Result alias for remote operations.
pub type Result<T> = std::result::Result<T, RemoteError>;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("invalid remote configuration: {0}")]
    Config(String),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("manifest error: {0}")]
    Format(#[from] FormatError),

    #[error("remote rejected `{endpoint}` with status {status}: {body}")]
    Status {
        status: u16,
        endpoint: String,
        body: String,
    },

    #[error("remote authentication failed (missing or invalid bearer token)")]
    Unauthorized,

    #[error("remote does not have chunk {0}")]
    MissingChunk(String),

    #[error("remote protocol error: {0}")]
    Protocol(String),

    #[error("a blocking task failed: {0}")]
    Join(String),
}

impl RemoteError {
    /// Whether retrying the same request could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            RemoteError::Http(_) => true,
            RemoteError::Status { status, .. } => *status == 429 || (500..600).contains(status),
            _ => false,
        }
    }

    /// Whether the error indicates a missing remote chunk (404 on GET).
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, RemoteError::Unauthorized)
    }
}
