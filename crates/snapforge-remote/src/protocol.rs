//! Wire protocol helpers shared by the client and server.
//!
//! SnapForge's remote transport is a small HTTP/1.1 protocol under `/v1`:
//!
//! | Method | Path                  | Meaning                                  |
//! |--------|-----------------------|------------------------------------------|
//! | GET    | `/v1/health`          | Liveness + protocol version              |
//! | HEAD   | `/v1/chunks/{id}`     | Does the remote already hold this chunk? |
//! | GET    | `/v1/chunks/{id}`     | Fetch a chunk (integrity-checked)        |
//! | PUT    | `/v1/chunks/{id}`     | Store a chunk; server verifies its id    |
//! | GET    | `/v1/snapshots/{id}`  | Fetch a published manifest               |
//! | PUT    | `/v1/snapshots/{id}`  | Publish a manifest (all chunks required) |
//!
//! Transport is plain HTTP; TLS is expected to be terminated by a reverse
//! proxy. An optional bearer token can protect the endpoints.

/// Protocol identifier reported by `/v1/health` and the server startup line.
pub const PROTOCOL_VERSION: &str = "snapforge-http-v1";

/// True for a 64-character lowercase hexadecimal BLAKE3 digest.
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `{base}/v1/health`
pub fn health_url(base: &str) -> String {
    format!("{base}/v1/health")
}

/// `{base}/v1/chunks/{id}`
pub fn chunk_url(base: &str, id: &str) -> String {
    format!("{base}/v1/chunks/{id}")
}

/// `{base}/v1/snapshots/{id}`
pub fn snapshot_url(base: &str, id: &str) -> String {
    format!("{base}/v1/snapshots/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex64_accepts_digests_only() {
        assert!(is_hex64(&"a".repeat(64)));
        assert!(is_hex64(&"0123456789abcdef".repeat(4)));
        assert!(!is_hex64(&"a".repeat(63)));
        assert!(!is_hex64(&"A".repeat(64)));
        assert!(!is_hex64(&"g".repeat(64)));
    }

    #[test]
    fn urls_are_well_formed() {
        assert_eq!(health_url("http://x"), "http://x/v1/health");
        assert_eq!(chunk_url("http://x", "abc"), "http://x/v1/chunks/abc");
        assert_eq!(
            snapshot_url("http://x", "snap"),
            "http://x/v1/snapshots/snap"
        );
    }
}
