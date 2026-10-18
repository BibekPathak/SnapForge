//! Streaming chunker.
//!
//! Splits an input stream into fixed-size chunks without loading the whole
//! input into memory. Each chunk is hashed (BLAKE3) to derive its
//! content-addressed id; the chunk bytes and its [`ChunkRef`] are yielded to
//! the caller.
//!
//! Chunk sizes are configurable (64 KiB, 256 KiB, 1 MiB, 4 MiB are the
//! benchmarked values). SnapForge does not assume a best size: the trade-off
//! between chunk count / metadata overhead and per-chunk I/O is measured.

use snapforge_format::ChunkRef;

/// Produce the hex-encoded BLAKE3 id and length for a chunk's bytes.
pub fn hash_chunk(bytes: &[u8]) -> ChunkRef {
    let id = blake3::hash(bytes).to_hex().to_string();
    ChunkRef {
        id,
        length: bytes.len() as u64,
    }
}

/// A chunk produced by the streaming chunker.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub reference: ChunkRef,
    pub bytes: Vec<u8>,
}

/// A streaming chunker over any [`std::io::Read`] source.
///
/// The iterator yields owned [`Chunk`] values. At most one chunk-sized buffer
/// plus the read buffer are held at a time, so memory use is bounded by the
/// configured chunk size regardless of input length.
pub struct Chunker<R: std::io::Read> {
    reader: R,
    chunk_size: usize,
    buf: Vec<u8>,
    done: bool,
}

impl<R: std::io::Read> Chunker<R> {
    /// Create a chunker with the given chunk size (must be > 0).
    pub fn new(reader: R, chunk_size: usize) -> Self {
        assert!(chunk_size > 0, "chunk size must be > 0");
        Chunker {
            reader,
            chunk_size,
            buf: vec![0u8; chunk_size],
            done: false,
        }
    }

    /// Read the next chunk. Returns `Ok(None)` at end of input.
    pub fn next_chunk(&mut self) -> std::io::Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        let mut filled = 0usize;
        while filled < self.chunk_size {
            match self.reader.read(&mut self.buf[filled..]) {
                Ok(0) => {
                    self.done = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if filled == 0 {
            return Ok(None);
        }
        let bytes = self.buf[..filled].to_vec();
        let reference = hash_chunk(&bytes);
        Ok(Some(Chunk { reference, bytes }))
    }
}

impl<R: std::io::Read> Iterator for Chunker<R> {
    type Item = std::io::Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_chunk() {
            Ok(Some(c)) => Some(Ok(c)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn hashes_are_deterministic_and_content_addressed() {
        let a = hash_chunk(b"hello world");
        let b = hash_chunk(b"hello world");
        let c = hash_chunk(b"hello worlds");
        assert_eq!(a, b);
        assert_ne!(a.id, c.id);
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        let mut ch = Chunker::new(Cursor::new(Vec::<u8>::new()), 4096);
        assert!(ch.next_chunk().unwrap().is_none());
    }

    #[test]
    fn splits_evenly() {
        let data = vec![7u8; 12_288];
        let ch = Chunker::new(Cursor::new(data), 4096);
        let chunks: Vec<_> = ch.map(|c| c.unwrap()).collect();
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.reference.length == 4096));
    }

    #[test]
    fn handles_ragged_final_chunk() {
        let data = vec![9u8; 4096 * 2 + 100];
        let ch = Chunker::new(Cursor::new(data), 4096);
        let chunks: Vec<_> = ch.map(|c| c.unwrap()).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2].reference.length, 100);
    }

    #[test]
    fn reconstructs_exactly() {
        let data: Vec<u8> = (0..(4096 * 5 + 123)).map(|i| (i % 251) as u8).collect();
        let ch = Chunker::new(Cursor::new(data.clone()), 4096);
        let mut out = Vec::new();
        for c in ch {
            let c = c.unwrap();
            out.extend_from_slice(&c.bytes);
        }
        assert_eq!(out, data);
    }

    #[test]
    fn chunk_refs_match_bytes() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let ch = Chunker::new(Cursor::new(data), 1024);
        for c in ch {
            let c = c.unwrap();
            assert_eq!(c.reference.id, blake3::hash(&c.bytes).to_hex().to_string());
            assert_eq!(c.reference.length as usize, c.bytes.len());
        }
    }
}
