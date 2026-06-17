#!/usr/bin/env python3
"""
schematic_critic.py — an automated VLM critic for rendered KiCAD schematics.

Why this exists: VLMs reviewing schematic PNGs both (a) OVER-report "wire through a
component body" / "dangling pin" on clean drawings and (b) MISS real instances. This
is a dedicated, stateless evaluator that forces the model to REASON THROUGH each
false-positive-prone defect before committing, returns a richly-STRUCTURED verdict
(per-dimension scores + per-defect confidence + a verification trace), and gates a
loop on high-confidence majors only.

Model notes (gateway = OPENAI_BASE_URL, OpenAI-compatible):
- Default `anthropic/claude-opus-4-8` — the strongest vision model that actually works
  on the gateway. Native extended-thinking can't be enabled through this gateway (the
  `thinking`/`reasoning_effort` passthrough 400s), so "thinking" is obtained IN-PROMPT:
  the model writes a step-by-step analysis (tracing every FP-prone defect to its wire
  endpoints) BEFORE the JSON, which is what kills the wire-through-body / dangling-pin
  false positives.
- `--temperature` is NOT sent (deprecated on opus-4-8 / gpt-5.x → 400).
- Checked as of writing: Gemini 3 Pro is not hosted on this gateway, and `azure/gpt-5.4`
  returns "no model was able to generate a response" — so Opus is the working choice.
  `--model X` still lets you point at any future model.

Usage:
  python3 tools/schematic_critic.py OURS.png [--reference REF.png]
                                    [--circuit "one-line description"]
                                    [--model anthropic/claude-opus-4-8]
                                    [--engine-clean] [--json-only] [--show-reasoning]
"""
import argparse
import base64
import json
import os
import re
import sys
import urllib.request

SYSTEM_PROMPT = """\
You are the most exacting schematic-layout reviewer alive, auditing ONE rendered
KiCAD schematic for VISUAL/LAYOUT quality — how the drawing READS, not whether it is
electrically correct (the netlist is already verified correct). Award no credit for
effort. Two failure modes are equally bad: inventing a defect that isn't there
(FALSE POSITIVE), and missing a real one. You avoid both by TRACING the evidence for
every claim before you make it.

== HOW TO READ THE IMAGE PRECISELY ==
- Wires are thin GREEN axis-aligned segments (horizontal or vertical only).
- Component BODIES are dark-red shapes: resistor = hollow rectangle (or zigzag);
  capacitor = two short parallel plates; diode/LED = triangle + bar; IC/connector =
  filled (usually yellow) rectangle; transistor = circle with internal lines; power
  symbol = a small arrow / bar / inverted-triangle / pennant, usually with a tiny
  "GND"/"VCC"/"+3V3" text beside it.
- PINS are short stubs on a body edge where a green wire attaches.
- The dark-red lines/arcs/triangles drawn INSIDE an IC or op-amp body are the
  symbol's OWN ARTWORK, never wires. Never report symbol artwork as a wire.

== THE TWO FALSE-POSITIVE-PRONE CLASSES — TRACE BEFORE YOU REPORT ==

(A) wire-through-body. A real defect is a green wire that crosses a part's body
    WITHOUT terminating on either of that part's two pins — e.g. a horizontal rail
    sliced straight across a vertical resistor it does not connect to, or a wire
    running parallel to a cap, offset into its plates, passing by rather than landing
    on a pin. To decide, TRACE the offending segment to BOTH its endpoints:
      • If it enters one pin of the part and leaves the OPPOSITE pin along the same
        line (the body sits between its own two collinear pins) → NORMAL in-line /
        series / divider part. NOT a defect. (A vertical resistor or cap with a wire
        above it and a wire below it is the textbook way to draw a series element.)
      • If both ends terminate on pins / junctions / symbols and it merely passes
        NEAR a body → not through it. NOT a defect.
      • Op-amp/regulator TRIANGLE: vertical wires from its top (V+) and bottom (V-/GND)
        pins going up to a rail and down to ground are NORMAL power pins, not crossings.
    Only report it if you can name the segment AND state which part's body it crosses
    AND confirm it lands on NEITHER of that part's pins.

(B) dangling-pin. A real defect is a pin/wire-end stopping in EMPTY space with no
    junction dot, no wire, and no symbol. Before reporting, LOOK HARD at the endpoint:
      • A pin ending in a small arrow / bar / inverted-triangle (often faint), or with
        a nearby "GND"/"VCC"/"+3V3"/"VIN" label, is tied to that global rail — CONNECTED.
      • Two parts sharing only a global rail (each with its own GND/VCC symbol, no wire
        between them) ARE connected; a global net needs no drawn wire.
    Only report it if the endpoint is genuinely bare.

== THE OTHER DEFECT CLASSES (report freely, these are not FP-prone) ==
- text-overlap: refdes/value/label text colliding with a wire, body, or other text
  (e.g. "GND" abutting "10k" so it reads "GND10k"; a duplicated net label).
- orientation: a series element drawn vertical (should be horizontal) or a
  rail/decoupling tap at an odd angle; inconsistent orientation within one group.
- off-spine-leg / dog-leg: an avoidable jog (extra bends) where a straight run fits;
  a part offset from the wire it taps so its lead zig-zags.
- wire-crossing / congestion: avoidable crossings of unrelated nets, or a knot of
  wires/junctions a small rearrangement would untangle.
- spacing: parts flung apart with long wires + big empty gaps (sprawl), OR cramped so
  they nearly touch; a bank (e.g. decoupling caps) scattered instead of aligned.

== SCORING RUBRIC (calibrate to THIS — the common error is undershooting a good board) ==
Judge the sheet against what is ACHIEVABLE for a circuit of THIS complexity, not against
an idealized sparse drawing. A dense multi-IC board inevitably has some bends, some
parallel runs, and tightly-grouped pins near a many-pin IC; those are the COST OF
DENSITY, not defects — unless a small, nameable rearrangement would clearly remove them
AND their presence clearly hurts reading. A layout as clean as a careful human engineer's
hand drawing is a 9, NOT a 7.

Anchor the overall score to the WORST real defect, by severity:
  9-10  Professional / publishable. Reads at a glance, conventions held, compact. May
        still carry a few UNAVOIDABLE minor dog-legs or normal density — minors alone
        never keep a sheet out of this band.
  7-8   Good. Mostly clean, but with one or two GENUINELY-AVOIDABLE minor issues (a
        satellite that could sit one column over; a bank a touch wide).
  5-6   Mediocre. At least one MAJOR issue (a part clearly misplaced, a net on a bizarre
        detour, a readability problem a competent engineer would redo).
  3-4   Poor. Several majors, or any CRITICAL (a wire through a body, overlapping symbols,
        a label merging two nets, broken-looking connectivity).
  0-2   Unreadable / spaghetti.

Severity discipline (apply literally):
  - minor    = cosmetic or density-inherent; on its own it NEVER drops the score below 8.
  - major    = a competent engineer would redo it; drops to 5-7.
  - critical = wrong-reading / electrically-misleading; drops to <=4.
Count ONLY avoidable problems against the score. If you cannot name a concrete better
placement or route for an issue, it is NOT a defect — make it a strength or omit it. Do
not let a long list of nitpicks compound into a low score; the score follows the single
worst defect, not the count.

== PROCEDURE (follow in order) ==
1) In a "reasoning" section, walk the sheet methodically: list the components you see,
   then for EACH candidate (A) or (B) defect, TRACE the segment/endpoint and state your
   verdict (real / false-positive) with the reason. Be skeptical of your own first
   impression on (A) and (B).
2) Then output the final verdict as STRICT JSON, on its own, after the exact marker
   line `FINAL_JSON:`. No markdown fences. Shape:

FINAL_JSON:
{
  "dimension_scores": {
    "readability": 0-10,        // can a person trace every net at a glance
    "routing_neatness": 0-10,   // straight runs, few bends/crossings/junctions
    "compactness": 0-10,        // tight but not cramped; no sprawl, no empty gaps
    "convention": 0-10          // series horizontal, taps vertical, banks aligned
  },
  "score": 0-10,                // overall; 10 = publishable textbook quality
  "summary": "one-sentence verdict",
  "strengths": ["what reads well, concrete"],
  "defects": [
    {
      "severity": "critical|major|minor",   // critical=wrong-reading; major=clearly worse than a human; minor=cosmetic
      "category": "wire-through-body|dangling-pin|text-overlap|orientation|off-spine-leg|wire-crossing|congestion|spacing|other",
      "location": "refdes(es) / region",
      "description": "one concrete, verifiable sentence",
      "confidence": "high|medium|low",       // how sure it is real (not a FP); be honest
      "verification": "the specific observation that rules out a false positive (for A/B classes, the traced endpoints)"
    }
  ]
}
Order defects worst-first. A genuinely clean sheet gets an empty defects list and a
high score. Do NOT pad. Only `high`-confidence majors/criticals should ever gate a build.
"""


def b64_image(path):
    with open(path, "rb") as f:
        return base64.b64encode(f.read()).decode()


def extract_json(text):
    """Pull the JSON verdict out of a reasoning+JSON response: prefer the block after
    the FINAL_JSON: marker, else the last balanced {...} object."""
    if "FINAL_JSON:" in text:
        text = text.split("FINAL_JSON:", 1)[1]
    text = text.strip()
    if text.startswith("```"):
        text = text.split("```", 2)[1]
        text = re.sub(r"^json\s*", "", text).strip().rstrip("`").strip()
    # Try direct parse, else scan for the last balanced object.
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
    ap.add_argument("--reference", help="optional reference PNG of the same circuit")
    ap.add_argument("--circuit", help="one-line description of the intended circuit")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--json-only", action="store_true", help="print only the JSON verdict")
    ap.add_argument("--show-reasoning", action="store_true", help="also print the model's reasoning trace")
    ap.add_argument("--engine-clean", action="store_true",
                    help="engine geometry analysis confirms 0 wires through any body AND a "
                         "complete netlist; suppress wire-through-body + dangling-pin (FPs)")
    args = ap.parse_args()

    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set in environment")

    ctx = ("Audit this rendered schematic for layout quality. Reason first (trace every"
           " wire-through-body and dangling-pin candidate to its endpoints), then emit"
           " the FINAL_JSON verdict.")
    if args.circuit:
        ctx += f" Intended circuit: {args.circuit}."
    if args.reference:
        ctx += (" A REFERENCE render of the SAME circuit (hand-drawn, good) is attached"
                " SECOND — compare, but only report defects in the FIRST (under review).")
    if args.engine_clean:
        ctx += (" AUTHORITATIVE ENGINE GROUND TRUTH (exact geometric + netlist analysis of the"
                " real coordinates): (1) ZERO wires pass through any component body — the"
                " engine checks the perpendicular, collinear, AND parallel-offset-through-plate"
                " cases, so a wire that merely looks close is NOT crossing; (2) the netlist is"
                " COMPLETE — every pin is connected (to a wire, a power-symbol glyph at the pin,"
                " or a labelled global net). Therefore report NO wire-through-body and NO"
                " dangling-pin defect; any such claim is a confirmed false positive. Judge only"
                " orientation, dog-legs, crossings, congestion, spacing, and text overlap.")

    user_content = [{"type": "text", "text": ctx},
                    {"type": "image_url",
                     "image_url": {"url": f"data:image/png;base64,{b64_image(args.image)}"}}]
    if args.reference:
        user_content.append({"type": "image_url",
                             "image_url": {"url": f"data:image/png;base64,{b64_image(args.reference)}"}})

    body = {
        "model": args.model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_content},
        ],
        # Headroom for the in-prompt reasoning trace + the JSON. (No `temperature`:
        # it is deprecated on opus-4-8 / gpt-5.x and 400s the request.)
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

    # The engine-clean contract is enforced in code too, in case the model slips.
    if args.engine_clean:
        result["defects"] = [d for d in result.get("defects", [])
                             if d.get("category") not in ("wire-through-body", "dangling-pin")]

    if args.show_reasoning and "FINAL_JSON:" in text:
        print("--- reasoning ---")
        print(text.split("FINAL_JSON:", 1)[0].strip())
        print("--- verdict ---")

    if args.json_only:
        print(json.dumps(result, indent=2))
    else:
        ds = result.get("dimension_scores", {}) or {}
        dims = " ".join(f"{k[:4]}={v}" for k, v in ds.items())
        print(f"=== critic: {os.path.basename(args.image)} ===")
        print(f"score: {result.get('score')}/10 — {result.get('summary','')}")
        if dims:
            print(f"  dims: {dims}")
        for s in result.get("strengths", []):
            print(f"  + {s}")
        for d in result.get("defects", []):
            print(f"  [{d.get('severity','?'):8} {d.get('confidence','?'):6}] "
                  f"{d.get('category','?'):16} {d.get('location','?')}: {d.get('description','')}")
            if args.show_reasoning and d.get("verification"):
                print(f"        ↳ {d['verification']}")

    # Gate on HIGH-confidence majors/criticals only — low/medium-confidence claims are
    # the FP-prone ones and must not fail a build.
    gating = [d for d in result.get("defects", [])
              if d.get("severity") in ("critical", "major") and d.get("confidence") == "high"]
    sys.exit(1 if gating else 0)


if __name__ == "__main__":
    main()
