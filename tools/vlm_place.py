#!/usr/bin/env python3
"""VLM floorplanner: read a gridded schematic render, return a compact floorplan.

The engine's placement algorithm is sprawl-capped (it can't de-sprawl without colliding
satellite fans). A vision LLM CAN do the global, semantic spatial reasoning that de-sprawls
a board ("the LDO is marooned bottom-right; move it next to the MCU"). This is the VLM step
of the placement loop:

    render -> coord_overlay -> [THIS: vlm_place -> {refdes:[col,row]}] -> vlm_apply -> re-render

It overlays a coordinate grid on the render, sends it to the OpenAI-gateway vision model
(same transport as tools/schematic_critic.py), and returns the floorplan as JSON. Apply the
result with vlm_apply.py, re-render, and A/B it against the auto layout via schematic_critic
(VLM-placement HELPS sprawled boards but can HURT already-tidy ones, so keep the better).

    set -a; . ./.env; set +a
    python3 tools/vlm_place.py RENDER.png --grid 8x6 [--out FLOORPLAN.json] [--show]
"""
import argparse
import json
import os
import re
import sys
import urllib.request
import urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from coord_overlay import overlay  # noqa: E402


def b64_image(path):
    import base64
    with open(path, "rb") as f:
        return base64.b64encode(f.read()).decode()


SYSTEM = """You are an expert schematic FLOORPLANNER. You are shown a rendered schematic with a
labelled coordinate grid (each cell tagged "col,row" in red; col increases left→right, row top→bottom).
Produce a COMPACT, signal-flow floorplan for the MAJOR parts (ICs and connectors — refdes like U1, J2):
power-input parts LEFT, the main IC(s) CENTRE, peripherals/outputs RIGHT.

PACK TIGHTLY — this is the most important rule. Use ADJACENT cells in the SMALLEST possible region:
connected parts go in NEIGHBOURING cells (differ by 1 in col or row), and the whole floorplan must fit
in roughly a 3-4 cell wide by 2-3 cell tall block. Do NOT spread parts across the grid with gaps
between them. GOOD: cols 2,3,4,5 next to each other. BAD: cols 0,2,4,6 with empty cells between.
Leave small passives (R, C, crystals, LEDs) OUT — the engine clusters those next to the pin they wire to.

Reason briefly first (where parts currently sit and how spread out they are), then emit the floorplan as a
fenced JSON code block of {refdes: [col,row]} and nothing else in the block. Distinct, ADJACENT cells."""


def extract_json(text):
    # Prefer a fenced ```json block; else the last {...} object.
    m = re.findall(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.DOTALL)
    if m:
        return json.loads(m[-1])
    m = re.findall(r"(\{[^{}]*\})", text, re.DOTALL)
    if m:
        return json.loads(m[-1])
    raise json.JSONDecodeError("no JSON object found", text, 0)


def place(image, cols, rows, model):
    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set")
    grid_png = image + ".grid.png"
    overlay(image, grid_png, cols, rows)
    body = {
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": [
                {"type": "text", "text": f"Grid is {cols} columns x {rows} rows. Floorplan the major parts."},
                {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{b64_image(grid_png)}"}},
            ]},
        ],
        "max_tokens": 4000,
    }
    req = urllib.request.Request(
        f"{base}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=240) as resp:
            out = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        sys.exit(f"vlm_place: request failed (HTTP {e.code}): {e.read().decode(errors='replace')[:300]}")
    text = out["choices"][0]["message"]["content"].strip()
    return extract_json(text), text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--grid", default="8x6", help="COLSxROWS, e.g. 8x6")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--out", help="write the floorplan JSON here")
    ap.add_argument("--show", action="store_true", help="also print the model's reasoning")
    args = ap.parse_args()
    cols, rows = (int(x) for x in args.grid.lower().split("x"))
    fp, text = place(args.image, cols, rows, args.model)
    if args.show:
        print(text, file=sys.stderr)
    js = json.dumps(fp)
    if args.out:
        open(args.out, "w").write(js)
    print(js)


if __name__ == "__main__":
    main()
