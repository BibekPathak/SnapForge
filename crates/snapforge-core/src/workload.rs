//! Deterministic workload harness.
//!
//! A workload is a controlled, reproducible program that SnapForge can
//! checkpoint and restore. It maintains a known memory pattern, counters, and
//! application state, writes a JSON state file, and can later *validate* that
//! state after a restore.
//!
//! The harness is intentionally conservative: it uses only constructs that are
//! compatible with CRIU (anonymous memory, plain threads, regular file-backed
//! mappings and descriptors) and documents anything it cannot guarantee.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Kind of workload; selects which subsystems are exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    CpuMemory,
    FileHeavy,
    Multithreaded,
}

/// Parameters for a workload run. Identical parameters must produce identical
/// state, so that snapshots are reproducible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadParams {
    pub kind: WorkloadKind,
    pub memory_mib: u64,
    pub threads: usize,
    pub iterations: u64,
    pub files: usize,
    pub file_mib: u64,
}

/// Derived, verifiable state. Written before execution and re-checked after a
/// restore to prove the restored process is genuinely the same workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadState {
    pub kind: WorkloadKind,
    pub memory_mib: u64,
    pub threads: usize,
    pub iterations: u64,
    pub files: usize,
    pub file_mib: u64,
    /// BLAKE3-derived checksum over the deterministic memory pattern.
    pub memory_digest: String,
    /// Sum of per-thread CPU counters (deterministic function of iterations).
    pub cpu_checksum: u64,
    /// Monotonic counter the workload increments while running.
    pub counter: u64,
}

/// Fill a buffer with a deterministic pattern and return its BLAKE3 digest.
///
/// The pattern is a simple position-dependent byte so that tampering or
/// corruption changes the digest.
pub fn pattern_digest(len: u64, seed: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    let buf = [0u8; 64 * 1024];
    let mut remaining = len;
    let mut offset: u64 = seed;
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        let mut chunk = buf[..take].to_vec();
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = ((offset.wrapping_add(i as u64)).wrapping_mul(2654435761) >> 13) as u8;
        }
        hasher.update(&chunk);
        offset = offset.wrapping_add(take as u64);
        remaining -= take as u64;
    }
    hasher.finalize().to_hex().to_string()
}

/// Deterministic CPU work; returns a checksum.
pub fn cpu_work(iterations: u64) -> u64 {
    let mut acc: u64 = 0;
    for i in 0..iterations {
        acc = acc.wrapping_add(i.wrapping_mul(2654435761));
        acc ^= acc >> 7;
    }
    acc
}

/// Compute the expected state for the given parameters. The restored workload
/// must reproduce this exactly (modulo the live `counter`, which starts at the
/// same value for equivalent captures).
pub fn expected_state(params: &WorkloadParams) -> WorkloadState {
    let bytes = params.memory_mib * 1024 * 1024;
    let memory_digest = pattern_digest(bytes, 0);

    let mut cpu_checksum: u64 = 0;
    for t in 0..params.threads.max(1) {
        cpu_checksum =
            cpu_checksum.wrapping_add(cpu_work(params.iterations).wrapping_add(t as u64));
    }

    WorkloadState {
        kind: params.kind,
        memory_mib: params.memory_mib,
        threads: params.threads,
        iterations: params.iterations,
        files: params.files,
        file_mib: params.file_mib,
        memory_digest,
        cpu_checksum,
        counter: 0,
    }
}

/// Write a workload state file atomically.
pub fn write_state(path: &Path, state: &WorkloadState) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(state).expect("state serializes");
    let tmp = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read a workload state file.
pub fn read_state(path: &Path) -> std::io::Result<WorkloadState> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Compare two states and return a human-readable list of differences.
pub fn diff_states(expected: &WorkloadState, actual: &WorkloadState) -> Vec<String> {
    let mut diffs = Vec::new();
    if expected != actual {
        if expected.memory_digest != actual.memory_digest {
            diffs.push(format!(
                "memory_digest mismatch: expected {} got {}",
                expected.memory_digest, actual.memory_digest
            ));
        }
        if expected.cpu_checksum != actual.cpu_checksum {
            diffs.push(format!(
                "cpu_checksum mismatch: expected {} got {}",
                expected.cpu_checksum, actual.cpu_checksum
            ));
        }
        if expected.counter != actual.counter {
            diffs.push(format!(
                "counter mismatch: expected {} got {}",
                expected.counter, actual.counter
            ));
        }
    }
    diffs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(kind: WorkloadKind) -> WorkloadParams {
        WorkloadParams {
            kind,
            memory_mib: 1,
            threads: 2,
            iterations: 10_000,
            files: 2,
            file_mib: 1,
        }
    }

    #[test]
    fn pattern_digest_is_deterministic() {
        assert_eq!(pattern_digest(1024, 0), pattern_digest(1024, 0));
    }

    #[test]
    fn pattern_digest_changes_with_length() {
        assert_ne!(pattern_digest(1024, 0), pattern_digest(2048, 0));
    }

    #[test]
    fn expected_state_is_stable() {
        let p = params(WorkloadKind::CpuMemory);
        assert_eq!(expected_state(&p), expected_state(&p));
    }

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let s = expected_state(&params(WorkloadKind::Multithreaded));
        write_state(&path, &s).unwrap();
        assert_eq!(read_state(&path).unwrap(), s);
    }

    #[test]
    fn diff_detects_change() {
        let a = expected_state(&params(WorkloadKind::CpuMemory));
        let mut b = a.clone();
        b.counter = 42;
        assert!(!diff_states(&a, &b).is_empty());
    }
}
