#!/usr/bin/env python3
"""Deterministic schematic-layout metrics from a .kicad_sch — an OBJECTIVE, low-noise,
zero-variance complement to the (subjective) VLM critic. Computes mined human-layout
proxies so "is our layout human-like?" becomes a MEASURED percentile rank against a
500-design human corpus, not an opinion.

Usage:
  python3 tools/layout_metrics.py FILE.kicad_sch [FILE2 ...]            # per-file JSON
  python3 tools/layout_metrics.py --agg GLOB                           # quick aggregate
  python3 tools/layout_metrics.py --percentiles [GLOB]                 # build corpus_baseline.json
  python3 tools/layout_metrics.py --score OURS.kicad_sch [--baseline F]  # rank vs humans

Metrics (per sheet):
  n_parts            component instances (excludes power symbols / lib defs)
  n_wires, wire_len_median/p90/max, wire_frac_gt50   # label-vs-wire rule (humans ~0% >50mm)
  n_labels, n_power_syms, label_per_part
  bbox_w/h, sprawl   # bbox_area / sum(part_bbox_area): 1=perfectly packed, higher=more whitespace
  n_clusters, cluster_ratio (clusters/parts), min_gap   # block-disjointness (single-linkage @25.4mm)
  wire_crossings, crossings_per_wire   # axis-aligned segment intersections — fewer reads cleaner

The corpus default is ~/kicad-scraper/dataset/*.kicad_sch (override via $KICAD_CORPUS or a
GLOB arg). The baseline JSON is the committed reference artifact.
"""
import os, sys, json, math, glob, statistics as st

DEFAULT_CORPUS = os.path.expanduser(os.environ.get('KICAD_CORPUS', '~/kicad-scraper/dataset'))
BASELINE_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'corpus_baseline.json')

# Size-normalized readability metrics scored against the human distribution. `bad` is the
# direction in which a design reads WORSE (used only for the human-readable verdict tail);
# the rolled-up human-likeness score is symmetric distance-to-median, so direction never
# biases the number — it only labels which tail an outlier sits in.
SCORED_METRICS = {
    'wire_len_median':   'high',  # long wires read worse than short hops + labels
    'wire_len_p90':      'high',
    'wire_frac_gt50':    'high',  # humans keep ~0% of wires >50mm (they use labels)
    'crossings_per_wire':'high',  # crossings hurt readability
    'sprawl':            'high',  # whitespace bloat
    'label_per_part':    'two',   # humans cluster around a norm; both tails are unusual
    'cluster_ratio':     'two',   # too fragmented or one giant blob both read oddly
    'min_gap':           'two',
}

def tokenize(s):
    # strip block to speed: we only need symbol/wire/label/at/xy/property/lib_id/lib_symbols tokens,
    # but a full tokenizer is simplest + robust.
    toks, i, n = [], 0, len(s)
    while i < n:
        c = s[i]
        if c in '()':
            toks.append(c); i += 1
        elif c == '"':
            j = i + 1; buf = []
            while j < n and s[j] != '"':
                if s[j] == '\\' and j + 1 < n:
                    buf.append(s[j+1]); j += 2
                else:
                    buf.append(s[j]); j += 1
            toks.append('"' + ''.join(buf)); i = j + 1
        elif c.isspace():
            i += 1
        else:
            j = i
            while j < n and not s[j].isspace() and s[j] not in '()"':
                j += 1
            toks.append(s[i:j]); i = j
    return toks

def parse(toks):
    # returns nested lists; atoms are str (quoted strings prefixed with '"')
    pos = 0
    def rd():
        nonlocal pos
        t = toks[pos]; pos += 1
        if t == '(':
            lst = []
            while toks[pos] != ')':
                lst.append(rd())
            pos += 1
            return lst
        return t
    out = []
    while pos < len(toks):
        out.append(rd())
    return out

def head(node): return node[0] if isinstance(node, list) and node and isinstance(node[0], str) else None
def find_all(node, name):
    if not isinstance(node, list): return
    if head(node) == name: yield node
    for c in node:
        if isinstance(c, list): yield from find_all(c, name)

def get_at(node):
    for c in node:
        if isinstance(c, list) and head(c) == 'at':
            try: return (float(c[1]), float(c[2]))
            except: return None
    return None

def metrics(path):
    s = open(path, errors='replace').read()
    tree = parse(tokenize(s))
    root = tree[0] if tree else []
    # top-level children only (avoid lib_symbols instance defs)
    children = [c for c in root if isinstance(c, list)]
    parts, power = [], 0
    for c in children:
        if head(c) != 'symbol': continue
        lib = next((x[1] for x in c if isinstance(x, list) and head(x) == 'lib_id'), '')
        at = get_at(c)
        if at is None: continue
        if lib.startswith('"power:') or lib.startswith('power:'):
            power += 1; continue
        parts.append(at)
    # wires (top-level): collect lengths + segments (for crossing detection)
    wlens, segs = [], []
    for w in children:
        if head(w) != 'wire': continue
        pts = next((x for x in w if isinstance(x, list) and head(x) == 'pts'), None)
        if not pts: continue
        xy = [(float(p[1]), float(p[2])) for p in pts if head(p) == 'xy']
        for a, b in zip(xy, xy[1:]):
            wlens.append(abs(a[0]-b[0]) + abs(a[1]-b[1]))
            if a != b: segs.append((a, b))
    labels = sum(1 for c in children if head(c) in ('label', 'global_label', 'hierarchical_label'))

    np_ = len(parts)
    res = {'file': path.split('/')[-1], 'n_parts': np_, 'n_wires': len(wlens),
           'n_labels': labels, 'n_power_syms': power}
    if wlens:
        wl = sorted(wlens)
        res['wire_len_median'] = round(st.median(wl), 1)
        res['wire_len_p90'] = round(wl[int(0.9*(len(wl)-1))], 1)
        res['wire_len_max'] = round(max(wl), 1)
        res['wire_frac_gt50'] = round(sum(1 for x in wl if x > 50) / len(wl), 3)
        x = count_crossings(segs)
        res['wire_crossings'] = x
        res['crossings_per_wire'] = round(x / len(segs), 3)
    if np_:
        res['label_per_part'] = round(labels / np_, 2)
        xs = [p[0] for p in parts]; ys = [p[1] for p in parts]
        bw, bh = max(xs)-min(xs), max(ys)-min(ys)
        res['bbox_w'] = round(bw, 1); res['bbox_h'] = round(bh, 1)
        # sprawl: bbox area / (parts * typical part cell ~ 6.35*5.08). Proxy for whitespace.
        cell = 6.35 * 5.08
        res['sprawl'] = round((bw*bh) / max(np_*cell, 1), 2) if bw>0 and bh>0 else None
        # single-linkage clustering at 25.4mm (one inch ~ one block gap)
        clusters = single_linkage(parts, 25.4)
        res['n_clusters'] = len(clusters)
        res['cluster_ratio'] = round(len(clusters)/np_, 2)
        res['min_gap'] = round(min_cluster_gap(parts, clusters), 1) if len(clusters) > 1 else None
    return res

def single_linkage(pts, thr):
    n = len(pts); parent = list(range(n))
    def f(x):
        while parent[x] != x: parent[x] = parent[parent[x]]; x = parent[x]
        return x
    for i in range(n):
        for j in range(i+1, n):
            if abs(pts[i][0]-pts[j][0]) + abs(pts[i][1]-pts[j][1]) <= thr:
                parent[f(i)] = f(j)
    groups = {}
    for i in range(n): groups.setdefault(f(i), []).append(i)
    return list(groups.values())

def min_cluster_gap(pts, clusters):
    cents = []
    for cl in clusters:
        xs = [pts[i][0] for i in cl]; ys = [pts[i][1] for i in cl]
        cents.append(((min(xs), min(ys), max(xs), max(ys))))
    best = 1e9
    for i in range(len(cents)):
        for j in range(i+1, len(cents)):
            a, b = cents[i], cents[j]
            dx = max(0, max(a[0]-b[2], b[0]-a[2]))
            dy = max(0, max(a[1]-b[3], b[1]-a[3]))
            best = min(best, math.hypot(dx, dy))
    return best

def count_crossings(segs):
    """Axis-aligned H/V wire crossings: an H and a V segment that intersect at a point
    INTERIOR to at least one of them. Shared endpoints (corners, T/+ junctions where a
    segment ends) don't count — only true overpass crossings, which read as clutter.
    Deterministic, O(#segments^2)."""
    H, V = [], []
    for (x0, y0), (x1, y1) in segs:
        if y0 == y1:   H.append((min(x0, x1), max(x0, x1), y0))
        elif x0 == x1: V.append((min(y0, y1), max(y0, y1), x0))
    n = 0
    for hx0, hx1, hy in H:
        for vy0, vy1, vx in V:
            if hx0 <= vx <= hx1 and vy0 <= hy <= vy1:
                interior_h = hx0 < vx < hx1   # crossing point not at the H seg's ends
                interior_v = vy0 < hy < vy1
                if interior_h or interior_v:
                    n += 1
    return n

# ---- percentile baseline & scoring ----

def percentile(sorted_vals, q):
    """Linear-interpolated percentile (q in [0,1]) of a pre-sorted list."""
    if not sorted_vals: return None
    if len(sorted_vals) == 1: return sorted_vals[0]
    pos = q * (len(sorted_vals) - 1)
    lo = int(math.floor(pos)); hi = int(math.ceil(pos))
    frac = pos - lo
    return sorted_vals[lo] * (1 - frac) + sorted_vals[hi] * frac

def expand_files(args):
    files = []
    for a in args:
        g = glob.glob(a)
        files.extend(g if g else [a])
    return files

def collect_rows(files):
    rows, fails = [], 0
    for fp in files:
        try: rows.append(metrics(fp))
        except Exception as e:
            fails += 1
            print(f"FAIL {fp}: {e}", file=sys.stderr)
    return rows, fails

def build_baseline(files):
    rows, fails = collect_rows(files)
    keys = list(SCORED_METRICS) + ['n_parts', 'n_wires', 'n_labels', 'n_power_syms',
                                   'bbox_w', 'bbox_h', 'wire_len_max', 'wire_crossings']
    seen = []
    for k in keys:
        if k not in seen: seen.append(k)
    out = {'_corpus_n': len(files), '_parsed': len(rows), '_failed': fails, 'metrics': {}}
    qs = {'p10': .10, 'p25': .25, 'p50': .50, 'p75': .75, 'p90': .90}
    for k in seen:
        vals = sorted(r[k] for r in rows if r.get(k) is not None)
        if not vals: continue
        out['metrics'][k] = {'n': len(vals),
                             **{name: round(percentile(vals, q), 4) for name, q in qs.items()}}
    return out, rows

def load_baseline(path):
    with open(path) as f: return json.load(f)

def score(path, baseline):
    """Rank OUR design's metrics against the human corpus CDF stored in the baseline.
    The baseline keeps only percentile bands, so rank_in_band interpolates the rank
    within the bracketing band (deterministic, no corpus re-read needed)."""
    m = metrics(path)
    bm = baseline['metrics']
    BANDS = [('p10', .10), ('p25', .25), ('p50', .50), ('p75', .75), ('p90', .90)]

    def rank_in_band(v, band):
        """Interpolate v's percentile within the band's [p10..p90] knots; clamp tails."""
        knots = [(q, band[name]) for name, q in BANDS]
        knots.sort(key=lambda kv: kv[1])
        if v <= knots[0][1]:  return knots[0][0]
        if v >= knots[-1][1]: return knots[-1][0]
        for (q0, x0), (q1, x1) in zip(knots, knots[1:]):
            if x0 <= v <= x1:
                f = 0 if x1 == x0 else (v - x0) / (x1 - x0)
                return q0 + f * (q1 - q0)
        return knots[-1][0]

    per_metric, dists = {}, []
    for k, direction in SCORED_METRICS.items():
        v = m.get(k)
        if v is None or k not in bm: continue
        band = bm[k]
        pr = rank_in_band(v, band)            # 0..1 percentile vs humans
        dist = abs(pr - 0.50) * 2             # 0 at median, 1 at either tail
        dists.append(dist)
        # verdict: which tail and how far
        tail = 'within' if dist < 0.5 else ('high tail' if pr > 0.5 else 'low tail')
        worse = (direction == 'high' and pr > 0.5) or (direction == 'two' and dist >= 0.5)
        per_metric[k] = {
            'value': v, 'p50_human': band['p50'], 'percentile': round(pr * 100, 1),
            'tail': tail, 'worse_than_human': bool(worse),
            'verdict': f"{k}: {v} -> p{round(pr*100)} ({'WORSE: ' if worse else ''}"
                       f"human p50={band['p50']}, {tail})",
        }
    human_likeness = round(100 * (1 - st.mean(dists)), 1) if dists else None
    return {'file': m['file'], 'human_likeness': human_likeness,
            'size': {k: m.get(k) for k in ('n_parts', 'n_wires', 'n_labels', 'n_power_syms')},
            'metrics': per_metric, 'raw': m}

def print_score(rep):
    print(f"\n=== {rep['file']} ===")
    print(f"size: {rep['size']}")
    hl = rep['human_likeness']
    print(f"HUMAN-LIKENESS: {hl}/100  "
          f"({'human-like' if hl is not None and hl >= 60 else 'off-distribution'})"
          if hl is not None else "HUMAN-LIKENESS: n/a (no scorable metrics)")
    for k, d in sorted(rep['metrics'].items(), key=lambda kv: -abs(kv[1]['percentile'] - 50)):
        flag = '  <== WORSE THAN HUMANS' if d['worse_than_human'] else ''
        print(f"  {d['verdict']}{flag}")

def main():
    args = sys.argv[1:]

    if args and args[0] == '--percentiles':
        rest = args[1:]
        files = expand_files(rest) if rest else glob.glob(os.path.join(DEFAULT_CORPUS, '*.kicad_sch'))
        if not files:
            print(f"no corpus files (looked in {DEFAULT_CORPUS}); set $KICAD_CORPUS or pass a GLOB",
                  file=sys.stderr)
            sys.exit(2)
        baseline, rows = build_baseline(files)
        with open(BASELINE_PATH, 'w') as f:
            json.dump(baseline, f, indent=2, sort_keys=True)
        cov = f"{baseline['_parsed']}/{baseline['_corpus_n']}"
        print(f"corpus coverage: {cov} parsed ({baseline['_failed']} failed)")
        print(f"wrote {BASELINE_PATH}")
        for k in list(SCORED_METRICS):
            b = baseline['metrics'].get(k)
            if b: print(f"  {k}: p10={b['p10']} p25={b['p25']} p50={b['p50']} p75={b['p75']} p90={b['p90']}")
        return

    if args and args[0] == '--score':
        rest = args[1:]
        baseline_path = BASELINE_PATH
        if '--baseline' in rest:
            i = rest.index('--baseline'); baseline_path = rest[i+1]; rest = rest[:i] + rest[i+2:]
        if not os.path.exists(baseline_path):
            print(f"no baseline at {baseline_path}; run --percentiles first", file=sys.stderr)
            sys.exit(2)
        baseline = load_baseline(baseline_path)
        json_only = '--json' in rest
        rest = [a for a in rest if a != '--json']
        files = expand_files(rest)
        reps = []
        for fp in files:
            try:
                rep = score(fp, baseline)
                reps.append(rep)
                if not json_only: print_score(rep)
            except Exception as e:
                print(f"FAIL {fp}: {e}", file=sys.stderr)
        if json_only:
            print(json.dumps([{k: r[k] for k in ('file', 'human_likeness', 'metrics', 'size')}
                              for r in reps], indent=2))
        return

    agg = False
    if args and args[0] == '--agg':
        agg = True; args = args[1:]
    files = []
    for a in args: files.extend(glob.glob(a))
    rows = []
    for fp in files:
        try: rows.append(metrics(fp))
        except Exception as e: print(f"FAIL {fp}: {e}", file=sys.stderr)
    if agg:
        def col(k): return [r[k] for r in rows if r.get(k) is not None]
        keys = ['n_parts','wire_len_median','wire_len_max','wire_frac_gt50','crossings_per_wire','label_per_part','sprawl','cluster_ratio','min_gap']
        out = {'n_files': len(rows)}
        for k in keys:
            v = col(k)
            if v: out[k] = {'median': round(st.median(v),2), 'mean': round(st.mean(v),2)}
        print(json.dumps(out, indent=2))
    else:
        for r in rows: print(json.dumps(r))

if __name__ == '__main__':
    main()
