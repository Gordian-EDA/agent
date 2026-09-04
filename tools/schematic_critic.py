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

Calibration: with `--anchor REF.png` the verdict is anchored to a human-drawn sheet
rated exactly 9 — equal to it is a 9, clearly better a 10 — which is what makes scores
comparable across circuits. `--samples 3` grades three times and reports the rounded mean.

Usage:
  python3 tools/schematic_critic.py OURS.png [--anchor REF.png [--anchor-same-circuit]]
                                    [--circuit "one-line description"]
                                    [--model anthropic/claude-opus-4-8] [--samples 3]
                                    [--engine-clean] [--json-only] [--show-reasoning]
"""
import argparse
import base64
import json
import os
import pathlib
import re
import sys
import urllib.request

# The rubric, the anchor calibration and the engine-clean ground truth are shared
# verbatim with the agent's `review_schematic` tool, which `include_str!`s these files.
ASSETS = pathlib.Path(__file__).resolve().parent


def read_asset(name):
    return (ASSETS / f"schematic_critic_{name}.txt").read_text(encoding="utf-8").strip()


SYSTEM_PROMPT = read_asset("system")
ANCHOR_CALIBRATION = read_asset("anchor")
ENGINE_CLEAN = read_asset("engine_clean")


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
    ap.add_argument("--anchor", help="human-drawn reference PNG rated 9; calibrates the score")
    ap.add_argument("--anchor-same-circuit", action="store_true",
                    help="the anchor draws the same circuit as the sheet under review")
    ap.add_argument("--circuit", help="one-line description of the intended circuit")
    ap.add_argument("--model", default=os.environ.get("CRITIC_MODEL", "anthropic/claude-opus-4-8"))
    ap.add_argument("--json-only", action="store_true", help="print only the JSON verdict")
    ap.add_argument("--show-reasoning", action="store_true", help="also print the model's reasoning trace")
    ap.add_argument("--engine-clean", action="store_true",
                    help="engine geometry analysis confirms 0 wires through any body AND a "
                         "complete netlist; suppress wire-through-body + dangling-pin (FPs)")
    ap.add_argument("--samples", type=int, default=1,
                    help="grade N times and report the MODAL-score run — a noise-robust "
                         "verdict (the model has ~±1-2 run-to-run variance, so a single "
                         "sample is unreliable for gating or comparison)")
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
    if args.anchor:
        ctx += " " + ANCHOR_CALIBRATION
        if args.anchor_same_circuit:
            ctx += (" The reference draws the SAME circuit as the sheet under review, so"
                    " compare how the two READ, not what they contain.")
    if args.engine_clean:
        ctx += " " + ENGINE_CLEAN

    user_content = [{"type": "text", "text": ctx},
                    {"type": "image_url",
                     "image_url": {"url": f"data:image/png;base64,{b64_image(args.image)}"}}]
    if args.anchor:
        user_content.append({"type": "image_url",
                             "image_url": {"url": f"data:image/png;base64,{b64_image(args.anchor)}"}})

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
    def grade_once():
        """One API call → (result, text). Returns None on a request/parse failure."""
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
            return extract_json(text), text
        except json.JSONDecodeError:
            return None, text

    n = max(1, args.samples)
    runs = []  # (score, result, text)
    last_text = ""
    for _ in range(n):
        r, t = grade_once()
        last_text = t
        if r is not None and isinstance(r.get("score"), (int, float)):
            runs.append((float(r["score"]), r, t))
    if not runs:
        print(last_text)
        sys.exit(2)
    # The score is the MEAN of the samples, rounded. The critic reads the same
    # unchanged image a point apart routinely and three apart at worst, so a single
    # read — or the modal of three, which is nearly the median — carries about a
    # point of noise, and an engine change worth half a point cannot be told from
    # nothing. The mean is the efficient estimator here: its error falls as the root
    # of the sample count, where the median's barely moves.
    #
    # The narrative half of the verdict cannot be averaged, so it comes from the
    # single run whose own score sits nearest that mean: real defects, one coherent
    # voice, attached to the score the samples actually support.
    runs.sort(key=lambda x: x[0])
    score_list = [s for s, _, _ in runs]
    mean = sum(score_list) / len(score_list)
    result, text = min(runs, key=lambda x: abs(x[0] - mean))[1:]
    result["score"] = round(mean)
    result["mean"] = round(mean, 2)
    result["samples"] = score_list
    if n > 1 and not args.json_only:
        print(f"# {len(runs)}/{n} samples; scores {score_list}; mean {mean:.2f}")

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
