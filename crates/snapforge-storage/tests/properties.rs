//! Property-based tests for chunk mapping and pipeline invariants.

use std::io::Cursor;
use std::sync::Arc;

use proptest::prelude::*;
use snapforge_storage::{store_reader, Cas, Chunker, PipelineOptions};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Chunk refs always describe exactly the bytes they were derived from, and
    /// chunking any input reassembles to the original regardless of chunk size.
    #[test]
    fn chunking_round_trips(data in prop::collection::vec(any::<u8>(), 0..50_000), size in 1usize..8192) {
        let chunker = Chunker::new(Cursor::new(data.clone()), size);
        let mut out = Vec::new();
        for c in chunker {
            let c = c.unwrap();
            prop_assert_eq!(c.reference.id.clone(), blake3::hash(&c.bytes).to_hex().to_string());
            prop_assert_eq!(c.reference.length as usize, c.bytes.len());
            out.extend_from_slice(&c.bytes);
        }
        prop_assert_eq!(out, data);
    }

    /// Ingesting any input stores it so that reassembly from the CAS is exact.
    #[test]
    fn pipeline_reassembly_is_exact(data in prop::collection::vec(any::<u8>(), 0..40_000), size in 512usize..8192, conc in 1usize..5) {
        let rt = runtime();
        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(Cas::open(tmp.path()).unwrap());
        let ingested = rt.block_on(async {
            store_reader(cas.clone(), Cursor::new(data.clone()), PipelineOptions {
                chunk_size: size,
                concurrency: conc,
                verify_on_write: false,
            }).await.unwrap()
        });
        prop_assert_eq!(ingested.size as usize, data.len());
        let mut out = Vec::with_capacity(data.len());
        for chunk in &ingested.chunks {
            out.extend_from_slice(&cas.get(&chunk.id).unwrap());
        }
        prop_assert_eq!(out, data);
    }

    /// Dedup accounting is consistent: written + deduplicated == chunk refs.
    #[test]
    fn dedup_accounting_consistent(data in prop::collection::vec(any::<u8>(), 0..30_000)) {
        let rt = runtime();
        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(Cas::open(tmp.path()).unwrap());
        let ingested = rt.block_on(async {
            store_reader(cas, Cursor::new(data), PipelineOptions {
                chunk_size: 2048,
                concurrency: 4,
                verify_on_write: false,
            }).await.unwrap()
        });
        prop_assert_eq!(
            ingested.stats.chunks_written + ingested.stats.chunks_deduplicated,
            ingested.chunks.len() as u64
        );
    }
}
