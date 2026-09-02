#!/usr/bin/env bash
set -euo pipefail

expected_major="${1:-}"
suite="${2:-all}"
if [[ ! "$expected_major" =~ ^(9|10)$ ]]; then
  echo "usage: $0 <9|10> [all|snapshot-only]" >&2
  exit 2
fi
if [[ "$suite" != "all" && "$suite" != "snapshot-only" ]]; then
  echo "suite must be all or snapshot-only" >&2
  exit 2
fi

for command_name in kicad-cli pcbnew Xvfb; do
  if ! command -v "$command_name" >/dev/null; then
    echo "missing live-test dependency: $command_name" >&2
    exit 1
  fi
done

actual_version="$(kicad-cli version)"
actual_major="${actual_version%%.*}"
if [[ "$actual_major" != "$expected_major" ]]; then
  echo "expected KiCad $expected_major, found $actual_version" >&2
  exit 1
fi

if [[ -e /tmp/kicad/api.sock ]] || pgrep -x pcbnew >/dev/null; then
  echo "live tests require exclusive pcbnew access and no /tmp/kicad/api.sock" >&2
  exit 1
fi

if [[ "$suite" == "all" && "$expected_major" == 9 ]]; then
  patch="${actual_version#9.0.}"
  patch="${patch%%[^0-9]*}"
  if [[ -z "$patch" || "$patch" -lt 3 ]]; then
    echo "KiCad $actual_version is unsafe for live footprint updates; require 9.0.3+" >&2
    exit 1
  fi
fi

# Use a disposable config so the live suite cannot change the user's KiCad
# preferences. Production uses kicad.enableApiConfig for the same opt-in.
live_config_root="$(mktemp -d /tmp/gordian-live-kicad.XXXXXX)"
trap 'rm -rf "$live_config_root"' EXIT
export XDG_CONFIG_HOME="$live_config_root"
kicad_config_dir="$live_config_root/kicad/${expected_major}.0"
kicad_config_file="$kicad_config_dir/kicad_common.json"
mkdir -p "$kicad_config_dir"
printf '{\n  "api": {\n    "enable_server": true\n  }\n}\n' > "$kicad_config_file"

echo "Comparing offline and live snapshots against KiCad $actual_version"
cargo test -p pcb-workflow offline_snapshots_match_ipc_for_corpus_and_quality_boards -- \
  --ignored --nocapture --test-threads=1

if [[ "$suite" == "all" ]]; then
  echo "Running live KiCad mutation tests against KiCad $actual_version"
  cargo test -p pcb-workflow seed_replacement_invalidates_same_runtime_live_session -- \
    --ignored --nocapture --test-threads=1
fi
