#!/usr/bin/env python3
"""Audit whether the placement engine PRESERVES a board's netlist when it re-lays it.

A placement engine only MOVES parts; it must never change which pins are
electrically connected. But the de-sprawl/cola compaction can pack a sheet tight
enough that the writer silently drops a connection (an OPEN) or fuses two (a
SHORT) — with no warning. The engine's own `truthfulness_breaks` measure misses
opens, and de-sprawl was validated on SPRAWL, never connectivity, so this shipped
undetected.

THE CHECK (ground-truth): re-lay the board with the engine, then compare the
emitted netlist's PIN-GROUPINGS to the ORIGINAL board's pin-groupings. The
original board IS the intended netlist; the engine must reproduce it exactly.

  * A group in ORIGINAL but not the engine output = an OPEN (a connection dropped).
  * A group in the engine output but not ORIGINAL  = a SHORT (pins wrongly fused).

We compare GROUPINGS (the frozenset of (refdes,pin) on each multi-pin net), not
net counts or names: net auto-names and power re-representation (PWR_FLAG / #FLG
helper symbols, single-pin stubs) differ harmlessly between source and re-emit, so
those are excluded — only real component-to-component connectivity is compared.

NOTE: an earlier version of this tool compared net COUNTS against a "bare anneal"
baseline. That was WRONG — the bare anneal can itself break a net, so it is not a
trustworthy reference. The ORIGINAL board is the only correct reference.

Usage:
  truthfulness_audit.py <board.kicad_sch> [<board.kicad_sch> ...]
  truthfulness_audit.py --all <dataset_dir>    # scan; non-liftable boards are skipped
"""
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RELAYOUT = REPO / "target/release/examples/relayout"


def groupings(sch: Path) -> set[frozenset]:
    """The component-to-component connectivity: the frozenset of (refdes,pin) on each
    multi-pin net, excluding power-helper refs (#FLG/#PWR) and single-pin stubs."""
    out = subprocess.run(
        ["kicad-cli", "sch", "export", "netlist", "--output", "/dev/stdout", str(sch)],
        capture_output=True, text=True,
    ).stdout
    groups = set()
    for block in re.split(r"\(net ", out):
        nodes = frozenset(
            (r, p)
            for r, p in re.findall(r'\(node \(ref "([^"]+)"\) \(pin "([^"]+)"', block)
            if not r.startswith("#")
        )
        if len(nodes) >= 2:
            groups.add(nodes)
    return groups


def relayout(board: Path, outdir: Path) -> Path | None:
    env = {**os.environ, "SCH_ENGINE": "cluster"}
    subprocess.run(
        [str(RELAYOUT), str(board), "--engine", "cluster", "--out", str(outdir), "--tag", "eng"],
        capture_output=True, text=True, env=env, timeout=400,
    )
    sch = outdir / "eng.kicad_sch"
    return sch if sch.exists() else None


def audit_board(board: Path) -> str:
    orig = groupings(board)
    if not orig:
        return "skip(not-liftable/empty)"
    with tempfile.TemporaryDirectory() as td:
        eng = relayout(board, Path(td))
        if eng is None:
            return "skip(engine-empty)"
        out = groupings(eng)
    opens = orig - out          # in original, lost by the engine
    shorts = out - orig         # invented by the engine
    if not opens and not shorts:
        return f"TRUTHFUL ({len(orig)} nets)"
    parts = []
    if opens:
        parts.append(f"{len(opens)} OPEN(s)")
    if shorts:
        parts.append(f"{len(shorts)} SHORT(s)")
    return f"*** UNTRUTHFUL *** {', '.join(parts)}"


def main(argv: list[str]) -> int:
    if not RELAYOUT.exists():
        print("build first: cargo build --release -p gordian-core --example relayout", file=sys.stderr)
        return 2
    boards = sorted(Path(argv[1]).glob("*.kicad_sch")) if argv[:1] == ["--all"] else [Path(a) for a in argv]
    bad = liftable = 0
    for b in boards:
        v = audit_board(b)
        if v.startswith("skip"):
            continue
        liftable += 1
        bad += "UNTRUTHFUL" in v
        print(f"{b.stem}: {v}")
    if liftable:
        print(f"\n=== {bad}/{liftable} liftable boards UNTRUTHFUL "
              f"({100 * bad // liftable}%) — engine changed the netlist ===")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
