#!/usr/bin/env python3
"""VLM coarse-ZONE planner for the HYBRID placement loop (LLM steers, engine places).

A vision LLM is good at rough DIRECTION ("power left, MCU centre, outputs right") but NOT at
millimetre positions — forcing its exact cells wrecks the engine's good local placement
(decoupling banks, spines). So this returns a COARSE zone per major part as a board-bbox
FRACTION {refdes:[fx,fy]} (fx 0=left..1=right, fy 0=top..1=bottom). The engine consumes it as
a SOFT bias (LayoutIr.zone / proxy_cost zbias), doing the precise placement itself.

    render -> coord_overlay (coarse grid) -> [THIS -> {refdes:[fx,fy]}] -> $ZONE_FILE -> re-render

    set -a; . ./.env; set +a
    python3 tools/vlm_place.py RENDER.png [--grid 4x3] [--out ZONE.json] [--context "..."] [--show]

--context feeds the previous critic's defects back in for the feedback loop (iterate: place ->
critic -> adjust zones -> repeat).
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


SYSTEM = """You are an expert schematic FLOORPLANNER giving ROUGH DIRECTION to a placement engine.
You see a rendered schematic with a COARSE zone grid overlaid (each cell tagged "col,row" in red;
col increases left→right, row top→bottom). The grid is intentionally coarse — you choose which broad
ZONE each major part belongs in, and the engine does the precise millimetre placement itself.

Assign each MAJOR part (ICs and connectors — refdes like U1, J2; ignore small R/C/crystal/LED passives)
to ONE coarse zone for clean left→right signal flow:
- power-INPUT connectors/regulators on the LEFT (low col),
- the main IC(s) in the CENTRE,
- peripheral/output connectors on the RIGHT (high col),
- put parts that talk to each other in the same or neighbouring rows.

You are giving rough direction, NOT exact positions — pick the zone that captures each part's ROLE in
the signal flow. It is fine for several parts to share a zone.

Reason briefly first (what's currently mis-placed / sprawled), then emit ONLY a fenced JSON code block
of {refdes: [col,row]} (the coarse cell per major part) and nothing else in the block."""


def extract_json(text):
    m = re.findall(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.DOTALL)
    if m:
        return json.loads(m[-1])
    m = re.findall(r"(\{[^{}]*\})", text, re.DOTALL)
    if m:
        return json.loads(m[-1])
    raise json.JSONDecodeError("no JSON object found", text, 0)


def cells_to_fractions(cells, cols, rows):
    """Coarse cell (col,row) -> board-bbox fraction [fx,fy], matching coord_overlay's mapping."""
    return {rd: [round((c + 0.5) / cols, 3), round((r + 0.5) / rows, 3)]
            for rd, (c, r) in cells.items()}


def plan(image, cols, rows, model, context, summary):
    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set")
    grid_png = image + ".grid.png"
    overlay(image, grid_png, cols, rows)
    user_text = f"Coarse grid is {cols} columns x {rows} rows. Give each major part its zone."
    if summary:
        # The MODULE GRAPH — trust this over the cluttered render for connectivity/roles.
        user_text = ("The circuit's module graph (use it for roles + what connects to what; the "
                     f"render is cluttered):\n{summary}\n\n" + user_text +
                     " Put connected parts in the same or neighbouring zones; keep same-role parts "
                     "(e.g. all inputs, all outputs) together.")
    if context:
        user_text += (f"\n\nThis is a REFINEMENT pass. The previous layout's critic feedback:\n{context}\n"
                      "Adjust the zones to address it (e.g. move a part nearer what it connects to).")
    body = {
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": [
                {"type": "text", "text": user_text},
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
    return cells_to_fractions(extract_json(text), cols, rows), text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--grid", default="4x3", help="coarse COLSxROWS, e.g. 4x3")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--out", help="write the zone JSON {refdes:[fx,fy]} here ($ZONE_FILE)")
    ap.add_argument("--context", help="previous critic feedback, for a refinement pass")
    ap.add_argument("--summary", help="circuit module-graph text (see circuit_summary.py)")
    ap.add_argument("--show", action="store_true")
    args = ap.parse_args()
    cols, rows = (int(x) for x in args.grid.lower().split("x"))
    zones, text = plan(args.image, cols, rows, args.model, args.context, args.summary)
    if args.show:
        print(text, file=sys.stderr)
    js = json.dumps(zones)
    if args.out:
        open(args.out, "w").write(js)
    print(js)


if __name__ == "__main__":
    main()
