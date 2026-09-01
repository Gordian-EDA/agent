#!/usr/bin/env bash
# Build release and run the schematic quality suite end to end.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p gordian -p gordian-core
cargo build --release -p sch-doc --example sch_facts
export GORDIAN_BIN="${GORDIAN_BIN:-$PWD/target/release/gordian}"
exec python3 quality/run.py --suite schematic "$@"
