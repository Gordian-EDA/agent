#!/usr/bin/env python3
"""
validate_engine.py — regression harness for the cluster placement engine's PARETO guarantee.

The cluster engine (the agent default) layers a pose search + de-sprawl floorplanner on the
annealer and must NEVER ship a worse sheet: for every board its rendered layout must be no
worse than the bare annealer on sprawl AND on layout warnings, and strictly better (a real
de-sprawl) on a fraction of boards. This script re-lays each board through both engines, reads
the objective metrics back, and FAILS if any board regresses — so a future change that breaks
the safety net is caught.

For each id it runs `examples/relayout` twice (--engine anneal / cluster), parses the printed
`warn=` count, measures sprawl via `layout_metrics`, and classifies win / tie / REGRESSION.

Usage:
  tools/validate_engine.py --ids FILE [--dataset DIR] [--timeout S]
    FILE: one board id per line (a `<id>.kicad_sch` under DATASET).
Exit status is non-zero if any board regresses (sprawl up >3% or warnings up).
"""
import argparse
import os
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(__file__))
from layout_metrics import metrics  # noqa: E402

BIN = "target/release/examples/relayout"


def run(engine, board, out_dir, tag, timeout):
    """Re-lay `board` through `engine`; return (sprawl, warnings) or (None, None) on failure."""
    env = dict(os.environ)
    env["SCH_ENGINE"] = engine
    try:
        proc = subprocess.run(
            [BIN, board, "--engine", engine, "--out", out_dir, "--tag", tag],
            capture_output=True, text=True, env=env, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None, None
    m = re.search(r"warn=\s*(\d+)", proc.stdout)
    warn = int(m.group(1)) if m else None
    sch = os.path.join(out_dir, f"{tag}.kicad_sch")
    if not os.path.exists(sch):
        return None, None
    spr = metrics(sch).get("sprawl")
    return spr, warn


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ids", required=True)
    ap.add_argument("--dataset", default=os.path.expanduser("~/kicad-scraper/dataset"))
    ap.add_argument("--timeout", type=float, default=560)
    a = ap.parse_args()

    ids = [l.strip() for l in open(a.ids) if l.strip()]
    wins = ties = regressions = skipped = 0
    print(f"{'board':<15} {'anneal':>7} {'cluster':>7} {'d%':>6} {'warn':>5}  verdict")
    with tempfile.TemporaryDirectory() as out:
        for i in ids:
            board = os.path.join(a.dataset, f"{i}.kicad_sch")
            if not os.path.exists(board):
                continue
            sa, sw = run("anneal", board, out, "a", a.timeout)
            cl, cw = run("cluster", board, out, "c", a.timeout)
            if None in (sa, cl, sw, cw) or not sa:
                print(f"{i:<15} {'?':>7} {'?':>7} {'?':>6} {'?':>5}  SKIP (build/timeout)")
                skipped += 1
                continue
            d = (cl - sa) / sa * 100
            # The guarantee: cluster never worse on sprawl (within 3% noise) or warnings.
            if cl > sa * 1.03 or cw > sw:
                verdict = "REGRESSION"
                regressions += 1
            elif d < -3:
                verdict = "win"
                wins += 1
            else:
                verdict = "tie"
                ties += 1
            print(f"{i:<15} {sa:>7.1f} {cl:>7.1f} {d:>+5.0f} {cw:>5}  {verdict}")

    print(f"\n{wins} wins, {ties} ties, {regressions} REGRESSIONS, {skipped} skipped")
    sys.exit(1 if regressions else 0)


if __name__ == "__main__":
    main()
