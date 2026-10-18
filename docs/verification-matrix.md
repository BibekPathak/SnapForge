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
| engine | criu version parsing (3 forms) | 2 | ran |
| engine | supported version threshold | 2 | ran |
| engine | criu not-found is a clean error | 2 | ran |
| engine | timeout kills process group | 2 | ran |
| engine | `/proc` process status parsing | 2 | ran |
| engine | process existence checks | 2 | ran |
| engine | capability gate blocks when host unsupported | 2 | ran |
| engine | empty checkpoint output is failure | 2 | ran |
| engine | never success on CRIU failure | 2 | ran |
| engine | argument construction (argv array) | 2 | ran |
| engine | timeout marks command timed_out | 2 | ran |
| engine | checkpoint/restore option defaults | 2 | ran |
| manifest | round-trip / corruption / traversal | 3 | ran |
| format | valid manifest passes strict validation | 3 | ran |
| format | manifest JSON round-trip | 3 | ran |
| format | unsupported version rejected | 3 | ran |
| format | missing/tampered integrity rejected | 3 | ran |
| format | path traversal / absolute path rejected | 3 | ran |
| format | duplicate artifact path rejected | 3 | ran |
| format | incomplete chunk map rejected | 3 | ran |
| format | chunk out-of-bounds / zero-length rejected | 3 | ran |
| format | non-hex chunk id rejected | 3 | ran |
| format | missing chunk in store rejected | 3 | ran |
| format | self-parent and cyclic parents rejected | 3 | ran |
| format | snapshot-id / artifact-path rules | 3 | ran |
| format | atomic publish + refuse-overwrite | 3 | ran |
| format | publish rejects invalid staged manifest | 3 | ran |
| format | property tests (coverage, digest stability) | 3 | ran |
| format | CLI inspect/verify end-to-end (binary) | 3 | ran |
| format | CLI verify fails on tamper / missing chunk | 3 | ran |
| storage | chunker: even/ragged/empty/reassembly | 4 | ran |
| storage | chunk ref matches its bytes | 4 | ran |
| storage | CAS put/get round-trip | 4 | ran |
| storage | CAS idempotent dedup | 4 | ran |
| storage | CAS missing chunk reported cleanly | 4 | ran |
| storage | CAS corruption detected on read | 4 | ran |
| storage | pipeline reassembles exactly | 4 | ran |
| storage | pipeline deduplicates identical chunks | 4 | ran |
| storage | dedup across sources (0 new writes) | 4 | ran |
| storage | concurrency bounded (peak in-flight) | 4 | ran |
| storage | rejects zero concurrency | 4 | ran |
| storage | varied chunk sizes reassemble | 4 | ran |
| storage | property tests (round-trip, accounting) | 4 | ran |
| storage | CLI store/dedup/verify end-to-end | 4 | ran |
| storage | chunk boundaries, hashing, dedup | 4 | ran |
| storage | concurrent writes, resource limits | 4 | ran |
| transfer | retries, cancellation, atomic publish | 5 | pending |
| engine | **real** CRIU capture/restore | 2 | **skipped: kernel lacks `CONFIG_CHECKPOINT_RESTORE`** |

## How to reproduce checks

```bash
make check                 # fmt + clippy -D warnings + unit tests
cargo run -p snapforge-cli -- doctor
cargo run -p snapforge-cli -- status --pid $$
cargo run -p snapforge-cli -- workload --kind cpu-memory \
    --memory-mib 16 --threads 2 --state-file /tmp/sf.json
cargo run -p snapforge-cli -- workload --kind cpu-memory \
    --memory-mib 16 --threads 2 --validate --state-file /tmp/sf.json

# Format layer: build a fixture, then inspect and verify it.
cargo run -p snapforge-format --example make_fixture -- /tmp/sf-store
cargo run -p snapforge-cli -- --store /tmp/sf-store inspect fixture-1
cargo run -p snapforge-cli -- --store /tmp/sf-store verify fixture-1 --require-chunks

# Storage: ingest a directory of artifacts, then verify (with CAS coverage).
cargo run -p snapforge-cli -- --store /tmp/sf-store4 --chunk-size 262144 \
    store /path/to/artifacts --id ckpt-1 --workload cpu-memory
cargo run -p snapforge-cli -- --store /tmp/sf-store4 verify ckpt-1 --require-chunks

# Microbenchmarks (storage layer).
cargo bench -p snapforge-storage

# On a host WITHOUT CONFIG_CHECKPOINT_RESTORE these must fail with an explicit
# capability message (exit 1) and produce no artifacts:
cargo run -p snapforge-cli -- checkpoint --pid $$ --output /tmp/sf-ckpt
cargo run -p snapforge-cli -- restore --input /tmp/sf-ckpt
```

Total automated tests: **85** (13 core, 18 engine, 24 format, 22 storage, 8 CLI integration).
