#!/usr/bin/env python3
"""LLM-as-draughtsman: ask the LLM for a 2D placement GRID (the engine's authored
`layout:` path — the same well-tuned path the high-scoring reference fixtures use, NOT a
soft bias). The LLM arranges refdes into rows x cols following signal flow + clustering;
the engine places them on that grid (enforced by the grid_order cost wall) and fans the
rest. Emits a new circuit.yaml with `layout:` injected.

Usage: python3 tools/grid_plan.py CIRCUIT.circuit.yaml OUT.circuit.yaml [--model gpt-5.4]
"""
import sys, os, json, re, urllib.request

def load(path):
    txt = open(path).read()
    parts = []
    for m in re.finditer(r'^\s+([A-Z]+\d+):\s*\{part:\s*([^,]+),.*?pins:\s*\{(.*?)\}\s*\}\s*$', txt, re.M):
        ref, part, pins = m.group(1), m.group(2).strip(), m.group(3)
        nets = sorted(set(re.findall(r':\s*([A-Za-z0-9_]+)', pins)))
        parts.append((ref, part, nets))
    return txt, parts

def main():
    cir, out = sys.argv[1], sys.argv[2]
    model = sys.argv[sys.argv.index('--model')+1] if '--model' in sys.argv else 'gpt-5.4'
    txt, parts = load(cir)
    lines = [f"{r}  {p}  nets=[{','.join(n)}]" for r, p, n in parts]
    anchors = [r for r, p, n in parts if True]  # let the LLM decide what to grid
    sysp = (
        "You are an expert schematic draughtsman. Arrange the components into a 2D PLACEMENT "
        "GRID (rows top->bottom, columns left->right). Output JSON: a list of rows, each row a "
        "list of refdes strings or null for an empty cell. Rules:\n"
        "1. SIGNAL FLOW left->right: power-input connectors + regulators in the LEFT columns; the "
        "main processing IC(s) in the CENTER columns; peripheral/output connectors in the RIGHT columns.\n"
        "2. CLUSTER each IC with its own decoupling caps, pull-up resistors and local passives in "
        "ADJACENT cells (same/neighbouring columns and rows) — a tight functional block.\n"
        "3. Put a decoupling cap in the cell directly above or beside the IC power pin it serves.\n"
        "4. Keep the grid compact: ~sqrt(N) columns; don't leave huge empty regions; a few nulls for "
        "breathing room between blocks is fine.\n"
        "5. EVERY refdes appears EXACTLY ONCE. Use null (not a string) for empty cells.\n"
        "Output ONLY the JSON list-of-lists."
    )
    user = "Components (refdes, part, nets):\n" + "\n".join(lines)
    body = json.dumps({"model": model, "messages": [
        {"role": "system", "content": sysp}, {"role": "user", "content": user}], "max_tokens": 4000}).encode()
    url = os.environ["OPENAI_BASE_URL"].rstrip("/") + "/chat/completions"
    req = urllib.request.Request(url, data=body, headers={
        "Authorization": "Bearer " + os.environ["OPENAI_API_KEY"], "Content-Type": "application/json"})
    resp = json.load(urllib.request.urlopen(req, timeout=120))
    content = resp["choices"][0]["message"]["content"]
    m = re.search(r'\[\s*\[.*\]\s*\]', content, re.S)
    grid = json.loads(m.group(0))
    known = {r for r, _, _ in parts}
    seen = set()
    # sanitize: keep known refdes once; null otherwise
    clean = []
    for row in grid:
        cr = []
        for cell in row:
            if isinstance(cell, str) and cell in known and cell not in seen:
                seen.add(cell); cr.append(cell)
            else:
                cr.append(None)
        clean.append(cr)
    # append any missing parts as a trailing row so connectivity is complete
    missing = [r for r in known if r not in seen]
    if missing:
        clean.append(missing)
    # to DSL: layout: [[A, B, ~], [C, ~, D]]
    def cell(x): return x if x else "~"
    rows_dsl = "[" + ", ".join("[" + ", ".join(cell(c) for c in row) + "]" for row in clean) + "]"
    # inject under the first block's components: add a sibling `layout:` line
    # find the block indent (components: line) and insert layout: after the components block.
    newtxt = re.sub(r'(\n(\s+)components:\n(?:.*\n)*?)(?=\n\S|\Z)',
                    lambda mo: mo.group(1) + f"{mo.group(2)}layout: {rows_dsl}\n", txt, count=1)
    if "layout:" not in newtxt:
        # fallback: append at end of the block
        newtxt = txt.rstrip() + f"\n    layout: {rows_dsl}\n"
    open(out, "w").write(newtxt)
    print(f"grid {len(clean)}x{max(len(r) for r in clean)}, {len(seen)} placed, {len(missing)} appended -> {out}")

if __name__ == "__main__":
    main()
