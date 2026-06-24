#!/usr/bin/env python3
"""Automatic zone lift: DETERMINISTIC coarse zones (free) gated by a CRITIC A/B.

The VLM's contribution to the hybrid is just rough DIRECTION, which circuit_summary.auto_zones
derives deterministically from the module graph + connector role-names — for free, no VLM call.
The soft bias helps some boards (c19 5→7) but hurts others (c14 7→6), and crossings/body/warnings
are NOT a faithful critic proxy (an objective A/B wrongly kept c14: fewer crossings, worse critic).
So this gates on the CRITIC: keep the zoned layout only if it scores HIGHER. Cheaper than the VLM
loop (the zones are free; only the A/B critic calls cost) but not free.

    set -a; . ./.env; set +a
    python3 tools/autozone.py IN.yaml [OUT_ZONE.json] [--circuit "desc"] [--samples 3]
        -> prints the A/B decision; if zones win, writes them to OUT_ZONE.json ($ZONE_FILE)
"""
import argparse
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
from circuit_summary import auto_zones  # noqa: E402


def render(yaml_path, out_dir, zone_file=None):
    os.makedirs(out_dir, exist_ok=True)
    env = os.environ.copy()
    if zone_file:
        env["ZONE_FILE"] = zone_file
    subprocess.run(
        ["cargo", "run", "--release", "-p", "gordian-core", "--example", "bench_corpus",
         "--", "--out", out_dir, yaml_path],
        cwd=ROOT, capture_output=True, text=True, env=env)
    return os.path.join(out_dir, os.path.basename(yaml_path).replace(".yaml", "") + ".png")


def critic(png, circuit, samples):
    out = subprocess.run(
        ["python3", os.path.join(HERE, "schematic_critic.py"), png, "--circuit", circuit,
         "--samples", str(samples)], cwd=ROOT, capture_output=True, text=True).stdout
    m = re.search(r"^score:\s*([0-9]+)", out, re.MULTILINE)
    return int(m.group(1)) if m else None


def decide(in_yaml, out_zone=None, circuit="schematic", samples=3):
    zones = auto_zones(in_yaml)
    if not zones:
        print("no major parts -> no zones")
        return False
    zf = "/tmp/autozone.json"
    json.dump(zones, open(zf, "w"))
    a = critic(render(in_yaml, "/tmp/autozone/auto"), circuit, samples)
    z = critic(render(in_yaml, "/tmp/autozone/zone", zf), circuit, samples)
    win = (z or -1) > (a or -1)
    print(f"auto={a}  zoned={z}  -> {'KEEP ZONES' if win else 'keep auto'}")
    if win and out_zone:
        json.dump(zones, open(out_zone, "w"))
        print(f"wrote zones -> {out_zone}")
    return win


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("in_yaml")
    ap.add_argument("out_zone", nargs="?")
    ap.add_argument("--circuit", default="schematic")
    ap.add_argument("--samples", type=int, default=3)
    args = ap.parse_args()
    decide(args.in_yaml, args.out_zone, args.circuit, args.samples)
