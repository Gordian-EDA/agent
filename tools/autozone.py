#!/usr/bin/env python3
"""FREE automatic zone lift: deterministic coarse zones + an OBJECTIVE (no-gateway) A/B.

The VLM's contribution to the hybrid is just rough DIRECTION, which circuit_summary.auto_zones
derives deterministically from the module graph + connector role-names — for free, no gateway.
The soft bias helps simple boards (c19 5→7) but can introduce a body crossing on complex
satellite-heavy ones (c01), so this keeps the zoned layout ONLY if it's objectively better
(fewer wire crossings, and no new body crossings or warnings). Objective metrics are a free,
fast proxy for the critic that aligned with it on the validated boards (keeps c19, rejects c01).

    python3 tools/autozone.py IN.yaml [OUT_ZONE.json]
        -> prints the A/B decision; if zones win, writes them to OUT_ZONE.json ($ZONE_FILE)
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
from circuit_summary import auto_zones  # noqa: E402


def metrics(yaml_path, out_dir, zone_file=None):
    os.makedirs(out_dir, exist_ok=True)
    env = os.environ.copy()
    if zone_file:
        env["ZONE_FILE"] = zone_file
    out = subprocess.run(
        ["cargo", "run", "--release", "-p", "agent", "--example", "bench_corpus",
         "--", "--out", out_dir, yaml_path],
        cwd=ROOT, capture_output=True, text=True, env=env).stdout
    stem = os.path.basename(yaml_path).replace(".yaml", "")
    for ln in out.splitlines():
        f = ln.split()
        if f and f[0] == stem:
            # stem parts pins nets warn body ic xing anneal
            return {"warn": int(f[4]), "body": int(f[5]), "xing": int(f[7])}
    raise RuntimeError(f"no metrics for {stem} in:\n{out}")


def decide(in_yaml, out_zone=None):
    zones = auto_zones(in_yaml)
    if not zones:
        print("no major parts -> no zones")
        return False
    zf = "/tmp/autozone.json"
    json.dump(zones, open(zf, "w"))
    a = metrics(in_yaml, "/tmp/autozone/auto")
    z = metrics(in_yaml, "/tmp/autozone/zone", zf)
    win = z["body"] <= a["body"] and z["warn"] <= a["warn"] and z["xing"] < a["xing"]
    print(f"auto={a}  zoned={z}  -> {'KEEP ZONES' if win else 'keep auto'}")
    if win and out_zone:
        json.dump(zones, open(out_zone, "w"))
        print(f"wrote zones -> {out_zone}")
    return win


if __name__ == "__main__":
    if not 2 <= len(sys.argv) <= 3:
        sys.exit("usage: autozone.py IN.yaml [OUT_ZONE.json]")
    decide(sys.argv[1], sys.argv[2] if len(sys.argv) == 3 else None)
