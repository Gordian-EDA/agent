#!/usr/bin/env bash
set -euo pipefail

expected_major="${1:-}"
if [[ ! "$expected_major" =~ ^(9|10)$ ]]; then
  echo "usage: $0 <9|10>" >&2
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

if [[ "$expected_major" == 9 ]]; then
  patch="${actual_version#9.0.}"
  patch="${patch%%[^0-9]*}"
  if [[ -z "$patch" || "$patch" -lt 3 ]]; then
    echo "KiCad $actual_version is unsafe for live footprint updates; require 9.0.3+" >&2
    exit 1
  fi
fi

# CI owns this disposable home directory, so configure the selected KiCad major
# explicitly before exercising managed launch. Production uses
# kicad.enableApiConfig for the same opt-in.
kicad_config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/kicad/${expected_major}.0"
kicad_config_file="$kicad_config_dir/kicad_common.json"
mkdir -p "$kicad_config_dir"
if [[ -f "$kicad_config_file" ]]; then
  sed -i 's/"enable_server"[[:space:]]*:[[:space:]]*false/"enable_server": true/g' "$kicad_config_file"
else
  printf '{\n  "api": {\n    "enable_server": true\n  }\n}\n' > "$kicad_config_file"
fi

echo "Running live KiCad IPC tests against KiCad $actual_version"
cargo test -p gordian-tools-pcb --lib -- --ignored --test-threads=1
cargo test -p gordian-core --test tools -- --ignored --test-threads=1
