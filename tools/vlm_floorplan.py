#!/usr/bin/env python3
"""VLM floorplan A/B loop: keep the better of the auto layout and the VLM-placed layout.

Ties the placement loop together (see docs/specs/schematic-quality-and-the-critic-ceiling.md):

    auto:  render(IN)            -> critic -> score_auto
    vlm :  render(IN) -> vlm_place -> vlm_apply -> render -> critic -> score_vlm
    out :  whichever scored higher  (VLM-placement helps sprawled boards but can hurt tidy
                                      ones by scattering satellites, so the A/B is the safety net)

    set -a; . ./.env; set +a
    python3 tools/vlm_floorplan.py IN.yaml OUT.yaml [--grid 8x6] [--samples 3]

Requires the OpenAI gateway (vlm_place + schematic_critic). If the gateway is down, run the
steps manually with a sub-agent as the VLM placer AND as the critic (same model class, works
when the gateway is 401). Everything but those two model calls is offline/deterministic.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def render(yaml_path, out_dir="/tmp/vlm_fp"):
    os.makedirs(out_dir, exist_ok=True)
    subprocess.run(
        ["cargo", "run", "--release", "-p", "agent", "--example", "bench_corpus",
         "--", "--out", out_dir, yaml_path],
        cwd=ROOT, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    # bench_corpus names the output <basename-minus-.yaml>.png (e.g. c01.circuit.yaml ->
    # c01.circuit.png; c01.circuit.vlm.yaml -> c01.circuit.vlm.png).
    base = os.path.basename(yaml_path)
    stem = base[:-5] if base.endswith(".yaml") else base
    p = os.path.join(out_dir, stem + ".png")
    if os.path.exists(p):
        return p
    raise FileNotFoundError(f"render of {yaml_path} not found at {p}")


def critic(png, circuit, samples):
    out = subprocess.run(
        ["python3", os.path.join(HERE, "schematic_critic.py"), png,
         "--circuit", circuit, "--samples", str(samples)],
        cwd=ROOT, capture_output=True, text=True,
    ).stdout
    m = re.search(r"^score:\s*([0-9]+)", out, re.MULTILINE)
    return int(m.group(1)) if m else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("in_yaml")
    ap.add_argument("out_yaml")
    ap.add_argument("--grid", default="8x6")
    ap.add_argument("--samples", type=int, default=3)
    ap.add_argument("--circuit", default="schematic")
    args = ap.parse_args()

    auto_png = render(args.in_yaml)
    score_auto = critic(auto_png, args.circuit, args.samples)

    fp = json.loads(subprocess.run(
        ["python3", os.path.join(HERE, "vlm_place.py"), auto_png, "--grid", args.grid],
        cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip())
    vlm_yaml = args.in_yaml.replace(".yaml", ".vlm.yaml")
    json.dump(fp, open("/tmp/vlm_fp_floorplan.json", "w"))
    subprocess.run(["python3", os.path.join(HERE, "vlm_apply.py"),
                    args.in_yaml, "/tmp/vlm_fp_floorplan.json", vlm_yaml], cwd=ROOT, check=True)
    vlm_png = render(vlm_yaml)
    score_vlm = critic(vlm_png, args.circuit, args.samples)

    print(f"auto={score_auto}  vlm={score_vlm}  floorplan={fp}")
    keep_vlm = (score_vlm or -1) > (score_auto or -1)
    shutil.copy(vlm_yaml if keep_vlm else args.in_yaml, args.out_yaml)
    print(f"kept {'VLM-placed' if keep_vlm else 'auto'} -> {args.out_yaml}")


if __name__ == "__main__":
    main()
