#!/usr/bin/env python3
"""LLM-as-placement-planner: ask the LLM for a coarse zone map {refdes:[fx,fy]} that the
floorplan engine consumes via $ZONE_FILE (soft zbias). Supplies the GLOBAL structure
(signal flow + functional grouping) the local force-layout can't discover.

Usage: python3 tools/zone_plan.py CIRCUIT.circuit.yaml OUT_ZONE.json [--model gpt-5.4]
Needs OPENAI_API_KEY / OPENAI_BASE_URL in env.
"""
import sys, os, json, re, urllib.request

def load_parts(path):
    # minimal parse: lines like "  REF: {part: X, value: Y, pins: {...}}"
    parts = []
    txt = open(path).read()
    for m in re.finditer(r'^\s+([A-Z]+\d+):\s*\{part:\s*([^,]+),.*?pins:\s*\{(.*?)\}\s*\}\s*$',
                         txt, re.M):
        ref, part, pins = m.group(1), m.group(2).strip(), m.group(3)
        nets = sorted(set(re.findall(r':\s*([A-Za-z0-9_]+)', pins)))
        parts.append((ref, part, nets))
    return parts

def main():
    cir, out = sys.argv[1], sys.argv[2]
    model = 'gpt-5.4'
    if '--model' in sys.argv: model = sys.argv[sys.argv.index('--model')+1]
    parts = load_parts(cir)
    lines = [f"{r}  {p}  nets=[{','.join(n)}]" for r, p, n in parts]
    sys_prompt = (
        "You are an expert schematic-layout planner. Given a netlist, assign each component a "
        "COARSE target position [fx, fy] on the sheet (fx: 0.0=far left ... 1.0=far right; "
        "fy: 0.0=top ... 1.0=bottom). Follow professional draughting conventions:\n"
        "1. SIGNAL FLOW left-to-right: power-input connectors/regulators at the LEFT (fx<0.2); "
        "the main processing IC(s) in the CENTER; peripheral/output connectors at the RIGHT (fx>0.8).\n"
        "2. TIGHT functional clusters: put each IC and ITS OWN decoupling caps, pull-up resistors, "
        "crystal and local passives in the SAME small region — give them nearly identical [fx,fy] "
        "(within ~0.08). A part shared between two ICs goes between them.\n"
        "3. Spread the CLUSTERS apart for readability, but keep each cluster internally tight. "
        "Aim for a handful of distinct clusters, not uniform scatter.\n"
        "4. Decoupling caps (small cap on a supply net + GND) sit immediately next to the IC they serve.\n"
        "Output ONLY a JSON object mapping refdes to [fx,fy], no prose, no markdown."
    )
    user = "Netlist (refdes, part, connected nets):\n" + "\n".join(lines)
    body = json.dumps({
        "model": model,
        "messages": [{"role": "system", "content": sys_prompt},
                     {"role": "user", "content": user}],
        "max_tokens": 4000,
    }).encode()
    url = os.environ["OPENAI_BASE_URL"].rstrip("/") + "/chat/completions"
    req = urllib.request.Request(url, data=body, headers={
        "Authorization": "Bearer " + os.environ["OPENAI_API_KEY"],
        "Content-Type": "application/json"})
    resp = json.load(urllib.request.urlopen(req, timeout=120))
    content = resp["choices"][0]["message"]["content"]
    # strip markdown fences if any, grab the JSON object
    m = re.search(r'\{.*\}', content, re.S)
    zmap = json.loads(m.group(0))
    # clamp + keep only refdes we know
    known = {r for r, _, _ in parts}
    zmap = {k: [max(0.0, min(1.0, float(v[0]))), max(0.0, min(1.0, float(v[1])))]
            for k, v in zmap.items() if k in known and isinstance(v, list) and len(v) == 2}
    json.dump(zmap, open(out, "w"))
    print(f"wrote {len(zmap)} zones to {out}")
    # quick cluster summary
    import statistics as st
    print("fx range:", round(min(v[0] for v in zmap.values()),2), "-", round(max(v[0] for v in zmap.values()),2))

if __name__ == "__main__":
    main()
