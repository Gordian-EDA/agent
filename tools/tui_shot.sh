#!/usr/bin/env bash
# Render the TUI screenshot states to PNGs for visual review.
#   1. cargo test writes SVGs to /tmp/tui_shots/ (one per App state)
#   2. rsvg-convert (or cairosvg) rasterises each to a PNG
# Usage: tools/tui_shot.sh
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p gordian --lib tui::screenshot::tui_screenshots -- --nocapture >/dev/null 2>&1 || {
  echo "test failed; rerun verbosely:"; cargo test -p gordian --lib tui::screenshot::tui_screenshots; exit 1; }
for svg in /tmp/tui_shots/*.svg; do
  png="${svg%.svg}.png"
  if command -v rsvg-convert >/dev/null; then
    rsvg-convert -z 1.5 "$svg" -o "$png"
  else
    python3 -c "import cairosvg,sys; cairosvg.svg2png(url=sys.argv[1], write_to=sys.argv[2], scale=1.5)" "$svg" "$png"
  fi
  echo "rendered $png"
done
