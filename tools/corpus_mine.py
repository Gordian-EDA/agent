#!/usr/bin/env python3
"""corpus_mine.py — mine STRUCTURAL layout laws from the human schematic corpus.

Where layout_metrics.py measures marginal statistics (sizes, counts), this tool
extracts the generative structure a layout engine can imitate: pin-level geometry,
part roles (in-line vs shunt vs decoupling), orientation conventions, label-stub
shape, power-symbol attachment, row banking, and per-net wire-vs-label policy.

Usage:
  tools/corpus_mine.py GLOB...            # aggregate report over matching .kicad_sch
  tools/corpus_mine.py --per-file GLOB    # per-file JSON rows
"""
import glob
import json
import math
import os
import sys
from collections import Counter, defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from layout_metrics import find_all, head, parse, tokenize  # noqa: E402

GRID = 1.27


def snap(v):
    return round(v / GRID) * GRID


def fnum(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def sattr(node, name, idx=1):
    for c in node:
        if isinstance(c, list) and head(c) == name:
            v = c[idx]
            return v[1:] if isinstance(v, str) and v.startswith('"') else v
    return None


def parse_lib_pins(root):
    """lib_id -> list of (px, py, rot, etype, unit)."""
    libs = {}
    for ls in find_all(root, 'lib_symbols'):
        for sym in ls:
            if head(sym) != 'symbol':
                continue
            name = sym[1][1:] if isinstance(sym[1], str) and sym[1].startswith('"') else sym[1]
            pins = []
            for sub in sym:
                if head(sub) != 'symbol':
                    continue
                subname = sub[1][1:] if isinstance(sub[1], str) and sub[1].startswith('"') else sub[1]
                parts = subname.rsplit('_', 2)
                unit = int(parts[1]) if len(parts) == 3 and parts[1].isdigit() else 0
                for pin in sub:
                    if head(pin) != 'pin':
                        continue
                    at = next((c for c in pin if isinstance(c, list) and head(c) == 'at'), None)
                    if not at:
                        continue
                    px, py = fnum(at[1]), fnum(at[2])
                    rot = fnum(at[3]) if len(at) > 3 else 0.0
                    etype = pin[1] if len(pin) > 1 and isinstance(pin[1], str) else 'passive'
                    pins.append((px, py, rot or 0.0, etype, unit))
            libs[name] = pins
    return libs


def instance_pins(inst, libs):
    """Absolute sheet coords of an instance's pins: (x, y, etype).

    KiCad convention (verified against corpus wire endpoints): pin offset (px,py)
    is Y-up in symbol space; instance placement applies rotation CCW then Y flip:
      no mirror : (x0 + c*px - s*py, y0 - (s*px + c*py))
      mirror x  : flip symbol-space Y before rotate
      mirror y  : flip symbol-space X before rotate
    """
    lib = sattr(inst, 'lib_id')
    at = next((c for c in inst if isinstance(c, list) and head(c) == 'at'), None)
    if lib not in libs or not at:
        return []
    x0, y0 = fnum(at[1]), fnum(at[2])
    theta = math.radians(fnum(at[3]) or 0.0) if len(at) > 3 else 0.0
    mirror = sattr(inst, 'mirror') or ''
    unit = fnum(sattr(inst, 'unit')) or 1
    c, s = math.cos(theta), math.sin(theta)
    out = []
    for px, py, _rot, etype, punit in libs[lib]:
        if punit not in (0, int(unit)):
            continue
        mx, my = px, py
        if 'x' in mirror:
            my = -my
        if 'y' in mirror:
            mx = -mx
        rx = c * mx - s * my
        ry = s * mx + c * my
        out.append((x0 + rx, y0 - ry, etype))
    return out


def wire_segments(children):
    segs = []
    for w in children:
        if head(w) != 'wire':
            continue
        pts = next((x for x in w if isinstance(x, list) and head(x) == 'pts'), None)
        if not pts:
            continue
        xy = [(fnum(p[1]), fnum(p[2])) for p in pts if head(p) == 'xy']
        segs.extend((a, b) for a, b in zip(xy, xy[1:]) if a != b)
    return segs


class UF:
    def __init__(self):
        self.p = {}

    def find(self, x):
        self.p.setdefault(x, x)
        while self.p[x] != x:
            self.p[x] = self.p[self.p[x]]
            x = self.p[x]
        return x

    def union(self, a, b):
        self.p[self.find(a)] = self.find(b)


def key(pt):
    return (round(pt[0] * 100), round(pt[1] * 100))


def build_conn(segs, labels, pins_flat, power_pts):
    """Union-find connectivity over wire endpoints, pins-on-wires, labels, power pins.

    Approximation: pins/labels/power connect only at segment ENDPOINTS or interior
    collinear points of a segment (T-joins). Good enough for corpus statistics.
    """
    uf = UF()
    pt_on = defaultdict(list)
    for i, (a, b) in enumerate(segs):
        uf.union(('s', i), key(a))
        uf.union(('s', i), key(b))
        pt_on[key(a)].append(i)
        pt_on[key(b)].append(i)

    def attach(pt):
        k = key(pt)
        if k in uf.p:
            return k
        for i, (a, b) in enumerate(segs):
            (x0, y0), (x1, y1) = a, b
            if x0 == x1 and abs(pt[0] - x0) < .01 and min(y0, y1) - .01 <= pt[1] <= max(y0, y1) + .01:
                uf.union(k, ('s', i))
                return k
            if y0 == y1 and abs(pt[1] - y0) < .01 and min(x0, x1) - .01 <= pt[0] <= max(x0, x1) + .01:
                uf.union(k, ('s', i))
                return k
        return k

    name_root = {}
    for (lx, ly, text) in labels:
        k = attach((lx, ly))
        if text in name_root:
            uf.union(k, name_root[text])
        else:
            name_root[text] = k
    for pt in power_pts:
        attach(pt)
    for (x, y, _e, _ref) in pins_flat:
        attach((x, y))
    return uf


def mine_file(path):
    src = open(path, errors='replace').read()
    tree = parse(tokenize(src))
    root = tree[0] if tree else []
    children = [c for c in root if isinstance(c, list)]
    libs = parse_lib_pins(root)

    segs = wire_segments(children)
    seg_ends = set()
    for a, b in segs:
        seg_ends.add(key(a))
        seg_ends.add(key(b))

    labels = []
    for c in children:
        if head(c) in ('label', 'global_label', 'hierarchical_label'):
            at = next((x for x in c if isinstance(x, list) and head(x) == 'at'), None)
            text = c[1][1:] if isinstance(c[1], str) and c[1].startswith('"') else str(c[1])
            if at:
                labels.append((fnum(at[1]), fnum(at[2]), text))

    parts, power_syms = [], []
    for c in children:
        if head(c) != 'symbol':
            continue
        lib = sattr(c, 'lib_id') or ''
        pins = instance_pins(c, libs)
        at = next((x for x in c if isinstance(x, list) and head(x) == 'at'), None)
        rot = (fnum(at[3]) if at and len(at) > 3 else 0.0) or 0.0
        ref = ''
        for pr in c:
            if head(pr) == 'property' and pr[1] == '"Reference':
                ref = pr[2][1:] if isinstance(pr[2], str) else ''
        entry = {'lib': lib, 'pins': pins, 'rot': rot, 'ref': ref,
                 'at': (fnum(at[1]), fnum(at[2])) if at else None}
        if lib.startswith('power:'):
            power_syms.append(entry)
        else:
            parts.append(entry)

    # --- transform sanity: pins should land on wire endpoints or label/power spots
    hit = tot = 0
    for p in parts:
        for (x, y, _e) in p['pins']:
            tot += 1
            hit += key((x, y)) in seg_ends
    res = {'file': os.path.basename(path), 'pin_endpoint_hit': round(hit / tot, 3) if tot else None}

    pins_flat = [(x, y, e, p['ref']) for p in parts for (x, y, e) in p['pins']]
    power_pts = [(x, y) for ps in power_syms for (x, y, _e) in ps['pins']]
    uf = build_conn(segs, labels, pins_flat, power_pts)

    gnd_roots, vcc_roots = set(), set()
    for ps in power_syms:
        nm = ps['lib'].split(':', 1)[-1].upper()
        for (x, y, _e) in ps['pins']:
            r = uf.find(key((x, y)))
            (gnd_roots if ('GND' in nm or 'EARTH' in nm) else vcc_roots).add(r)

    # --- 2-pin part roles & orientation
    role_orient = Counter()
    for p in parts:
        if len(p['pins']) != 2:
            continue
        (x1, y1, _), (x2, y2, _) = p['pins']
        if abs(x1 - x2) < .01:
            orient = 'V'
        elif abs(y1 - y2) < .01:
            orient = 'H'
        else:
            continue
        r1, r2 = uf.find(key((x1, y1))), uf.find(key((x2, y2)))
        to_gnd = r1 in gnd_roots or r2 in gnd_roots
        to_vcc = r1 in vcc_roots or r2 in vcc_roots
        if to_gnd and not to_vcc:
            role = 'shunt_gnd'
        elif to_vcc and to_gnd:
            role = 'decouple'
        elif to_vcc:
            role = 'pull_vcc'
        else:
            role = 'series'
        role_orient[(role, orient)] += 1
    res['role_orient'] = {f'{r}_{o}': n for (r, o), n in role_orient.items()}

    # --- label stubs: manhattan distance label -> nearest same-net pin
    stub = []
    for (lx, ly, _t) in labels:
        lr = uf.find(key((lx, ly)))
        ds = [abs(lx - x) + abs(ly - y) for (x, y, _e, _ref) in pins_flat
              if uf.find(key((x, y))) == lr]
        if ds:
            stub.append(min(ds))
    if stub:
        stub.sort()
        res['stub_median'] = round(stub[len(stub) // 2], 2)
        res['stub_p90'] = round(stub[int(.9 * (len(stub) - 1))], 2)

    # --- power symbol -> nearest pin distance
    pd = []
    for ps in power_syms:
        for (x, y, _e) in ps['pins']:
            ds = [abs(x - px) + abs(y - py) for (px, py, _e2, _r) in pins_flat]
            if ds:
                pd.append(min(ds))
    if pd:
        pd.sort()
        res['powerpin_median'] = round(pd[len(pd) // 2], 2)

    # --- IC rotation convention (>=4 pins)
    ic_rot = Counter(int(p['rot']) % 360 for p in parts if len(p['pins']) >= 4)
    res['ic_rot'] = dict(ic_rot)

    # --- grid quantization of part origins
    on_grid = sum(1 for p in parts if p['at'] and
                  abs(p['at'][0] - snap(p['at'][0])) < .01 and
                  abs(p['at'][1] - snap(p['at'][1])) < .01)
    res['grid_frac'] = round(on_grid / len(parts), 3) if parts else None

    # --- net-level wire-vs-label policy: per net root, pin count + spatial span
    net_pins = defaultdict(list)
    for (x, y, _e, _ref) in pins_flat:
        net_pins[uf.find(key((x, y)))].append((x, y))
    labeled_roots = {uf.find(key((lx, ly))) for (lx, ly, _t) in labels}
    pol = {'wired': [], 'labeled': []}
    for r, pts in net_pins.items():
        if len(pts) < 2 or r in gnd_roots or r in vcc_roots:
            continue
        xs = [p[0] for p in pts]
        ys = [p[1] for p in pts]
        span = (max(xs) - min(xs)) + (max(ys) - min(ys))
        pol['labeled' if r in labeled_roots else 'wired'].append((len(pts), round(span, 1)))
    res['nets_wired'] = len(pol['wired'])
    res['nets_labeled'] = len(pol['labeled'])
    if pol['wired']:
        spans = sorted(s for _n, s in pol['wired'])
        res['wired_span_p90'] = spans[int(.9 * (len(spans) - 1))]
    if pol['labeled']:
        spans = sorted(s for _n, s in pol['labeled'])
        res['labeled_span_p10'] = spans[int(.1 * (len(spans) - 1))]

    # --- straightness: wire-drawn 2-pin nets sharing an axis (labels excluded:
    # a labeled pair was never drawn, so its geometry says nothing about wiring)
    straight = bent = 0
    for r, pts in net_pins.items():
        if len(pts) == 2 and r not in gnd_roots and r not in vcc_roots and r not in labeled_roots:
            (x1, y1), (x2, y2) = pts
            if abs(x1 - x2) < .01 or abs(y1 - y2) < .01:
                straight += 1
            else:
                bent += 1
    if straight + bent:
        res['p2p_straight_frac'] = round(straight / (straight + bent), 3)

    # --- row banking: passives sharing exact Y (or X) with another passive
    ys = Counter()
    for p in parts:
        if len(p['pins']) == 2 and p['at']:
            ys[round(p['at'][1], 2)] += 1
    if ys:
        banked = sum(n for n in ys.values() if n >= 2)
        res['passive_y_banked_frac'] = round(banked / sum(ys.values()), 3)

    return res


def aggregate(rows):
    agg = {}
    num_keys = ['pin_endpoint_hit', 'stub_median', 'stub_p90', 'powerpin_median',
                'grid_frac', 'p2p_straight_frac', 'passive_y_banked_frac',
                'wired_span_p90', 'labeled_span_p10']
    for k in num_keys:
        vals = sorted(r[k] for r in rows if r.get(k) is not None)
        if vals:
            agg[k] = {'n': len(vals),
                      'p25': vals[len(vals) // 4],
                      'p50': vals[len(vals) // 2],
                      'p75': vals[3 * len(vals) // 4]}
    ro = Counter()
    for r in rows:
        ro.update(r.get('role_orient', {}))
    agg['role_orient_total'] = dict(ro.most_common())
    rot = Counter()
    for r in rows:
        rot.update({int(k): v for k, v in r.get('ic_rot', {}).items()})
    agg['ic_rot_total'] = dict(rot.most_common())
    agg['nets_wired_total'] = sum(r.get('nets_wired', 0) for r in rows)
    agg['nets_labeled_total'] = sum(r.get('nets_labeled', 0) for r in rows)
    return agg


def main():
    args = sys.argv[1:]
    per_file = '--per-file' in args
    args = [a for a in args if a != '--per-file']
    files = []
    for a in args:
        files.extend(sorted(glob.glob(a)))
    if not files:
        sys.exit('no files matched')
    rows, fails = [], 0
    for fp in files:
        try:
            rows.append(mine_file(fp))
        except Exception as e:
            fails += 1
            print(f'FAIL {fp}: {e}', file=sys.stderr)
    if per_file:
        for r in rows:
            print(json.dumps(r))
    print(json.dumps({'n': len(rows), 'fails': fails, **aggregate(rows)}, indent=2))


if __name__ == '__main__':
    main()
