#!/usr/bin/env bash
# Render the TUI screenshot states to PNGs for visual review.
#   1. cargo test writes SVGs to /tmp/tui_shots/ (one per App state)
#   2. cairosvg rasterises each to a PNG
# Usage: tools/tui_shot.sh   (needs .venv-pcb with cairosvg)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p autopcb --bin autopcb tui::screenshot::tui_screenshots -- --nocapture >/dev/null 2>&1 || {
  echo "test failed; rerun verbosely:"; cargo test -p autopcb --bin autopcb tui::screenshot::tui_screenshots; exit 1; }
source .venv-pcb/bin/activate 2>/dev/null || true
for svg in /tmp/tui_shots/*.svg; do
  png="${svg%.svg}.png"
  python3 -c "import cairosvg,sys; cairosvg.svg2png(url=sys.argv[1], write_to=sys.argv[2], scale=2.0)" "$svg" "$png"
  echo "rendered $png"
done
