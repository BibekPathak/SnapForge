//! Errors raised by the CRIU orchestration engine.

use std::path::PathBuf;

use snapforge_core::host::CriuReadiness;

/// Result alias for engine operations.
pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("criu binary not found on PATH")]
    CriuNotFound,

    #[error("host is not capable of CRIU checkpoint/restore: {readiness:?}")]
    HostNotCapable { readiness: CriuReadiness },

    #[error("invalid engine configuration: {0}")]
    Config(String),

    #[error("target process {pid} does not exist or is not accessible")]
    TargetNotFound { pid: i32 },

    #[error("criu `{op}` failed (exit {code:?}): {stderr}")]
    CriuFailed {
        op: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("criu `{op}` did not exit within {seconds}s and was killed")]
    Timeout { op: String, seconds: u64 },

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("checkpoint directory {0} is empty after a reported-successful dump")]
    EmptyCheckpoint(PathBuf),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl EngineError {
    /// Attach a path to a bare I/O error.
    pub fn io_at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        EngineError::Io {
            path: path.into(),
            source,
        }
    }

    /// True when the failure is a missing host capability rather than a
    /// genuine operation failure.
    pub fn is_capability_block(&self) -> bool {
        matches!(
            self,
            EngineError::CriuNotFound | EngineError::HostNotCapable { .. }
        )
    }
}

impl From<snapforge_core::Error> for EngineError {
    fn from(value: snapforge_core::Error) -> Self {
        EngineError::Other(value.to_string())
    }
}
