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
| manifest | round-trip / corruption / traversal | 3 | pending |
| storage | chunk boundaries, hashing, dedup | 4 | pending |
| storage | concurrent writes, resource limits | 4 | pending |
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

# On a host WITHOUT CONFIG_CHECKPOINT_RESTORE these must fail with an explicit
# capability message (exit 1) and produce no artifacts:
cargo run -p snapforge-cli -- checkpoint --pid $$ --output /tmp/sf-ckpt
cargo run -p snapforge-cli -- restore --input /tmp/sf-ckpt
```

Total automated tests: **31** (13 core, 11 engine unit, 7 engine integration).
