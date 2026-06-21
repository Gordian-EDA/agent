#!/usr/bin/env python3
"""Electrical-DESIGN critic — reviews a circuit NETLIST (not the rendered image) for
electrical-CORRECTNESS faults that pass ERC but are still wrong: pin-function mis-wires,
order-of-magnitude wrong values, missing essential parts, topology errors. The netlist
analog of tools/schematic_critic.py (which scores layout/readability).

Why it exists: the layout is solved and the model includes support circuitry, but it can
still make subtle FUNCTIONAL errors (e.g. ATtiny ISP MISO wired to PB3 instead of PB1) that
ERC can't catch (the net IS connected, just to the wrong pin) and the layout critic can't see.

  set -a; . ./.env; set +a
  python3 tools/design_critic.py DESIGN.yaml --intent "one-line circuit description" [--show-reasoning] [--json-only]
"""
import argparse, json, os, re, sys, urllib.request, urllib.error, statistics

SYSTEM_PROMPT = """You are a senior electronics design engineer performing a NETLIST review (NOT a layout review).

You are given a circuit's intended function and its netlist in circuit-YAML:
- each component has a refdes, a `part:` (KiCAD lib_id, e.g. `MCU_Microchip_ATtiny:ATtiny85-20P`), an optional `value:`, and a pin map (pin number/name -> net), OR a sugar form:
- `between: [A, B]` = a symmetric 2-pin part across nets A and B (R, C, etc.)
- `positive: A / negative: B` = a polarized part (LED/diode anode=A cathode=B; cap +=A -=B)
- `power: NET` component = a power-rail symbol marking NET as a rail
- `label:global` component = an exposed board-level port
- `decouple: {100nF: N}` = N synthesized bypass caps (so decoupling IS present if you see this)

Review ONLY for ELECTRICAL-DESIGN CORRECTNESS — faults a netlist can have while still passing ERC (connectivity) and looking clean:
1. PIN-FUNCTION mis-wires: a net wired to the WRONG pin for its function. Use your knowledge of the SPECIFIC part's pinout. Examples: SPI/ISP MISO/MOSI/SCK on the wrong MCU pin; a regulator FB pin not seeing the feedback divider; an enable/boot/reset pin tied to the wrong place; swapped differential pair.
2. WRONG VALUES: a resistor/cap value wrong by ~an order of magnitude for its role (1M I2C pull-up; 10nF "bulk" cap; a feedback divider whose ratio gives the wrong output voltage for the stated target).
3. MISSING ESSENTIAL parts (cannot function without — NOT nice-to-haves): crystal with no load caps; regulator with no output cap; MCU with NO decoupling at all; a floating high-impedance input that MUST be biased.
4. TOPOLOGY errors: feedback/bias/reference wired wrong; reversed polarity; a gain/divider network that does not match the stated intent.

Do NOT report: layout/placement, naming/style, missing nice-to-have protection (ESD/TVS/filters unless the intent demands them), test points, or anything you are not genuinely confident is a real electrical fault. A clean, correct design SHOULD score 9-10 with few or no defects — do NOT invent defects to seem thorough; false alarms are worse than silence here.

REASON STEP BY STEP FIRST: for each IC, state its key pins from your knowledge of that exact part, then trace the critical nets and check them. THEN emit, after a line `FINAL_JSON:`, a JSON object:
{"score": 0-10, "summary": "one line", "defects": [{"severity": "critical|major|minor", "confidence": "high|medium|low", "refdes": "U1", "issue": "short", "why": "the electrical reason"}]}
Only high-confidence critical/major defects are real gates."""


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
        raise


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("design", help="path to a circuit-YAML netlist (draft.yaml)")
    ap.add_argument("--intent", help="one-line description of the intended circuit", default="")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--samples", type=int, default=1, help="grade N times, report median score")
    ap.add_argument("--focus", default="", help="extra emphasis for this pass (a diverse-lens "
                    "ensemble member, e.g. power/feedback or digital-interface faults)")
    ap.add_argument("--show-reasoning", action="store_true")
    ap.add_argument("--json-only", action="store_true")
    args = ap.parse_args()

    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set in environment")

    netlist = open(args.design).read()
    user = (f"Intended circuit: {args.intent}\n\n" if args.intent else "") + f"Netlist:\n{netlist}"
    system = SYSTEM_PROMPT
    if args.focus:
        system += (f"\n\nEXTRA EMPHASIS THIS PASS — scrutinise especially: {args.focus}. "
                   "(Still report any other clear electrical fault you notice.)")
    body = {
        "model": args.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": 6000,
    }

    def grade_once():
        req = urllib.request.Request(
            f"{base}/chat/completions",
            data=json.dumps(body).encode(),
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=240) as resp:
                out = json.loads(resp.read())
        except urllib.error.HTTPError as e:
            print("gateway error:", e.read().decode(errors="replace")[:300], file=sys.stderr)
            return None
        text = out["choices"][0]["message"]["content"]
        try:
            return text, extract_json(text)
        except Exception as e:
            print("parse fail:", e, file=sys.stderr)
            return None

    runs = [r for _ in range(args.samples) if (r := grade_once())]
    if not runs:
        sys.exit("all grading attempts failed")
    runs.sort(key=lambda r: r[1].get("score", 0))
    text, verdict = runs[len(runs) // 2]  # median-score run

    if args.show_reasoning:
        print(text.split("FINAL_JSON:")[0].strip(), file=sys.stderr)
    if args.json_only:
        print(json.dumps(verdict))
        return
    print(f"score: {verdict.get('score')}/10 — {verdict.get('summary','')}")
    for d in verdict.get("defects", []):
        print(f"  [{d.get('severity','?'):8} {d.get('confidence','?'):6}] {d.get('refdes','')}: "
              f"{d.get('issue','')} — {d.get('why','')}")


if __name__ == "__main__":
    main()
