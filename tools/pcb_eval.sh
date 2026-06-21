#!/usr/bin/env bash
# End-to-end PCB quality sweep: route + export every harness circuit, render each
# with KiCAD's plotter, and score it with the VLM critic. Prints a summary table.
#
#   set -a; . ./.env; set +a
#   . .venv-pcb/bin/activate
#   tools/pcb_eval.sh
#
# Boards that are KiCAD-DRC-clean are critiqued with --drc-clean (the critic then
# judges only layout quality); others are critiqued plain so routing gaps surface.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT=/tmp/pcb-harness
SCALE=16

# (Re)generate boards from the circuit specs.
cargo run --release -q -p agent --example board_harness >/dev/null 2>&1

declare -A DESC=(
  [rc-divider]="resistive voltage divider with a 2-pin power/ground header"
  [ldo-regulator]="SOT-23 LDO with input/output decoupling caps and in/out headers"
  [led-array]="four LEDs each with a series resistor off a shared VCC/GND header"
  [soic-decoupling]="SOIC-8 IC with two decoupling caps and a 1x04 IO header"
  [transistor-led-driver]="low-side BJT LED driver from a control input"
  [opamp-filter]="dual op-amp with feedback dividers, decoupling, and IO headers"
  [rc-lowpass-chain]="three-stage RC low-pass ladder between input and output headers"
)

for board in "$OUT"/*/board.kicad_pcb; do
  name="$(basename "$(dirname "$board")")"
  png="$OUT/$name/pro.png"
  python3 "$ROOT/tools/render_pcb.py" "$board" -o "$png" --scale "$SCALE" >/dev/null 2>&1 || continue
  # DRC clean = 0 error-severity violations AND 0 unconnected items (KiCAD reports
  # unconnected separately from violations, so both must be checked).
  kicad-cli pcb drc --format json --severity-error -o /tmp/_drc.json "$board" >/dev/null 2>&1
  drc=$(python3 -c "import json;d=json.load(open('/tmp/_drc.json'));print(len(d['violations'])+len(d.get('unconnected_items',[])))" 2>/dev/null || echo "?")
  flag=""; [ "$drc" = "0" ] && flag="--drc-clean"
  echo "########## $name (drc errors: $drc) ##########"
  python3 "$ROOT/tools/pcb_critic.py" "$png" --circuit "${DESC[$name]:-$name}" $flag 2>&1 | sed -n '2,4p'
  echo
done
