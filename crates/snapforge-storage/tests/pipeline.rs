//! Integration tests for the storage pipeline.
//!
//! These verify end-to-end ingest behaviour: exact reassembly, deduplication
//! across and within sources, bounded concurrency (peak in-flight), and that a
//! corrupt store entry is detected on read-back.

use std::io::Cursor;
use std::sync::Arc;

use snapforge_storage::{store_reader, Cas, PipelineOptions};

fn opts(chunk_size: usize, concurrency: usize) -> PipelineOptions {
    PipelineOptions {
        chunk_size,
        concurrency,
        verify_on_write: false,
    }
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn ingest_reassembles_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    let input = data(100_000);

    let ingested = store_reader(cas.clone(), Cursor::new(input.clone()), opts(4096, 4))
        .await
        .unwrap();

    // Reassemble from the CAS using the recorded chunk refs.
    let mut out = Vec::new();
    for chunk in &ingested.chunks {
        out.extend_from_slice(&cas.get(&chunk.id).unwrap());
    }
    assert_eq!(out, input);
    assert_eq!(ingested.size, input.len() as u64);
    assert_eq!(ingested.stats.bytes_in, input.len() as u64);
}

#[tokio::test]
async fn ingest_deduplicates_identical_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    // All-identical content so every chunk has the same id.
    let input = vec![0u8; 40_000];

    let ingested = store_reader(cas.clone(), Cursor::new(input), opts(4096, 4))
        .await
        .unwrap();

    assert!(
        ingested.stats.chunks_written >= 1,
        "expected at least one written chunk"
    );
    assert!(
        ingested.stats.chunks_deduplicated > 0,
        "expected deduplicated chunks, got {:?}",
        ingested.stats
    );
    assert!(ingested.stats.dedup_saved_bytes > 0);
    // Distinct chunk ids should be far fewer than refs.
    let distinct: std::collections::BTreeSet<_> =
        ingested.chunks.iter().map(|c| c.id.clone()).collect();
    assert!(distinct.len() < ingested.chunks.len());
}

#[tokio::test]
async fn dedup_across_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    let input = data(20_000);

    let first = store_reader(cas.clone(), Cursor::new(input.clone()), opts(4096, 2))
        .await
        .unwrap();
    let second = store_reader(cas.clone(), Cursor::new(input), opts(4096, 2))
        .await
        .unwrap();

    assert_eq!(
        first.stats.chunks_written + first.stats.chunks_deduplicated,
        first.chunks.len() as u64
    );
    // Second ingest of the same bytes should write nothing new.
    assert_eq!(second.stats.chunks_written, 0);
    assert_eq!(second.stats.chunks_deduplicated, second.chunks.len() as u64);
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    let input = data(200_000);

    let ingested = store_reader(cas, Cursor::new(input), opts(4096, 3))
        .await
        .unwrap();

    // Peak in-flight must never exceed the configured bound (plus slack for the
    // accounting happening inside the task, so we assert <= concurrency).
    assert!(
        ingested.stats.peak_in_flight <= 3,
        "peak in flight {} exceeded bound",
        ingested.stats.peak_in_flight
    );
}

#[tokio::test]
async fn rejects_zero_concurrency() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    let err = store_reader(cas, Cursor::new(vec![1u8; 10]), opts(4096, 0))
        .await
        .unwrap_err();
    assert!(matches!(err, snapforge_storage::StorageError::Config(_)));
}

#[tokio::test]
async fn corrupt_chunk_detected_on_readback() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    let ingested = store_reader(cas.clone(), Cursor::new(data(10_000)), opts(4096, 2))
        .await
        .unwrap();

    // Corrupt the first stored chunk on disk.
    let victim = &ingested.chunks[0].id;
    std::fs::write(cas.chunk_path(victim), b"corrupted").unwrap();
    let err = cas.get(victim).unwrap_err();
    assert!(matches!(
        err,
        snapforge_storage::StorageError::CorruptChunk { .. }
    ));
}

#[tokio::test]
async fn empty_input_yields_empty_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let cas = Arc::new(Cas::open(tmp.path()).unwrap());
    let ingested = store_reader(cas, Cursor::new(Vec::<u8>::new()), opts(4096, 2))
        .await
        .unwrap();
    assert_eq!(ingested.size, 0);
    assert!(ingested.chunks.is_empty());
}

#[tokio::test]
async fn varied_chunk_sizes_produce_same_reassembly() {
    let input = data(50_000);
    for size in [4096usize, 16 * 1024, 64 * 1024, 256 * 1024] {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(Cas::open(tmp.path()).unwrap());
        let ingested = store_reader(cas.clone(), Cursor::new(input.clone()), opts(size, 4))
            .await
            .unwrap();
        let mut out = Vec::new();
        for chunk in &ingested.chunks {
            out.extend_from_slice(&cas.get(&chunk.id).unwrap());
        }
        assert_eq!(out, input, "reassembly failed for chunk size {size}");
    }
}
