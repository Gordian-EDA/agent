#!/usr/bin/env python3
"""Render a .kicad_pcb to a clean, professional 2D board PNG using KiCAD's own
plotter (kicad-cli) plus a cairosvg rasterizer.

This is the *artifact* render the PCB visual critic evaluates: copper traces and
pads are visible (unlike a 3D soldermask shot), silkscreen shows references and
component outlines, and the drawing sheet / title block is excluded so the frame
is just the board.

Usage:
    python3 tools/render_pcb.py BOARD.kicad_pcb [-o OUT.png] [--scale N]
                                [--layers L1,L2,...] [--side front|back]

Requires: kicad-cli (KiCAD >= 8) on PATH, and `cairosvg` (pip/uv install).
"""
import argparse
import os
import subprocess
import sys
import tempfile

# Front-side artifact layers: top copper, top silk (refs + outlines), and the
# board edge. Bottom copper is included so two-layer routing is fully visible;
# KiCAD renders it in a distinct colour beneath the top copper.
FRONT_LAYERS = "F.Cu,B.Cu,F.SilkS,Edge.Cuts"
BACK_LAYERS = "B.Cu,F.Cu,B.SilkS,Edge.Cuts"


def render(board: str, out: str, scale: float, layers: str, side: str) -> None:
    if side == "back":
        layers = layers or BACK_LAYERS
    else:
        layers = layers or FRONT_LAYERS

    with tempfile.TemporaryDirectory() as td:
        svg = os.path.join(td, "board.svg")
        cmd = [
            "kicad-cli", "pcb", "export", "svg",
            "--mode-single",
            "--exclude-drawing-sheet",
            "--page-size-mode", "2",   # board area only
            "--layers", layers,
            "--output", svg,
            board,
        ]
        if side == "back":
            cmd.insert(5, "--mirror")
        res = subprocess.run(cmd, capture_output=True, text=True)
        if not os.path.exists(svg):
            sys.stderr.write(res.stdout + res.stderr)
            raise SystemExit(f"kicad-cli failed to plot {board}")
        try:
            import cairosvg
        except ImportError:
            raise SystemExit("cairosvg not installed (uv pip install cairosvg)")
        cairosvg.svg2png(url=svg, write_to=out, scale=scale, background_color="white")
    print(out)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("board")
    ap.add_argument("-o", "--output", default=None)
    ap.add_argument("--scale", type=float, default=12.0)
    ap.add_argument("--layers", default="")
    ap.add_argument("--side", choices=["front", "back"], default="front")
    a = ap.parse_args()
    out = a.output or os.path.splitext(a.board)[0] + ".png"
    render(a.board, out, a.scale, a.layers, a.side)


if __name__ == "__main__":
    main()
