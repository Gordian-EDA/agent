#!/usr/bin/env python3
"""VLM structure pass (Phase 3, path E): read a rendered schematic and emit a STRUCTURE
constraint set — left->right signal-flow order + horizontal alignment rows — as JSON for
cola_place to consume via the COLA_VLM env var. This is the "VLM supplies structure the
rules/crossmin miss" lever from docs/specs/constraint-placement-and-vlm-structure.md, kept
as soft constraints (never raw geometry) so it can't break connectivity.

  set -a; . ./.env; set +a
  python3 tools/vlm_structure.py RENDER.png [--circuit "..."] > structure.json
"""
import argparse, base64, json, os, re, sys, urllib.request, urllib.error

PROMPT = """You are an expert schematic layout engineer. Look at this rendered schematic.
Its placement may be suboptimal (parts in awkward positions, weak left-to-right signal flow,
decoupling/passive banks not aligned). Propose a cleaner STRUCTURE using the EXACT reference
designators visible in the image:
- "flow": the major parts (ICs, connectors, transistors, regulators) in left-to-right
  signal-flow order (inputs/sources on the left, outputs/loads on the right).
- "rows": groups of parts that should sit together in ONE horizontal row (e.g. a bank of
  decoupling capacitors, a row of series resistors), each as a list of refdes.
Output ONLY JSON: {"flow": ["U1","Q1",...], "rows": [["C1","C2",...], ...]}. Omit parts you
are unsure about. Never invent a refdes that is not visible in the image."""


def extract_json(text):
    m = text
    if "```" in m:
        m = m.split("```")[1]
        m = re.sub(r"^json\s*", "", m).strip()
    try:
        return json.loads(m)
    except json.JSONDecodeError:
        depth = start = 0
        last = None
        for i, ch in enumerate(text):
            if ch == "{":
                if depth == 0:
                    start = i
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    last = text[start:i + 1]
        return json.loads(last)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--circuit")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    args = ap.parse_args()

    base = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    key = os.environ.get("OPENAI_API_KEY", "")
    if not base or not key:
        sys.exit("OPENAI_BASE_URL / OPENAI_API_KEY not set in environment")

    ctx = PROMPT + (f"\nIntended circuit: {args.circuit}." if args.circuit else "")
    with open(args.image, "rb") as f:
        img = base64.b64encode(f.read()).decode()
    body = {
        "model": args.model,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": ctx},
            {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{img}"}},
        ]}],
        "max_tokens": 2000,
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
        sys.exit(f"gateway error: {e.read().decode(errors='replace')[:400]}")
    text = out["choices"][0]["message"]["content"]
    obj = extract_json(text)
    # Keep only the two expected keys; coerce to lists of strings.
    clean = {
        "flow": [str(x) for x in obj.get("flow", []) if isinstance(x, (str, int))],
        "rows": [[str(x) for x in row] for row in obj.get("rows", []) if isinstance(row, list)],
    }
    print(json.dumps(clean))


if __name__ == "__main__":
    main()
