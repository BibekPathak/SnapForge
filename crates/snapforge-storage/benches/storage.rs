//! Criterion microbenchmarks for the storage layer.
//!
//! Measures the trade-off between chunk size and throughput/hashing/peak-memory
//! characteristics. Run with `cargo bench -p snapforge-storage`.

use std::io::Cursor;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use snapforge_storage::{hash_chunk, store_reader, Cas, Chunker, PipelineOptions};

fn bench_chunking(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunking");
    let sizes = [64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024];
    // 16 MiB of deterministic data.
    let data: Vec<u8> = (0..16 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    group.throughput(Throughput::Bytes(data.len() as u64));
    for &size in &sizes {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let chunker = Chunker::new(Cursor::new(&data), size);
                let mut total = 0u64;
                for chunk in chunker {
                    let chunk = chunk.unwrap();
                    total += chunk.reference.length;
                }
                assert_eq!(total, data.len() as u64);
            });
        });
    }
    group.finish();
}

fn bench_hashing(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashing");
    let sizes = [64 * 1024usize, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024];
    for &size in &sizes {
        let buf = vec![0xABu8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &buf, |b, buf| {
            b.iter(|| hash_chunk(buf));
        });
    }
    group.finish();
}

fn bench_pipeline(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("pipeline");
    let data: Vec<u8> = (0..16 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("ingest_1mib_c4", |b| {
        b.iter(|| {
            let tmp = tempfile::tempdir().unwrap();
            let cas = Arc::new(Cas::open(tmp.path()).unwrap());
            rt.block_on(async {
                store_reader(
                    cas,
                    Cursor::new(data.clone()),
                    PipelineOptions {
                        chunk_size: 1024 * 1024,
                        concurrency: 4,
                        verify_on_write: false,
                    },
                )
                .await
                .unwrap()
            })
        });
    });
    group.finish();
}

criterion_group!(benches, bench_chunking, bench_hashing, bench_pipeline);
criterion_main!(benches);
