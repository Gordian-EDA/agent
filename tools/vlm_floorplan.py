#!/usr/bin/env python3
"""HYBRID VLM placement loop: LLM gives coarse zones, engine places, critic closes the loop.

The LLM is good at rough direction but not millimetre positions, so it returns COARSE zones
({refdes:[fx,fy]}, via vlm_place.py) that the engine applies as a SOFT bias ($ZONE_FILE ->
LayoutIr.zone -> proxy_cost zbias) — the engine still does the precise placement. The critic
scores each result and feeds its defects back so the LLM can REFINE the zones (the
iterate-on-the-rendered-result pattern of LLM design tools). The best-scoring layout wins; the
auto baseline is always in the running so a refine never ships worse.

    auto:   render(IN)                                   -> critic -> score_auto
    iter i: vlm_place(prev_render, +critic_feedback) -> $ZONE_FILE
            render(IN, ZONE_FILE=bias) -> critic -> score_i ; prev_render = this render
    out:    the best of {auto, iter0, iter1, ...}; writes the winning zones to OUT (or notes auto)

    set -a; . ./.env; set +a
    python3 tools/vlm_floorplan.py IN.yaml OUT.zones.json [--grid 4x3] [--samples 3] [--iters 2]
"""
import argparse
import json
import os
import re
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def render(yaml_path, out_dir, zone_file=None):
    os.makedirs(out_dir, exist_ok=True)
    env = os.environ.copy()
    if zone_file:
        env["ZONE_FILE"] = zone_file
    subprocess.run(
        ["cargo", "run", "--release", "-p", "agent", "--example", "bench_corpus",
         "--", "--out", out_dir, yaml_path],
        cwd=ROOT, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env,
    )
    base = os.path.basename(yaml_path)
    stem = base[:-5] if base.endswith(".yaml") else base
    return os.path.join(out_dir, stem + ".png")


def critic(png, circuit, samples):
    out = subprocess.run(
        ["python3", os.path.join(HERE, "schematic_critic.py"), png,
         "--circuit", circuit, "--samples", str(samples)],
        cwd=ROOT, capture_output=True, text=True,
    ).stdout
    m = re.search(r"^score:\s*([0-9]+)\s*/?\s*[0-9]*\s*[—-]?\s*(.*)", out, re.MULTILINE)
    return (int(m.group(1)), m.group(2).strip()) if m else (None, "")


def circuit_summary(in_yaml):
    return subprocess.run(
        ["python3", os.path.join(HERE, "circuit_summary.py"), in_yaml],
        cwd=ROOT, capture_output=True, text=True).stdout.strip()


def vlm_zones(image, grid, zone_file, context, summary):
    cmd = ["python3", os.path.join(HERE, "vlm_place.py"), image, "--grid", grid, "--out", zone_file]
    if context:
        cmd += ["--context", context]
    if summary:
        cmd += ["--summary", summary]
    subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, check=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("in_yaml")
    ap.add_argument("out_zones")
    ap.add_argument("--grid", default="4x3")
    ap.add_argument("--samples", type=int, default=3)
    ap.add_argument("--iters", type=int, default=2)
    ap.add_argument("--circuit", default="schematic")
    args = ap.parse_args()
    work = "/tmp/vlm_hybrid"

    auto_png = render(args.in_yaml, os.path.join(work, "auto"))
    s_auto, _ = critic(auto_png, args.circuit, args.samples)
    best = {"label": "auto", "score": s_auto if s_auto is not None else -1, "zones": None}
    print(f"auto={s_auto}")

    summary = circuit_summary(args.in_yaml)
    prev_png, context = auto_png, None
    for i in range(args.iters):
        zf = os.path.join(work, f"zone{i}.json")
        vlm_zones(prev_png, args.grid, zf, context, summary)
        png = render(args.in_yaml, os.path.join(work, f"z{i}"), zone_file=zf)
        s, summary = critic(png, args.circuit, args.samples)
        zones = json.load(open(zf))
        print(f"iter{i}: score={s}  zones={zones}")
        if s is not None and s > best["score"]:
            best = {"label": f"iter{i}", "score": s, "zones": zones}
        prev_png = png
        context = f"This layout scored {s}/10. Critic said: {summary}"
        if s is not None and s >= 9:
            break

    if best["zones"] is not None:
        json.dump(best["zones"], open(args.out_zones, "w"))
        print(f"BEST = {best['label']} ({best['score']}/10) -> wrote zones to {args.out_zones}")
    else:
        print(f"BEST = auto ({best['score']}/10) -> no zone bias improves it")


if __name__ == "__main__":
    main()
