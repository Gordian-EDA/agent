#!/usr/bin/env python3
"""Render a .kicad_pcb to a clean, professional 2D board PNG using KiCAD's own
plotter (`kicad-cli`) plus a cairosvg rasterizer.

This is the *artifact* render the PCB visual critic evaluates: copper traces and
pads are visible (unlike a 3D soldermask shot), silkscreen shows references and
component outlines, and the drawing sheet / title block is excluded so the frame
is just the board.

Usage:
    python3 tools/render_pcb.py BOARD.kicad_pcb [-o OUT.png] [--scale N]
                                [--layers L1,L2,...] [--side front|back]

Requires: KiCad 10 configured through `KICAD_CLI` or `kicad.cliPath`, and
`cairosvg` (pip/uv install).
"""
import argparse
import os
import subprocess
import sys
import tempfile

from kicad_cli import configured_kicad_cli

# Front-side artifact layers: top copper, top silk (refs + outlines), and the
# board edge. Bottom copper is included so two-layer routing is fully visible;
# KiCAD renders it in a distinct colour beneath the top copper.
FRONT_LAYERS = "F.Cu,B.Cu,F.SilkS,Edge.Cuts"
BACK_LAYERS = "B.Cu,F.Cu,B.SilkS,Edge.Cuts"


def render_3d(board: str, out: str, side: str, width: int, height: int) -> None:
    """Photorealistic 3D 'beauty' render via KiCAD's raytracer — green soldermask,
    ENIG pads, white silk. No SVG/cairosvg step (`kicad-cli` writes the PNG)."""
    cmd = [
        configured_kicad_cli(), "pcb", "render",
        "--side", side,
        "--quality", "high",
        "--background", "opaque",
        "-w", str(width), "-h", str(height),
        "--output", out,
        board,
    ]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if not os.path.exists(out):
        sys.stderr.write(res.stdout + res.stderr)
        raise SystemExit(f"kicad-cli failed to 3D-render {board}")
    print(out)


def render(board: str, out: str, scale: float, layers: str, side: str) -> None:
    if side == "back":
        layers = layers or BACK_LAYERS
    else:
        layers = layers or FRONT_LAYERS

    with tempfile.TemporaryDirectory() as td:
        svg = os.path.join(td, "board.svg")
        cmd = [
            configured_kicad_cli(), "pcb", "export", "svg",
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
    ap.add_argument("--3d", dest="threed", action="store_true",
                    help="photorealistic 3D raytrace render instead of the 2D plot")
    ap.add_argument("--width", type=int, default=1400)
    ap.add_argument("--height", type=int, default=1000)
    a = ap.parse_args()
    out = a.output or os.path.splitext(a.board)[0] + (".3d.png" if a.threed else ".png")
    if a.threed:
        render_3d(a.board, out, "top" if a.side == "front" else "bottom", a.width, a.height)
    else:
        render(a.board, out, a.scale, a.layers, a.side)


if __name__ == "__main__":
    main()
