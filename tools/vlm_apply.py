#!/usr/bin/env python3
"""Apply a VLM-produced floorplan to a circuit YAML as a per-block `layout:` grid.

The VLM placement pass returns a floorplan {refdes: [col,row]} (see coord_overlay.py +
the placer sub-agent). This writes it back into the design so the engine honours it: for
each block that owns floorplanned refdes, it inserts a `layout:` grid placing those refdes
at their cells (`~` elsewhere). This is the deterministic half of the VLM-placement loop:

    render -> coord_overlay -> [VLM placer -> {refdes:[col,row]}] -> vlm_apply -> re-render

    python3 tools/vlm_apply.py IN.yaml FLOORPLAN.json OUT.yaml

Single global grid: every floorplanned refdes is placed in ITS block's grid at the given
(col,row); blocks with no floorplanned refdes are untouched. (Cross-block global cells on a
multi-block design compose per-block by declaration order — keep one block, or one cell row
per block, for a faithful layout.) No YAML lib needed — a small indent-aware line scanner.
"""
import json
import sys


def parse_blocks(lines):
    """Return [(block_name, header_line_idx)] and {refdes: block_name} from the YAML lines."""
    blocks, refdes_block = [], {}
    in_blocks = False
    cur = None
    for i, ln in enumerate(lines):
        s = ln.rstrip("\n")
        if s.strip() == "blocks:" and not s.startswith(" "):
            in_blocks = True
            continue
        if not in_blocks:
            continue
        # block header: exactly 2-space indent, "name:"
        if len(s) > 2 and s[:2] == "  " and s[2] != " " and s.rstrip().endswith(":"):
            cur = s.strip().rstrip(":")
            blocks.append((cur, i))
        # refdes: 6-space indent, "REFDES:" or "REFDES: {...}"
        elif s[:6] == "      " and s[6] != " " and cur is not None:
            name = s.strip().split(":", 1)[0]
            if name and name[0].isalpha():
                refdes_block[name] = cur
    return blocks, refdes_block


def grid_lines(cells):
    """cells: {refdes:[col,row]} for ONE block -> indented YAML `layout:` text lines."""
    maxc = max(c for c, _ in cells.values())
    maxr = max(r for _, r in cells.values())
    at = {(c, r): rd for rd, (c, r) in cells.items()}
    out = ["    layout:\n"]
    for r in range(maxr + 1):
        row = [at.get((c, r), "~") for c in range(maxc + 1)]
        out.append("      - [" + ", ".join(row) + "]\n")
    return out


def apply(in_yaml, floorplan, out_yaml):
    lines = open(in_yaml).readlines()
    fp = json.load(open(floorplan)) if isinstance(floorplan, str) else floorplan
    _, refdes_block = parse_blocks(lines)
    # group floorplan cells by the block that owns each refdes
    by_block = {}
    for rd, cr in fp.items():
        blk = refdes_block.get(rd)
        if blk is None:
            print(f"  warn: {rd} not found in any block, skipped", file=sys.stderr)
            continue
        by_block.setdefault(blk, {})[rd] = cr
    # re-scan for header indices (stable) and insert grids after each block header
    blocks, _ = parse_blocks(lines)
    header_idx = {name: idx for name, idx in blocks}
    # insert from the bottom up so earlier indices stay valid
    for blk in sorted(by_block, key=lambda b: -header_idx[b]):
        ins = grid_lines(by_block[blk])
        i = header_idx[blk] + 1
        lines[i:i] = ins
    open(out_yaml, "w").writelines(lines)
    print(f"applied floorplan ({len(fp)} parts across {len(by_block)} block(s)) -> {out_yaml}")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit("usage: vlm_apply.py IN.yaml FLOORPLAN.json OUT.yaml")
    apply(sys.argv[1], sys.argv[2], sys.argv[3])
