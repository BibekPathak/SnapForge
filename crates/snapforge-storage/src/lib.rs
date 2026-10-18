//! Content-addressed chunk storage and a bounded parallel storage pipeline.
//!
//! This crate implements the storage side of SnapForge's snapshot model:
//!
//! - [`chunker`] streams an input into fixed-size, content-hashed chunks.
//! - [`cas`] is a content-addressed chunk store with atomic, deduplicating
//!   writes and integrity-checked reads.
//! - [`pipeline`] ingests a source into the CAS with bounded concurrency and
//!   backpressure, so memory stays within a documented budget.
//!
//! It never reads an entire snapshot into RAM and never exposes a partially
//! written chunk as valid.

pub mod cas;
pub mod chunker;
pub mod error;
pub mod pipeline;

pub use cas::Cas;
pub use chunker::{hash_chunk, Chunk, Chunker};
pub use error::{Result, StorageError};
pub use pipeline::{store_reader, IngestStats, IngestedArtifact, PipelineOptions};
