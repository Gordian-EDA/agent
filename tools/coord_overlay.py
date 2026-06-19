#!/usr/bin/env python3
"""Stamp a labelled coordinate grid over a schematic render.

The VLM placement pass (sub-agent) needs a way to REFERENCE and SPECIFY positions on a
rendered schematic — to say "move U2 to cell (3,5)". This overlays a light grid with
integer (col,row) labels so the model can read where each part sits and name a target
cell. Columns increase left→right (x), rows top→bottom (y); labels sit at each line.

    python3 tools/coord_overlay.py IN.png OUT.png [COLS] [ROWS]

The grid is purely visual (drawn ON the raster); the (col,row) cell → fraction-of-canvas
mapping is deterministic so the caller can convert a model's target cell back to a
normalised position: x_frac = (col+0.5)/COLS, y_frac = (row+0.5)/ROWS.
"""
import sys
from PIL import Image, ImageDraw, ImageFont


def overlay(in_path, out_path, cols=16, rows=12):
    img = Image.open(in_path).convert("RGB")
    w, h = img.size
    d = ImageDraw.Draw(img)
    try:
        font = ImageFont.truetype("DejaVuSans-Bold.ttf", max(12, w // 90))
    except Exception:
        font = ImageFont.load_default()
    grid = (180, 180, 255)   # light blue-grey lines
    lab = (200, 0, 0)        # red labels — distinct from the schematic's greens/maroons
    cw, ch = w / cols, h / rows
    for c in range(cols + 1):
        x = c * cw
        d.line([(x, 0), (x, h)], fill=grid, width=1)
    for r in range(rows + 1):
        y = r * ch
        d.line([(0, y), (w, y)], fill=grid, width=1)
    # Cell labels (col,row) at each cell's top-left — small, so they don't hide parts.
    for c in range(cols):
        for r in range(rows):
            d.text((c * cw + 2, r * ch + 1), f"{c},{r}", fill=lab, font=font)
    img.save(out_path)
    # Status to stderr so stdout stays clean for callers that import overlay() and emit
    # machine-readable output on stdout (e.g. vlm_place.py's floorplan JSON).
    print(f"overlaid {cols}x{rows} grid -> {out_path} ({w}x{h})", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit("usage: coord_overlay.py IN.png OUT.png [COLS] [ROWS]")
    c = int(sys.argv[3]) if len(sys.argv) > 3 else 16
    r = int(sys.argv[4]) if len(sys.argv) > 4 else 12
    overlay(sys.argv[1], sys.argv[2], c, r)
