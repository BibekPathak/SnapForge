# SnapForge Verification Matrix

Status of automated checks on the current development host
(Arch Linux, kernel `7.1.9-hardened1-1-hardened`, UID 1000, no
`CONFIG_CHECKPOINT_RESTORE`).

Legend: **ran** = executed here and passed; **skipped** = cannot run here;
**pending** = implemented in a later phase.

| Area | Test | Phase | Status |
|---|---|---|---|
| config | defaults valid | 1 | ran |
| config | reject zero concurrency | 1 | ran |
| config | reject non-power-of-two chunk size | 1 | ran |
| config | accept 64K/256K/1M/4M chunk sizes | 1 | ran |
| host | probe returns a report | 1 | ran |
| host | classify missing binary | 1 | ran |
| host | classify unsupported kernel | 1 | ran |
| host | doctor render contains status line | 1 | ran |
| workload | pattern digest deterministic | 1 | ran |
| workload | pattern digest length-sensitive | 1 | ran |
| workload | expected state stable | 1 | ran |
| workload | state file round-trip | 1 | ran |
| workload | diff detects change | 1 | ran |
| workload | execute + validate round trip | 1 | ran (manual) |
| workload | validate fails on wrong params | 1 | ran (manual) |
| manifest | round-trip / corruption / traversal | 3 | pending |
| storage | chunk boundaries, hashing, dedup | 4 | pending |
| storage | concurrent writes, resource limits | 4 | pending |
| transfer | retries, cancellation, atomic publish | 5 | pending |
| engine | real CRIU capture/restore | 2 | **skipped: kernel lacks `CONFIG_CHECKPOINT_RESTORE`** |
| engine | CRIU prerequisite diagnostics | 2 | pending |

## How to reproduce Phase 1 checks

```bash
make check                 # fmt + clippy -D warnings + unit tests
cargo run -p snapforge-cli -- doctor
cargo run -p snapforge-cli -- workload --kind cpu-memory \
    --memory-mib 16 --threads 2 --state-file /tmp/sf.json
cargo run -p snapforge-cli -- workload --kind cpu-memory \
    --memory-mib 16 --threads 2 --validate --state-file /tmp/sf.json
```
