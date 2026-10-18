//! Content-addressed chunk store (CAS).
//!
//! Chunks are stored at `cas/<blake3-hex>`. Writes are atomic (write to a
//! temporary file on the same filesystem, then `rename`), so a partially
//! written chunk is never observable under its content id. Writing an existing
//! chunk is a no-op (deduplication): identical content has an identical id and
//! therefore a single stored copy.

use std::path::{Path, PathBuf};

use crate::error::{Result, StorageError};

/// A handle to an on-disk content-addressed store rooted at `cas_dir`.
#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    /// Open (creating if needed) a CAS rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| StorageError::io_at(root.clone(), e))?;
        Ok(Cas { root })
    }

    /// Root directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for a given chunk id.
    pub fn chunk_path(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// True if a chunk with this id is already stored.
    pub fn contains(&self, id: &str) -> bool {
        self.chunk_path(id).is_file()
    }

    /// Store a chunk's bytes, returning `true` if newly written and `false` if
    /// it already existed (deduplicated). The id is recomputed from the bytes
    /// so the caller cannot mislabel content.
    ///
    /// The write is atomic: bytes go to a unique temporary file, are fsync-free
    /// but flushed, then renamed into place.
    pub fn put(&self, bytes: &[u8]) -> Result<(String, bool)> {
        let id = blake3::hash(bytes).to_hex().to_string();
        let final_path = self.chunk_path(&id);
        if final_path.is_file() {
            return Ok((id, false));
        }

        // Unique temp name to avoid collisions between concurrent writers.
        let tmp = self.root.join(format!(".tmp-{id}-{}", std::process::id()));
        {
            use std::io::Write;
            let mut f =
                std::fs::File::create(&tmp).map_err(|e| StorageError::io_at(tmp.clone(), e))?;
            f.write_all(bytes)
                .map_err(|e| StorageError::io_at(tmp.clone(), e))?;
            f.flush().map_err(|e| StorageError::io_at(tmp.clone(), e))?;
        }
        // Atomic publish. If another writer won the race, this rename still
        // produces the same content at the same id, so it is idempotent.
        match std::fs::rename(&tmp, &final_path) {
            Ok(()) => Ok((id, true)),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                // If the file appeared concurrently, treat as deduplicated.
                if final_path.is_file() {
                    Ok((id, false))
                } else {
                    Err(StorageError::io_at(final_path, e))
                }
            }
        }
    }

    /// Read a chunk's bytes, verifying its content id. A mismatch (corruption)
    /// is an error, never a silent pass.
    pub fn get(&self, id: &str) -> Result<Vec<u8>> {
        let path = self.chunk_path(id);
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::MissingChunk { id: id.to_string() }
            } else {
                StorageError::io_at(path.clone(), e)
            }
        })?;
        let actual = blake3::hash(&bytes).to_hex().to_string();
        if actual != id {
            return Err(StorageError::CorruptChunk {
                id: id.to_string(),
                expected: id.to_string(),
                actual,
            });
        }
        Ok(bytes)
    }

    /// Verify a chunk exists and its content matches its id, without returning
    /// the bytes to the caller.
    pub fn verify(&self, id: &str) -> Result<()> {
        self.get(id).map(|_| ())
    }

    /// List all chunk ids currently stored.
    pub fn list(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let rd =
            std::fs::read_dir(&self.root).map_err(|e| StorageError::io_at(self.root.clone(), e))?;
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') && entry.path().is_file() {
                    ids.push(name.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path()).unwrap();
        let (id, new) = cas.put(b"hello").unwrap();
        assert!(new);
        assert_eq!(cas.get(&id).unwrap(), b"hello");
    }

    #[test]
    fn put_is_idempotent_and_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path()).unwrap();
        let (id1, new1) = cas.put(b"same bytes").unwrap();
        let (id2, new2) = cas.put(b"same bytes").unwrap();
        assert_eq!(id1, id2);
        assert!(new1);
        assert!(!new2, "second put must deduplicate");
        assert_eq!(cas.list().unwrap().len(), 1);
    }

    #[test]
    fn get_missing_reports_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path()).unwrap();
        let err = cas.get("0".repeat(64).as_str()).unwrap_err();
        assert!(matches!(err, StorageError::MissingChunk { .. }));
    }

    #[test]
    fn corrupt_chunk_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path()).unwrap();
        let (id, _) = cas.put(b"good").unwrap();
        // Overwrite the chunk file with different bytes.
        std::fs::write(cas.chunk_path(&id), b"tampered").unwrap();
        let err = cas.get(&id).unwrap_err();
        assert!(matches!(err, StorageError::CorruptChunk { .. }));
    }

    #[test]
    fn list_ignores_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path()).unwrap();
        cas.put(b"a").unwrap();
        std::fs::write(tmp.path().join(".tmp-leftover"), b"x").unwrap();
        assert_eq!(cas.list().unwrap().len(), 1);
    }
}
