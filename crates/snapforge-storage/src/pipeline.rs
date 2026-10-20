//! Bounded parallel storage pipeline.
//!
//! The pipeline chunks an input and stores chunks into the CAS with **bounded
//! concurrency** and backpressure. It never spawns unbounded tasks or buffers
//! the whole input: at most `concurrency` chunks are in flight, each at most
//! `chunk_size` bytes, so peak memory is bounded by
//! `concurrency * chunk_size` plus a small constant.
//!
//! Blocking filesystem work runs on `spawn_blocking` so the async runtime
//! workers are not stalled.

use std::io::Read;
use std::sync::Arc;

use crate::cas::Cas;
use crate::chunker::Chunker;
use crate::error::{Result, StorageError};
use serde::{Deserialize, Serialize};
use snapforge_format::{Artifact, ChunkRef};
use tokio::sync::{Mutex, Semaphore};

/// Options controlling storage-pipeline behaviour.
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    /// Chunk size in bytes.
    pub chunk_size: usize,
    /// Maximum number of chunk-store operations in flight at once.
    pub concurrency: usize,
    /// Hash each chunk a second time on read-back to detect corruption
    /// (defense in depth; off by default to avoid doubling I/O).
    pub verify_on_write: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        PipelineOptions {
            chunk_size: 1024 * 1024,
            concurrency: 4,
            verify_on_write: false,
        }
    }
}

impl PipelineOptions {
    fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            return Err(StorageError::Config("concurrency must be >= 1".into()));
        }
        if self.chunk_size == 0 {
            return Err(StorageError::Config("chunk_size must be > 0".into()));
        }
        Ok(())
    }
}

/// Statistics collected while ingesting one logical artifact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestStats {
    /// Bytes read from the source.
    pub bytes_in: u64,
    /// Number of chunk references produced (before dedup).
    pub chunk_refs: u64,
    /// Number of chunks newly written to the CAS.
    pub chunks_written: u64,
    /// Number of chunks that already existed (deduplicated).
    pub chunks_deduplicated: u64,
    /// Bytes saved by deduplication.
    pub dedup_saved_bytes: u64,
    /// Peak number of chunks in flight simultaneously.
    pub peak_in_flight: usize,
    /// Wall-clock nanoseconds spent in the chunker thread (reading, hashing, and
    /// any time blocked on the bounded channel due to backpressure).
    pub chunking_time_ns: u64,
    /// Wall-clock nanoseconds spent in CAS writes (summed across workers; this
    /// is aggregate worker time, not wall time).
    pub storage_time_ns: u64,
    /// Wall-clock nanoseconds for the whole `store_reader` call.
    pub total_time_ns: u64,
}

/// The result of ingesting one source into the CAS.
#[derive(Debug, Clone)]
pub struct IngestedArtifact {
    /// The artifact descriptor (path must be supplied by the caller).
    pub chunks: Vec<ChunkRef>,
    pub size: u64,
    pub stats: IngestStats,
}

impl IngestedArtifact {
    /// Build a format [`Artifact`] for a given logical path.
    pub fn into_artifact(self, path: impl Into<String>) -> Artifact {
        Artifact {
            path: path.into(),
            size: self.size,
            chunks: self.chunks,
        }
    }
}

/// Ingest a readable source into the CAS, returning the artifact descriptor and
/// statistics.
///
/// The input is chunked and stored with bounded concurrency. Chunk order is
/// preserved so the resulting artifact can be reassembled deterministically.
pub async fn store_reader<R: Read + Send + 'static>(
    cas: Arc<Cas>,
    reader: R,
    opts: PipelineOptions,
) -> Result<IngestedArtifact> {
    opts.validate()?;
    let chunk_size = opts.chunk_size;
    let semaphore = Arc::new(Semaphore::new(opts.concurrency));
    let started = std::time::Instant::now();
    let chunking_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Chunking runs on a blocking thread; it pushes (index, ChunkRef, bytes)
    // into a bounded channel so producers block when the pipeline is saturated.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(usize, ChunkRef, Vec<u8>)>(opts.concurrency);

    let chunking_ns_writer = chunking_ns.clone();
    let chunker_handle = tokio::task::spawn_blocking(move || {
        let mut chunker = Chunker::new(reader, chunk_size);
        let mut idx = 0usize;
        let chunk_start = std::time::Instant::now();
        loop {
            match chunker.next_chunk() {
                Ok(Some(chunk)) => {
                    // Blocking send provides backpressure to the chunker.
                    if tx
                        .blocking_send((idx, chunk.reference, chunk.bytes))
                        .is_err()
                    {
                        break;
                    }
                    idx += 1;
                }
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        chunking_ns_writer.store(
            chunk_start.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
        Ok::<usize, std::io::Error>(idx)
    });

    // Collect results keyed by index; a Mutex-guarded Vec keeps ordering
    // explicit and prevents unbounded reordering buffers.
    let results: Arc<Mutex<Vec<Option<StoredChunk>>>> = Arc::new(Mutex::new(Vec::new()));
    let stats = Arc::new(Mutex::new(IngestStats::default()));
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut join_set = tokio::task::JoinSet::new();

    while let Some((idx, reference, bytes)) = rx.recv().await {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| StorageError::Join(e.to_string()))?;
        let cas = cas.clone();
        let results = results.clone();
        let stats = stats.clone();
        let in_flight = in_flight.clone();
        let verify = opts.verify_on_write;

        join_set.spawn(async move {
            let cur = in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            {
                let mut s = stats.lock().await;
                s.peak_in_flight = s.peak_in_flight.max(cur);
            }

            let id = reference.id.clone();
            let store_start = std::time::Instant::now();
            let store_result = tokio::task::spawn_blocking(move || {
                let (id, newly_written) = cas.put(&bytes)?;
                if verify {
                    cas.verify(&id)?;
                }
                Ok::<(String, bool), StorageError>((id, newly_written))
            })
            .await
            .map_err(|e| StorageError::Join(e.to_string()))?;
            let store_elapsed = store_start.elapsed().as_nanos() as u64;

            let (stored_id, newly_written) = store_result?;
            // The recomputed id must match the chunker's id.
            debug_assert_eq!(stored_id, id);

            {
                let mut s = stats.lock().await;
                s.bytes_in += reference.length;
                s.chunk_refs += 1;
                s.storage_time_ns += store_elapsed;
                if newly_written {
                    s.chunks_written += 1;
                } else {
                    s.chunks_deduplicated += 1;
                    s.dedup_saved_bytes += reference.length;
                }
            }

            let mut r = results.lock().await;
            if r.len() <= idx {
                r.resize_with(idx + 1, || None);
            }
            r[idx] = Some(StoredChunk { reference });

            in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            drop(permit);
            Ok::<(), StorageError>(())
        });
    }

    // Surface any task errors.
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(StorageError::Join(e.to_string())),
        }
    }

    chunker_handle
        .await
        .map_err(|e| StorageError::Join(e.to_string()))?
        .map_err(|e| StorageError::io_at("<input>", e))?;

    let collected = Arc::try_unwrap(results)
        .map_err(|_| StorageError::Other("results still shared".into()))?
        .into_inner();

    let mut chunks = Vec::with_capacity(collected.len());
    let mut size = 0u64;
    for slot in collected {
        let c = slot.ok_or_else(|| StorageError::Other("missing chunk result".into()))?;
        size += c.reference.length;
        chunks.push(c.reference);
    }

    let final_stats = Arc::try_unwrap(stats)
        .map_err(|_| StorageError::Other("stats still shared".into()))?
        .into_inner();

    let mut final_stats = final_stats;
    final_stats.chunking_time_ns = chunking_ns.load(std::sync::atomic::Ordering::SeqCst);
    final_stats.total_time_ns = started.elapsed().as_nanos() as u64;

    Ok(IngestedArtifact {
        chunks,
        size,
        stats: final_stats,
    })
}

#[derive(Debug, Clone)]
struct StoredChunk {
    reference: ChunkRef,
}
