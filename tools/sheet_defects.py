#!/usr/bin/env python3
"""Deterministic counts of four drawing defects on emitted .kicad_sch sheets.

Per sheet: net labels printed on a same-named power symbol's pin, blocks whose
sibling connectors disagree on mirror, junction dots, and collinear wire splits
at a pin with no third conductor.
"""
import sys, os, re, json, math
from collections import defaultdict


def parse(text):
    toks = re.findall(r'\(|\)|"(?:[^"\\]|\\.)*"|[^\s()]+', text)
    stack, cur = [], []
    for t in toks:
        if t == '(':
            stack.append(cur); cur = []
        elif t == ')':
            done = cur; cur = stack.pop(); cur.append(done)
        elif t.startswith('"'):
            cur.append(('s', t[1:-1].replace('\\"', '"').replace('\\\\', '\\')))
        else:
            cur.append(('a', t))
    return cur


def head(n):
    return n[0][1] if n and isinstance(n[0], tuple) else None


def kids(n, name):
    return [c for c in n if isinstance(c, list) and head(c) == name]


def kid(n, name):
    k = kids(n, name)
    return k[0] if k else None


def atoms(n):
    return [v for k, v in (c for c in n if isinstance(c, tuple))][1:]


def num(s):
    return round(float(s) * 1000)


def prop(sym, key):
    for p in kids(sym, 'property'):
        a = atoms(p)
        if a and a[0] == key:
            return a[1] if len(a) > 1 else ''
    return None


GND_ALIASES = {'GND', 'GNDA', 'GNDD', 'GNDPWR', 'AGND', 'DGND', 'VSS', 'EARTH'}


def norm_net(n):
    return n.lstrip('~').lstrip('+') if n else n


def load(path):
    root = parse(open(path).read())[0]
    syms, wires, juncs, labels = [], [], [], []
    for s in kids(root, 'symbol'):
        lib = atoms(kid(s, 'lib_id'))[0]
        at = atoms(kid(s, 'at'))
        mir = kid(s, 'mirror')
        syms.append(dict(
            lib=lib, x=num(at[0]), y=num(at[1]), ang=float(at[2]) if len(at) > 2 else 0.0,
            mirror=atoms(mir)[0] if mir else None,
            ref=prop(s, 'Reference'), val=prop(s, 'Value'), block=prop(s, 'ap_block')))
    for w in kids(root, 'wire'):
        p = kid(w, 'pts')
        xy = [atoms(c) for c in kids(p, 'xy')]
        wires.append(((num(xy[0][0]), num(xy[0][1])), (num(xy[1][0]), num(xy[1][1]))))
    for j in kids(root, 'junction'):
        a = atoms(kid(j, 'at'))
        juncs.append((num(a[0]), num(a[1])))
    for tag in ('label', 'global_label', 'hierarchical_label'):
        for l in kids(root, tag):
            a = atoms(kid(l, 'at'))
            labels.append(dict(text=atoms(l)[0], x=num(a[0]), y=num(a[1]), kind=tag))
    return syms, wires, juncs, labels


def power_pins(syms):
    """A power symbol's single pin sits at the symbol origin."""
    out = {}
    for s in syms:
        if s['lib'].startswith('power:') and s['lib'] != 'power:PWR_FLAG':
            out[(s['x'], s['y'])] = s['val']
    return out


def label_on_power(syms, labels):
    pp = power_pins(syms)
    hits = []
    for l in labels:
        net = pp.get((l['x'], l['y']))
        if net is None:
            continue
        if norm_net(net) == norm_net(l['text']) or (
                net in GND_ALIASES and l['text'] in GND_ALIASES):
            hits.append((l['text'], l['x'] / 1000, l['y'] / 1000))
    return hits


def mixed_mirror_blocks(syms):
    groups = defaultdict(set)
    for s in syms:
        if not s['block'] or not s['lib'].split(':')[-1].startswith('Conn'):
            continue
        groups[(s['block'], s['lib'])].add((s['mirror'], s['ang'] % 360))
    return sorted(k for k, v in groups.items() if len(v) > 1)


def collinear_splits(syms, wires, juncs):
    """Endpoints where exactly two collinear wire ends meet, no other conductor."""
    ends = defaultdict(list)
    for a, b in wires:
        ends[a].append(b)
        ends[b].append(a)
    pins = set(power_pins(syms))
    hits = []
    for p, others in ends.items():
        if len(others) != 2:
            continue
        (ax, ay), (bx, by) = others
        d1 = (ax - p[0], ay - p[1])
        d2 = (bx - p[0], by - p[1])
        if d1[0] * d2[1] - d1[1] * d2[0] != 0:
            continue
        if d1[0] * d2[0] + d1[1] * d2[1] >= 0:  # doubling back, not a straight run
            continue
        hits.append((p[0] / 1000, p[1] / 1000, p in pins))
    return hits


def measure(path):
    syms, wires, juncs, labels = load(path)
    return dict(
        label_on_power=label_on_power(syms, labels),
        mixed_mirror=mixed_mirror_blocks(syms),
        junctions=len(juncs),
        collinear=collinear_splits(syms, wires, juncs),
    )


def main():
    dirs = sys.argv[1:]
    reports = []
    for d in dirs:
        r = {}
        for f in sorted(os.listdir(d)):
            if f.endswith('.kicad_sch'):
                r[f[:-10]] = measure(os.path.join(d, f))
        reports.append(r)
    names = sorted(set().union(*[set(r) for r in reports]))
    cols = ['label_on_power', 'mixed_mirror', 'junctions', 'collinear', 'collinear_at_pin']
    def val(m, c):
        if c == 'collinear_at_pin':
            return sum(1 for h in m['collinear'] if h[2])
        v = m[c]
        return v if isinstance(v, int) else len(v)
    print(f"{'fixture':32} " + ' '.join(f'{c:>18}' for c in cols))
    tot = [[0] * len(cols) for _ in reports]
    for n in names:
        cells = []
        for c in cols:
            vs = [val(r[n], c) if n in r else 0 for r in reports]
            for i, v in enumerate(vs):
                tot[i][cols.index(c)] += v
            cells.append('->'.join(str(v) for v in vs))
        print(f'{n:32} ' + ' '.join(f'{c:>18}' for c in cells))
    print(f"{'TOTAL':32} " + ' '.join(
        f"{'->'.join(str(tot[i][j]) for i in range(len(reports))):>18}"
        for j in range(len(cols))))
    if os.environ.get('DETAIL'):
        for n in names:
            m = reports[-1][n]
            if m['label_on_power'] or m['mixed_mirror'] or any(h[2] for h in m['collinear']):
                print(n, json.dumps({k: v for k, v in m.items() if k != 'junctions'}, default=str))


if __name__ == '__main__':
    main()
