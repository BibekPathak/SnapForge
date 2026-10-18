#!/usr/bin/env bash
# Print the SnapForge host capability report.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/snapforge-target}"
exec cargo run --quiet -p snapforge-cli -- doctor "$@"
