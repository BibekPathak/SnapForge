//! Runtime configuration shared across SnapForge commands.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Default on-disk location for content-addressed storage and snapshots.
///
/// `/tmp` is chosen deliberately for this environment because the home
/// filesystem is nearly full. Operators can override with `--store`.
pub const DEFAULT_STORE_DIR: &str = "/tmp/snapforge";

/// Default bounded concurrency used by storage and transfer pipelines.
///
/// Chosen conservatively so that in-flight buffers stay within the documented
/// memory budget (see `docs/architecture.md`).
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Default chunk size (1 MiB). Other sizes are benchmarked explicitly.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Fully-resolved configuration for a single SnapForge invocation.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory for content-addressed storage (`cas/`) and snapshots (`snapshots/`).
    pub store_dir: PathBuf,
    /// Bounded concurrency for storage/transfer operations.
    pub concurrency: usize,
    /// Chunk size in bytes used by the storage pipeline.
    pub chunk_size: usize,
    /// Optional per-operation timeout.
    pub timeout: Option<std::time::Duration>,
    /// Emit machine-readable JSON where a command supports it.
    pub json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            store_dir: PathBuf::from(DEFAULT_STORE_DIR),
            concurrency: DEFAULT_CONCURRENCY,
            chunk_size: DEFAULT_CHUNK_SIZE,
            timeout: None,
            json: false,
        }
    }
}

impl Config {
    /// Validate invariants that the rest of the pipeline relies on.
    pub fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(Error::Config("concurrency must be >= 1".into()));
        }
        if self.chunk_size < 4096 || !self.chunk_size.is_power_of_two() {
            return Err(Error::Config(
                "chunk-size must be a power of two and at least 4096 bytes".into(),
            ));
        }
        Ok(())
    }

    /// Directory holding the content-addressed chunk store.
    pub fn cas_dir(&self) -> PathBuf {
        self.store_dir.join("cas")
    }

    /// Directory holding published snapshot manifests and their metadata.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.store_dir.join("snapshots")
    }

    /// Staging directory for atomic publication (must be on the same filesystem
    /// as `snapshots_dir` so that renames are atomic).
    pub fn staging_dir(&self) -> PathBuf {
        self.store_dir.join("staging")
    }

    /// Create the store directory tree if it does not exist.
    pub async fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.store_dir.clone(),
            self.cas_dir(),
            self.snapshots_dir(),
            self.staging_dir(),
        ] {
            tokio_create_dir_all(&dir).await?;
        }
        Ok(())
    }
}

async fn tokio_create_dir_all(path: &Path) -> Result<()> {
    // Use std here to avoid pulling tokio into core's synchronous surface.
    std::fs::create_dir_all(path).map_err(|e| Error::io_at(path.to_path_buf(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn rejects_zero_concurrency() {
        let cfg = Config {
            concurrency: 0,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_non_power_of_two_chunk_size() {
        let cfg = Config {
            chunk_size: 1000,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_common_chunk_sizes() {
        for size in [64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
            let cfg = Config {
                chunk_size: size,
                ..Config::default()
            };
            cfg.validate().unwrap();
        }
    }
}
