#!/usr/bin/env python3
"""Emit a multi-BLOCK circuit as MULTI-SHEET (one clean sheet per block) and critic each.

The single-sheet sprawl ceiling (a complete board crammed on one sheet scores ~5-7) is escaped
the professional way: draw each block on its own sheet. Each sheet is small enough to land where
the clean fixtures do (9-10), and inter-block nets become labeled ports automatically (a shared
net is single-pin within a block -> auto-port; power nets -> power symbols). Demonstrated: c01
(composed = 5) -> power 9 / mcu 8 / io (fixable overlap).

    set -a; . ./.env; set +a
    python3 tools/multisheet.py MULTIBLOCK.circuit.yaml [--samples 3] [--circuit "desc"]

Splits the YAML into one single-block circuit per block (reusing the engine's normal render),
renders + critics each, and prints per-sheet scores + the MIN (a board is 9+ iff every sheet is).
No Rust change — the agent already partitions into blocks; this is the per-block emit + per-sheet
critic on top of the existing single-block renderer.
"""
import argparse
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def split_blocks(path):
    """Parse a multi-block YAML -> {block_name: [component-line, ...]} (indent-aware, no YAML lib)."""
    blocks, cur, in_comps = {}, None, False
    in_blocks = False
    for ln in open(path):
        s = ln.rstrip("\n")
        if s.strip() == "blocks:" and not s.startswith(" "):
            in_blocks = True
            continue
        if not in_blocks:
            continue
        if len(s) > 2 and s[:2] == "  " and s[2] != " " and s.rstrip().endswith(":"):
            cur = s.strip().rstrip(":")
            blocks[cur] = []
            in_comps = False
        elif s.strip() == "components:":
            in_comps = True
        elif in_comps and s[:6] == "      " and s[6] != " " and cur is not None:
            blocks[cur].append(s.strip())
    return blocks


def render(yaml_path, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    subprocess.run(
        ["cargo", "run", "--release", "-p", "gordian-core", "--example", "bench_corpus",
         "--", "--out", out_dir, yaml_path],
        cwd=ROOT, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    stem = os.path.basename(yaml_path)[:-5]
    return os.path.join(out_dir, stem + ".png")


def critic(png, desc, samples):
    out = subprocess.run(
        ["python3", os.path.join(HERE, "schematic_critic.py"), png, "--circuit", desc,
         "--samples", str(samples)], cwd=ROOT, capture_output=True, text=True).stdout
    m = re.search(r"^score:\s*([0-9]+)", out, re.MULTILINE)
    return int(m.group(1)) if m else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("in_yaml")
    ap.add_argument("--samples", type=int, default=3)
    ap.add_argument("--circuit", default="circuit")
    args = ap.parse_args()
    blocks = split_blocks(args.in_yaml)
    if len(blocks) < 2:
        sys.exit(f"only {len(blocks)} block(s) — multi-sheet needs a multi-block design")
    work = "/tmp/multisheet"
    os.makedirs(work, exist_ok=True)
    scores = {}
    for name, comps in blocks.items():
        if not comps:
            continue
        sub = os.path.join(work, f"{name}.circuit.yaml")
        with open(sub, "w") as f:
            f.write("version: 1\nblocks:\n  main:\n    components:\n")
            for c in comps:
                f.write(f"      {c}\n")
        png = render(sub, work)
        scores[name] = critic(png, f"{args.circuit} — {name} sheet", args.samples)
        print(f"  sheet {name}: {scores[name]}/10")
    vals = [v for v in scores.values() if v is not None]
    worst = min(vals) if vals else None
    print(f"MULTI-SHEET: {len(scores)} sheets, scores={scores}, MIN={worst}"
          f"  ({'ALL >=9' if worst and worst >= 9 else 'gated by min'})")


if __name__ == "__main__":
    main()
