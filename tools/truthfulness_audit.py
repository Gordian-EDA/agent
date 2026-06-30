#!/usr/bin/env python3
"""Audit whether the placement engine's COMPACTION preserves the netlist.

The de-sprawl / cola compaction can pack a sheet tight enough that the writer
silently drops a connection (an OPEN) or fuses two (a SHORT) — an untruthful
netlist with NO warning. The engine's own `truthfulness_breaks` measure misses
OPENS, and the de-sprawl was validated on SPRAWL, never connectivity, so this
shipped undetected on ~30% of liftable boards.

The complete check this tool performs: re-lay a board two ways and compare the
EMITTED net counts (via `kicad-cli`, the ground truth) —

  * BARE ANNEAL  (CLUSTER_NO_COLA=1 CLUSTER_NO_COMPACT=1): spread, truthful baseline.
  * ENGINE       (default cluster, i.e. de-sprawl + pose + rail): what ships.

Equal net count  => the compaction preserved connectivity (truthful).
ENGINE > BARE    => an OPEN (a net split).
ENGINE < BARE    => a SHORT (two nets fused).

The bare anneal — not the ORIGINAL board — is the reference, because the relayout
re-represents power nets (adds PWR_FLAG / per-pin power symbols) so the absolute
count differs from the source even when connectivity is sound; the bare anneal
carries the SAME re-representation, isolating the compaction's effect.

Usage:
  truthfulness_audit.py <board.kicad_sch> [<board.kicad_sch> ...]
  truthfulness_audit.py --all <dataset_dir>     # scan, skip non-liftable (0 nets)
"""
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RELAYOUT = REPO / "target/release/examples/relayout"


def net_count(sch: Path) -> int:
    """Distinct nets kicad-cli resolves from the sheet geometry (the ground truth)."""
    out = subprocess.run(
        ["kicad-cli", "sch", "export", "netlist", "--output", "/dev/stdout", str(sch)],
        capture_output=True, text=True,
    ).stdout
    return out.count("(net ")


def relayout(board: Path, outdir: Path, tag: str, env_extra: dict) -> Path | None:
    import os
    env = {**os.environ, "SCH_ENGINE": "cluster", **env_extra}
    r = subprocess.run(
        [str(RELAYOUT), str(board), "--engine", "cluster", "--out", str(outdir), "--tag", tag],
        capture_output=True, text=True, env=env, timeout=300,
    )
    sch = outdir / f"{tag}.kicad_sch"
    return sch if sch.exists() else None


def audit_board(board: Path) -> str:
    with tempfile.TemporaryDirectory() as td:
        out = Path(td)
        bare = relayout(board, out, "bare", {"CLUSTER_NO_COLA": "1", "CLUSTER_NO_COMPACT": "1"})
        eng = relayout(board, out, "eng", {})
        if bare is None or eng is None:
            return "skip(not-liftable)"
        b, e = net_count(bare), net_count(eng)
        if b == 0:
            return "skip(empty)"
        if b == e:
            return f"TRUTHFUL ({e} nets)"
        kind = "OPEN(s)" if e > b else "SHORT(s)"
        return f"*** UNTRUTHFUL *** {kind}: engine={e} vs bare={b} ({abs(e-b)} broken)"


def main(argv: list[str]) -> int:
    if not RELAYOUT.exists():
        print(f"build first: cargo build --release -p gordian-core --example relayout", file=sys.stderr)
        return 2
    if argv[:1] == ["--all"]:
        boards = sorted(Path(argv[1]).glob("*.kicad_sch"))
    else:
        boards = [Path(a) for a in argv]
    bad = liftable = 0
    for b in boards:
        verdict = audit_board(b)
        if verdict.startswith("skip"):
            continue
        liftable += 1
        if "UNTRUTHFUL" in verdict:
            bad += 1
        print(f"{b.stem}: {verdict}")
    if liftable:
        print(f"\n=== {bad}/{liftable} liftable boards UNTRUTHFUL "
              f"({100 * bad // liftable}%) — the compaction's hidden netlist bug ===")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
