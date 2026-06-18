#!/usr/bin/env python3
"""
pcb_critic.py — an automated VLM critic for rendered KiCAD PCB layouts.

Companion to schematic_critic.py, for the *board* side. It evaluates a rendered
2-D PCB plot (KiCAD's own plotter output — see tools/render_pcb.py) for LAYOUT
QUALITY: placement sense, routing directness/neatness, board utilisation, and
silkscreen legibility — the things that make a board read as professional and
that an autorouter's DRC pass does NOT capture.

Division of labour (important): KiCAD's `pcb drc` is the authoritative oracle for
clearance, shorts, and connectivity. This critic must NOT relitigate those — it
cannot read net assignments from a flat copper plot, so a "two traces cross =
short" or "trace too close to pad" claim from vision is FALSE-POSITIVE-PRONE.
Pass --drc-clean (set it from the export_board DRC result) to hard-suppress those
classes and focus the model on what it CAN judge: is the placement sensible, are
the routes direct, is the board the right size, is the silkscreen readable.

Model notes (gateway = OPENAI_BASE_URL, OpenAI-compatible): default
`anthropic/claude-opus-4-8` (strongest vision model that works on the gateway).
Reasoning is obtained IN-PROMPT (write analysis before the JSON); native
thinking/temperature passthrough 400s on this gateway.

Usage:
  set -a; . ./.env; set +a
  python3 tools/pcb_critic.py BOARD.png [--circuit "one-line description"]
          [--drc-clean] [--layers-note "..."] [--model M]
          [--json-only] [--show-reasoning]
"""
import argparse
import base64
import json
import os
import re
import sys
import urllib.request

SYSTEM_PROMPT = """\
You are the most exacting PCB-layout reviewer alive, auditing ONE rendered KiCAD
board plot for PLACEMENT and ROUTING quality — how the board reads and whether a
seasoned layout engineer would sign off on it. Award no credit for effort. Two
failure modes are equally bad: inventing a defect that isn't there (FALSE
POSITIVE) and missing a real one. Avoid both by TRACING the evidence for every
claim before you make it.

== HOW TO READ THE IMAGE PRECISELY ==
This is a 2-D fabrication-style plot, not a 3-D photo. Typical KiCAD plot colours:
- TOP copper (F.Cu): red/crimson — traces and pads on the top layer.
- BOTTOM copper (B.Cu): blue or green — traces on the bottom layer; a
  through-hole pad shows its barrel in the bottom colour inside a top-colour ring.
- SILKSCREEN (F.SilkS): yellow / cream — the reference designators (R1, U1, C3,
  J1...) and the component outline boxes. This is NOT copper.
- BOARD EDGE (Edge.Cuts): a thin outline marking the board boundary.
- PADS are solid filled copper shapes (rectangles/rounded-rects for SMD, rings
  with a hole for through-hole). TRACES are the thinner constant-width runs
  between pads. A VIA is a small filled circle (often with a ring) where a route
  changes layer (top↔bottom).

== WHAT YOU CANNOT JUDGE FROM THIS PLOT — DO NOT REPORT THESE ==
You cannot read which net each trace/pad belongs to from a flat copper plot, so
you CANNOT reliably judge electrical correctness. The board's DRC (clearance,
shorts, unconnected nets) is verified by a separate authoritative tool. Therefore:
- NEVER report "short circuit", "trace too close", "clearance violation",
  "unconnected net", or "two traces touch". Two copper runs of DIFFERENT colours
  (top vs bottom) crossing is a NORMAL layer change, not a crossing at all.
- A trace passing over/under a pad may simply connect to it or be on the other
  layer. Do not infer a short.
If DRC-clean is asserted below, treat all clearance/short/connectivity questions
as settled and judge ONLY layout quality.

== WHAT YOU SHOULD JUDGE (report freely; trace each claim) ==
- placement: Are related parts grouped and adjacent (a series resistor next to
  its LED; decoupling caps hugging their IC's power pins; a regulator between its
  input and output caps)? Are connectors/headers at the board EDGE, not stranded
  in the middle? Is the orientation consistent? A part that clearly belongs near
  another but sits across the board is a real placement defect — name both parts.
- board-utilisation / sizing: Does the board outline fit the parts, or is most of
  the board EMPTY with all parts crammed into one corner/region? Wasted board area
  (parts using a small fraction of the outline) is a real, common, professional
  defect — call it out and say which region is empty. Conversely, parts crammed so
  their courtyards collide is also a defect.
- routing-directness: Do traces take the direct route, or do they make avoidable
  detours / dog-legs / long loops far around the board when a short run exists?
  Trace a specific run from pad to pad and say where it wastes length.
- routing-neatness: Are runs straight with clean 45°/90° corners, or are there
  acute angles, wandering diagonals, or a knot of crossings a small rearrangement
  would remove? (Different-layer crossings don't count.)
- via-economy: Are there far more vias than the circuit needs (a via on a net that
  could stay on one layer)? Excess vias are a quality defect.
- silkscreen-legibility: Are reference designators present, non-overlapping, and
  clearly associated with their part? Refs colliding with each other, or a ref far
  from its part, are real defects. (Silk over a pad is a minor fab note, not a
  layout-quality blocker, unless it makes a ref unreadable.)

== SEVERITY DISCIPLINE (apply literally) ==
- minor    = cosmetic or density-inherent; on its own NEVER drops the score below 8.
- major    = a competent layout engineer would redo it (a clearly misplaced part, a
             badly wasted board, a net on a bizarre detour); drops to 5-7.
- critical = the board would not be accepted / reads as broken (parts overlapping,
             total spaghetti, connector buried, board 4x larger than needed); <=4.
Count ONLY avoidable problems. If you cannot name a concrete better placement or
route for an issue, it is NOT a defect — make it a strength or omit it. The overall
score follows the SINGLE WORST real defect, not the count of nitpicks. Judge against
what is ACHIEVABLE for a board of THIS part count, not an idealized empty board;
density-inherent bends/vias near a many-pin IC are the cost of density, not defects.

== SCORING BANDS ==
  9-10  Professional / manufacturable-looking. Sensible placement, direct tidy
        routing, board fits the parts, refs legible. Minor density artifacts OK.
  7-8   Good. One or two genuinely-avoidable minor issues.
  5-6   Mediocre. At least one MAJOR (a misplaced part, wasted board, ugly detour).
  3-4   Poor. Several majors or any CRITICAL.
  0-2   Unusable.

== PROCEDURE (in order) ==
1) In a "reasoning" section, list the parts you can identify (by silkscreen ref),
   describe the placement and the board outline vs parts, then walk the routing.
   For each candidate defect, state the concrete better alternative; if you can't,
   drop it.
2) Then output the verdict as STRICT JSON after the exact marker `FINAL_JSON:`
   (no markdown fences):

FINAL_JSON:
{
  "dimension_scores": {
    "placement": 0-10,        // related parts grouped, connectors at edge, sensible
    "routing": 0-10,          // direct, tidy, clean angles, few real crossings
    "board_use": 0-10,        // outline fits the parts; no big empty waste, no cram
    "silkscreen": 0-10        // refs present, legible, associated with their part
  },
  "score": 0-10,
  "summary": "one-sentence verdict",
  "strengths": ["what reads well, concrete"],
  "defects": [
    {
      "severity": "critical|major|minor",
      "category": "placement|board-utilisation|routing-directness|routing-neatness|via-economy|silkscreen|other",
      "location": "refdes(es) / region",
      "description": "one concrete, verifiable sentence with a better alternative",
      "confidence": "high|medium|low",
      "verification": "the specific observation that rules out a false positive"
    }
  ]
}
Order defects worst-first. A genuinely clean board gets an empty defects list and a
high score. Do NOT pad. Only `high`-confidence majors/criticals should ever gate a build.
"""

SUPPRESSED_WHEN_DRC_CLEAN = {
    "short", "shorts", "short-circuit", "clearance", "trace-clearance",
    "unconnected", "connectivity", "trace-too-close", "crossing", "wire-crossing",
}


def b64_image(path):
    with open(path, "rb") as f:
        return base64.b64encode(f.read()).decode()


def extract_json(text):
    if "FINAL_JSON:" in text:
        text = text.split("FINAL_JSON:", 1)[1]
    text = text.strip()
    if text.startswith("```"):
        text = text.split("```", 2)[1]
        text = re.sub(r"^json\s*", "", text).strip().rstrip("`").strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        pass
    depth, start, last = 0, None, None
    for i, ch in enumerate(text):
        if ch == "{":
            if depth == 0:
                start = i
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0 and start is not None:
                last = text[start:i + 1]
    if last:
        return json.loads(last)
    raise json.JSONDecodeError("no JSON object found", text, 0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--circuit", help="one-line description of the intended circuit")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--layers-note", help="extra note about the render's layer colours")
    ap.add_argument("--drc-clean", action="store_true",
                    help="KiCAD DRC confirms 0 clearance/short/unconnected violations; "
                         "suppress those FP-prone classes and judge layout quality only")
    ap.add_argument("--json-only", action="store_true")
    ap.add_argument("--show-reasoning", action="store_true")
    args = ap.parse_args()

    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set in environment")

    ctx = ("Audit this rendered PCB layout for placement and routing quality. Reason"
           " first (identify parts by silkscreen ref, assess placement and board-vs-parts"
           " sizing, then walk the routing), then emit the FINAL_JSON verdict.")
    if args.circuit:
        ctx += f" Intended circuit: {args.circuit}."
    if args.layers_note:
        ctx += f" Render note: {args.layers_note}."
    if args.drc_clean:
        ctx += (" AUTHORITATIVE DRC GROUND TRUTH: KiCAD's design-rule check on the real"
                " board geometry reports ZERO clearance violations, ZERO shorts, and ZERO"
                " unconnected items. Therefore every net is correctly and fully connected"
                " with legal spacing. Report NO short / clearance / unconnected / trace-too-close"
                " / crossing defect — any such claim is a confirmed false positive. Judge ONLY"
                " placement sense, board utilisation, routing directness/neatness, via economy,"
                " and silkscreen legibility.")

    user_content = [{"type": "text", "text": ctx},
                    {"type": "image_url",
                     "image_url": {"url": f"data:image/png;base64,{b64_image(args.image)}"}}]

    body = {
        "model": args.model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": 6000,
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
        msg = e.read().decode(errors="replace")
        try:
            msg = json.loads(msg).get("error", {}).get("message", msg)
        except Exception:
            pass
        sys.exit(f"critic: model `{args.model}` request failed (HTTP {e.code}): {msg[:300]}")
    text = out["choices"][0]["message"]["content"].strip()
    try:
        result = extract_json(text)
    except json.JSONDecodeError:
        print(text)
        sys.exit(2)

    # Enforce the drc-clean contract in code too, in case the model slips.
    if args.drc_clean:
        result["defects"] = [
            d for d in result.get("defects", [])
            if d.get("category", "").lower() not in SUPPRESSED_WHEN_DRC_CLEAN
            and not any(w in (d.get("description", "") + d.get("category", "")).lower()
                        for w in ("short circuit", "clearance viol", "unconnected"))
        ]

    if args.show_reasoning and "FINAL_JSON:" in text:
        print("--- reasoning ---")
        print(text.split("FINAL_JSON:", 1)[0].strip())
        print("--- verdict ---")

    if args.json_only:
        print(json.dumps(result, indent=2))
    else:
        ds = result.get("dimension_scores", {}) or {}
        dims = " ".join(f"{k[:5]}={v}" for k, v in ds.items())
        print(f"=== pcb critic: {os.path.basename(args.image)} ===")
        print(f"score: {result.get('score')}/10 — {result.get('summary','')}")
        if dims:
            print(f"  dims: {dims}")
        for s in result.get("strengths", []):
            print(f"  + {s}")
        for d in result.get("defects", []):
            print(f"  [{d.get('severity','?'):8} {d.get('confidence','?'):6}] "
                  f"{d.get('category','?'):18} {d.get('location','?')}: {d.get('description','')}")
            if args.show_reasoning and d.get("verification"):
                print(f"        ↳ {d['verification']}")

    gating = [d for d in result.get("defects", [])
              if d.get("severity") in ("critical", "major") and d.get("confidence") == "high"]
    sys.exit(1 if gating else 0)


if __name__ == "__main__":
    main()
