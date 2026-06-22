#!/usr/bin/env python3
"""Deterministic schematic-layout metrics from a .kicad_sch — an OBJECTIVE, low-noise
complement to the VLM critic. Computes the mined human-layout proxies so "ours vs human"
is measurable without eyeballing or critic variance.

Usage:
  python3 tools/layout_metrics.py FILE.kicad_sch [FILE2 ...]            # per-file JSON
  python3 tools/layout_metrics.py --agg ~/kicad-scraper/dataset/*.kicad_sch   # aggregate stats

Metrics (per sheet):
  n_parts            component instances (excludes power symbols / lib defs)
  n_wires, wire_len_median/p90/max, wire_frac_gt50   # label-vs-wire rule (humans ~0% >50mm)
  n_labels, n_power_syms, label_per_part
  bbox_w/h, sprawl   # bbox_area / sum(part_bbox_area): 1=perfectly packed, higher=more whitespace
  n_clusters, cluster_ratio (clusters/parts), min_gap   # block-disjointness (single-linkage @25.4mm)
"""
import sys, re, json, math, glob, statistics as st

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
    # wires (top-level)
    wlens = []
    for w in children:
        if head(w) != 'wire': continue
        pts = next((x for x in w if isinstance(x, list) and head(x) == 'pts'), None)
        if not pts: continue
        xy = [(float(p[1]), float(p[2])) for p in pts if head(p) == 'xy']
        for a, b in zip(xy, xy[1:]):
            wlens.append(abs(a[0]-b[0]) + abs(a[1]-b[1]))
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

def main():
    args = sys.argv[1:]
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
        keys = ['n_parts','wire_len_median','wire_len_max','wire_frac_gt50','label_per_part','sprawl','cluster_ratio','min_gap']
        out = {'n_files': len(rows)}
        for k in keys:
            v = col(k)
            if v: out[k] = {'median': round(st.median(v),2), 'mean': round(st.mean(v),2)}
        print(json.dumps(out, indent=2))
    else:
        for r in rows: print(json.dumps(r))

if __name__ == '__main__':
    main()
