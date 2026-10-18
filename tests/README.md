# Tests

Unit tests live next to the code they exercise (`#[cfg(test)]` modules).

This directory is reserved for:

- **Integration tests** that compose multiple crates (snapshot round-trips,
  storage pipelines, transfer).
- **Property-based tests** for chunk mapping and artifact integrity.
- **CRIU integration tests**, which are gated behind a runtime capability check
  so they are *skipped with a clear reason* on hosts without
  `CONFIG_CHECKPOINT_RESTORE`.

See `docs/verification-matrix.md` for which tests actually ran on the current
host.
