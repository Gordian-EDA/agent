#!/usr/bin/env python3
"""
schematic_critic.py — an automated VLM critic for rendered KiCAD schematics.

Why this exists: Claude sub-agents reviewing schematic PNGs both (a) over-report
"wire through a component body" on clean references and (b) MISS real instances on
generated circuits. This is a dedicated, stateless evaluator with a tightly-scoped
system prompt that forces the model to localize each defect concretely, so its
output can gate engine iteration objectively instead of by eyeballing.

It talks to the OpenAI-compatible gateway configured in the environment
(OPENAI_API_KEY / OPENAI_BASE_URL) and asks a vision model for a STRICT-JSON ranked
defect list. Exits non-zero (>0) when any critical/major defect is found, so it can
be used as a pass/fail gate in scripts.

Usage:
  python3 tools/schematic_critic.py OURS.png [--reference REF.png]
                                    [--circuit "one-line description of the intended circuit"]
                                    [--model anthropic/claude-opus-4-8] [--json-only]
"""
import argparse
import base64
import json
import os
import sys
import urllib.request

SYSTEM_PROMPT = """\
You are a ruthless, senior schematic-layout reviewer auditing a single rendered
KiCAD schematic for VISUAL/LAYOUT defects. The netlist is already known to be
electrically correct — your ONLY job is to judge how the drawing READS. Award no
credit for effort. But every defect you report MUST be one you can point to
concretely; do NOT invent defects to seem thorough, and do NOT pad the list.

How to read the image precisely:
- Wires are thin GREEN line segments (always horizontal or vertical).
- Component BODIES are the dark-red shapes: a resistor is a tall/wide hollow
  rectangle (or zigzag); a capacitor is two short parallel plates; a diode/LED is a
  triangle+bar; an IC/connector is a filled (often yellow) rectangle; a transistor
  is a circle with internal lines; a power symbol is a small arrow/bar/pennant.
- Pins are short stubs on a body's edge where a green wire attaches. The DARK-RED
  lines/triangles drawn INSIDE an IC rectangle are the symbol's own artwork, NOT
  wires — never report those as wires.

Defect classes to hunt, hardest-to-see first:

1. wire-through-body (HIGH PRIORITY, but DEFINED NARROWLY — read carefully to avoid
   false positives): a defect ONLY when a green wire crosses a component body
   TRANSVERSELY (perpendicular to the part), or when a wire that does NOT terminate
   on either of the part's pins overlaps the body. Concretely, a defect looks like:
   a HORIZONTAL rail passing straight across a VERTICAL resistor's rectangle, or a
   vertical wire slicing across a horizontal part.
   CRUCIAL — the following are CORRECT and must NEVER be reported as wire-through-body:
   • A 2-pin part placed IN-LINE / IN SERIES on a straight wire: the wire enters one
     pin and leaves the opposite pin along the SAME straight line, with the body
     sitting between the two pins. A vertical resistor with a green wire above it
     (to its top pin) and below it (from its bottom pin) is a NORMAL series/divider
     resistor — NOT a defect. A horizontal cap with wire on its left and right pins
     is NORMAL. The body bridging its own two collinear pins is how every series
     part is drawn.
   • An OP-AMP / COMPARATOR / regulator drawn as a TRIANGLE: its V+ power pin exits
     the TOP and its V- power pin exits the BOTTOM; vertical wires from those pins
     going up (to a +V rail symbol) and down (to GND) are NORMAL power connections,
     NOT wires through the body. The +/- input markers and the ">" inside the
     triangle are symbol artwork, not wires. Only a wire crossing the triangle's
     interior that is NOT one of its own pin connections counts.
   • A wire touching a pin at the body edge.
   • The dark-red artwork drawn INSIDE an IC rectangle.
   Test before reporting: does the green segment cross the body WITHOUT ending at
   either of that part's two pins? If it ends at a pin, it is NOT this defect.

2. dangling-pin: a pin stub or wire end that stops in EMPTY SPACE with no junction
   dot, no wire, and no symbol — a terminal left truly hanging.
   CRUCIAL — these are CONNECTED and must NEVER be reported as dangling/floating:
   • A pin (or short stub) ending at a POWER/GROUND SYMBOL — the small arrow, bar,
     or inverted-triangle glyph labeled GND / VCC / +5V / +3V3 / VIN etc. That glyph
     IS the connection: the pin is tied to that global rail. These glyphs are SMALL
     and easy to miss — before calling a pin dangling, look hard at its end for a tiny
     triangle/bar/arrow; an LED cathode or cap pin ending in a small inverted-triangle
     is GROUNDED, not floating. A nearby "GND"/"VCC" text label confirms the symbol
     is there even if the glyph is faint.
   • Two parts that connect ONLY through a shared rail (each has its own GND or VCC
     symbol, with no direct green wire between them) — a global power net needs no
     drawn wire. A decoupling cap whose top goes to a +5V symbol and an IC whose
     power pin goes to its own +5V symbol ARE connected. Do not call either floating.

3. text-overlap: a refdes/value/label text colliding with a wire, a body, or
   another text so they overlap or read as one run (e.g. a net label "GND" abutting
   a value "10k" so it reads "GND10k").

4. orientation: a part in series with the signal path drawn vertical when it should
   be horizontal, or a rail/decoupling tap to a power/ground symbol drawn at an odd
   angle. Resistor dividers / pull-ups should be clean vertical taps; series
   elements horizontal.

5. off-spine-leg / dog-leg: a connection that makes an unnecessary jog (extra
   bends) instead of a straight run; a part offset from the wire it taps so its lead
   zig-zags to reach it.

6. wire-crossing / congestion: avoidable crossings of unrelated nets, or a dense
   knot of wires/junctions that a small rearrangement would untangle.

7. spacing: parts flung far apart with long wires and big empty gaps (sprawl), OR
   parts/text cramped so they nearly touch.

Output STRICT JSON ONLY (no prose, no markdown fences), exactly this shape:
{
  "defects": [
    {"severity": "critical|major|minor",
     "category": "wire-through-body|dangling-pin|text-overlap|orientation|off-spine-leg|wire-crossing|congestion|spacing|other",
     "location": "which refdes / region of the sheet",
     "description": "one concrete sentence a person could verify"}
  ],
  "score": 0-10,            // 10 = publishable textbook quality, 0 = unreadable
  "summary": "one sentence overall verdict"
}
Order defects worst-first. If the sheet is genuinely clean, return an empty defects
list with a high score. severity guide: critical = wrong-reading/unreadable
(wire-through-body, dangling pin, text merged); major = clearly worse than a human
would draw; minor = cosmetic.
"""


def b64_image(path):
    with open(path, "rb") as f:
        return base64.b64encode(f.read()).decode()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--reference", help="optional reference PNG of the same circuit")
    ap.add_argument("--circuit", help="one-line description of the intended circuit")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--json-only", action="store_true", help="print only the JSON")
    args = ap.parse_args()

    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set in environment")

    user_content = []
    ctx = "Audit this rendered schematic for layout defects."
    if args.circuit:
        ctx += f" Intended circuit: {args.circuit}."
    if args.reference:
        ctx += (" A REFERENCE render of the SAME circuit (hand-drawn, good) is"
                " attached SECOND — compare, but only report defects in the FIRST"
                " (the one under review).")
    user_content.append({"type": "text", "text": ctx})
    user_content.append({"type": "image_url",
                         "image_url": {"url": f"data:image/png;base64,{b64_image(args.image)}"}})
    if args.reference:
        user_content.append({"type": "image_url",
                             "image_url": {"url": f"data:image/png;base64,{b64_image(args.reference)}"}})

    body = {
        "model": args.model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": 2000,
    }
    req = urllib.request.Request(
        f"{base}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=180) as resp:
        out = json.loads(resp.read())
    text = out["choices"][0]["message"]["content"].strip()
    # Strip accidental code fences.
    if text.startswith("```"):
        text = text.split("```", 2)[1].lstrip("json").strip().rstrip("`").strip()
    try:
        result = json.loads(text)
    except json.JSONDecodeError:
        print(text)
        sys.exit(2)

    if args.json_only:
        print(json.dumps(result, indent=2))
    else:
        print(f"=== critic: {os.path.basename(args.image)} ===")
        print(f"score: {result.get('score')}/10 — {result.get('summary','')}")
        for d in result.get("defects", []):
            print(f"  [{d.get('severity','?'):8}] {d.get('category','?'):16} "
                  f"{d.get('location','?')}: {d.get('description','')}")

    sev = {d.get("severity") for d in result.get("defects", [])}
    sys.exit(1 if ("critical" in sev or "major" in sev) else 0)


if __name__ == "__main__":
    main()
