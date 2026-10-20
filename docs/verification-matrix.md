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
| transfer | copies all chunks + publishes | 5 | ran |
| transfer | idempotent re-run is a no-op | 5 | ran |
| transfer | skips chunks already at destination | 5 | ran |
| transfer | fails on missing source chunk | 5 | ran |
| transfer | cancellation is prompt | 5 | ran |
| transfer | concurrency bounded, correct | 5 | ran |
| transfer | refuses conflicting destination | 5 | ran |
| transfer | rejects zero concurrency | 5 | ran |
| transfer | CLI transfer + rerun + missing source | 5 | ran |
| incremental | reuse report counts chunks/bytes correctly | 6 | ran |
| incremental | chain resolves root-first | 6 | ran |
| incremental | chain detects cycle | 6 | ran |
| incremental | chain detects missing parent | 6 | ran |
| incremental | compaction analysis reports droppable | 6 | ran |
| incremental | child stores only new chunks (≤1 blob) | 6 | ran |
| incremental | identical snapshot reuses every chunk | 6 | ran |
| incremental | chain verifies all chunks in CAS | 6 | ran |
| incremental | CLI store --parent reuse + inspect/verify --chain | 6 | ran |
| storage | chunk boundaries, hashing, dedup | 4 | ran |
| storage | concurrent writes, resource limits | 4 | ran |
| transfer | retries, cancellation, atomic publish | 5 | pending |
| benchmark | `bench` is labeled `storage_only`, never a checkpoint | 7 | ran |
| benchmark | `bench --bench-json` emits labeled JSON; human mode is not JSON | 7 | ran |
| benchmark | storage sweep completes; timings/RSS recorded | 7 | ran |
| benchmark | transfer cold + warm (idempotent) measured | 7 | ran |
| benchmark | incremental reuse measured (0.875 for 1/8 change) | 7 | ran |
| benchmark | CRIU experiment recorded as skipped with reason | 7 | ran (skipped on this host) |
| benchmark | all result records validate against schema | 7 | ran (94/94) |
| extract | extract round-trips artifacts byte-for-byte | 8 | ran |
| extract | creates missing and nested destination paths | 8 | ran |
| extract | reassembles deduplicated/repeated chunks | 8 | ran |
| extract | refuses non-empty destination without overwrite | 8 | ran |
| extract | detects missing chunk before writing anything | 8 | ran |
| extract | detects corrupt chunk (digest mismatch) | 8 | ran |
| extract | rejects zero concurrency | 8 | ran |
| extract | CLI extract round-trip + JSON output | 8 | ran |
| extract | CLI extract refuses non-empty / fails on missing chunk | 8 | ran |
| gc | dry run reports orphans without deleting | 9 | ran |
| gc | apply removes orphans and keeps live chunks | 9 | ran |
| gc | invalid manifest aborts before deleting anything | 9 | ran |
| gc | removes leftover temp files from interrupted writes | 9 | ran |
| gc | shared chunks survive when one referrer is deleted | 9 | ran |
| gc | rejects zero concurrency (unit) | 9 | ran |
| gc | live set unions multiple manifests (unit) | 9 | ran |
| compact | flattens chain and reclaims ancestor-only chunks | 9 | ran |
| compact | dry run reports but changes nothing | 9 | ran |
| compact | refuses when an ancestor has another child | 9 | ran |
| compact | compacting a root is a no-op | 9 | ran |
| compact | missing snapshot errors cleanly | 9 | ran |
| compact | CLI compact dry run preserves the chain | 9 | ran |
| gc | CLI gc dry run then apply | 9 | ran |
| compact | CLI compact apply keeps the tip restorable | 9 | ran |
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

# Storage-only end-to-end benchmark (explicitly NOT a process checkpoint).
snapforge --store /tmp/sf-bench --chunk-size 262144 --concurrency 4 \
    --json bench --size-mib 256

# Full reproducible benchmark suite + report artifacts.
CARGO_TARGET_DIR=/tmp/snapforge-target cargo build --release -p snapforge-cli
python3 benchmarks/run.py --quick --out /tmp/sf-bench-out   # smoke test
python3 benchmarks/run.py --sizes 64,256 --repetitions 3 \
    --workdir /var/tmp/snapforge-bench --out benchmarks/results

# Transfer a snapshot between local stores (second run is an idempotent no-op).
cargo run -p snapforge-cli -- --store /tmp/sf-store4 transfer ckpt-1 /tmp/sf-store4b
cargo run -p snapforge-cli -- --store /tmp/sf-store4b verify ckpt-1 --require-chunks

# Extract (restore files from) a snapshot into a directory, then compare.
cargo run -p snapforge-cli -- --store /tmp/sf-store4 extract ckpt-1 --output /tmp/sf-restored
diff -r /path/to/artifacts /tmp/sf-restored
# A non-empty destination is refused unless --force is given:
cargo run -p snapforge-cli -- --store /tmp/sf-store4 extract ckpt-1 --output /tmp/sf-restored --force

# Incremental: capture a child linked to a parent, then inspect/verify the chain.
cargo run -p snapforge-cli -- --store /tmp/sf-store6 --chunk-size 65536 \
    store /path/to/baseline --id base
cargo run -p snapforge-cli -- --store /tmp/sf-store6 --chunk-size 65536 \
    store /path/to/changed --id child --parent base
cargo run -p snapforge-cli -- --store /tmp/sf-store6 inspect child --chain
cargo run -p snapforge-cli -- --store /tmp/sf-store6 verify child --chain --require-chunks

# Storage maintenance: both report by default; --apply mutates the store.
cargo run -p snapforge-cli -- --store /tmp/sf-store6 gc
cargo run -p snapforge-cli -- --store /tmp/sf-store6 gc --apply
cargo run -p snapforge-cli -- --store /tmp/sf-store6 compact child
cargo run -p snapforge-cli -- --store /tmp/sf-store6 compact child --apply
cargo run -p snapforge-cli -- --store /tmp/sf-store6 verify child --require-chunks

# On a host WITHOUT CONFIG_CHECKPOINT_RESTORE these must fail with an explicit
# capability message (exit 1) and produce no artifacts:
cargo run -p snapforge-cli -- checkpoint --pid $$ --output /tmp/sf-ckpt
cargo run -p snapforge-cli -- restore --input /tmp/sf-ckpt
```

Total automated tests: **141** (13 core, 18 engine, 24 format, 64 storage, 22 CLI integration; property tests are included in the format and storage totals). Benchmark result records are validated separately against `benchmarks/schemas/result.schema.json`.
