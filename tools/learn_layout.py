#!/usr/bin/env python3
"""
learn_layout.py — learn what distinguishes HUMAN schematic layouts from the engine's.

For each board id, compute layout features (via layout_metrics.metrics) on the human
.kicad_sch and on the engine's re-layout, label human=1 / machine=0, and fit a
standardized logistic regression (pure Python, no numpy). The standardized coefficients
rank which features most separate human from machine — i.e. the engine's measurable
deficiencies — and the model is a cheap learned "human-likeness" score the engine could
gate placement moves on (rewarding the banking/grouping a hand-tuned proxy can't see).

Usage:
  tools/learn_layout.py --human DIR --machine DIR --ids FILE
    DIR/<id>.kicad_sch ; ids one per line.
"""
import argparse
import math
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from layout_metrics import metrics  # noqa: E402

# Features that describe layout QUALITY (not raw size), present for most boards.
FEATS = [
    "sprawl",
    "label_per_part",
    "crossings_per_wire",
    "wire_len_median",
    "wire_len_p90",
    "wire_frac_gt50",
    "cluster_ratio",  # n_clusters / n_parts — islands
]


def feats(path):
    m = metrics(path)
    if "n_clusters" in m and m.get("n_parts"):
        m["cluster_ratio"] = m["n_clusters"] / m["n_parts"]
    return [m.get(f) for f in FEATS]


def standardize(rows):
    n = len(rows[0])
    mean = [sum(r[j] for r in rows) / len(rows) for j in range(n)]
    var = [sum((r[j] - mean[j]) ** 2 for r in rows) / len(rows) for j in range(n)]
    std = [math.sqrt(v) or 1.0 for v in var]
    z = [[(r[j] - mean[j]) / std[j] for j in range(n)] for r in rows]
    return z, mean, std


def fit(X, y, iters=4000, lr=0.2, l2=0.01):
    n, d = len(X), len(X[0])
    w = [0.0] * d
    b = 0.0
    for _ in range(iters):
        gw = [0.0] * d
        gb = 0.0
        for xi, yi in zip(X, y):
            z = b + sum(w[j] * xi[j] for j in range(d))
            p = 1.0 / (1.0 + math.exp(-max(-30, min(30, z))))
            e = p - yi
            for j in range(d):
                gw[j] += e * xi[j]
            gb += e
        for j in range(d):
            w[j] -= lr * (gw[j] / n + l2 * w[j])
        b -= lr * gb / n
    return w, b


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--human", required=True)
    ap.add_argument("--machine", required=True)
    ap.add_argument("--ids", required=True)
    a = ap.parse_args()
    ids = [l.strip() for l in open(a.ids) if l.strip()]

    rows, labels, used = [], [], []
    for i in ids:
        hp = os.path.join(a.human, f"{i}.kicad_sch")
        mp = os.path.join(a.machine, f"{i}.kicad_sch")
        if not (os.path.exists(hp) and os.path.exists(mp)):
            continue
        try:
            hf, mf = feats(hp), feats(mp)
        except Exception:
            continue
        if any(v is None for v in hf) or any(v is None for v in mf):
            continue
        rows.append(hf); labels.append(1); used.append(i)
        rows.append(mf); labels.append(0)
    if len(rows) < 8:
        sys.exit(f"too few usable boards ({len(rows)//2})")
    print(f"boards used: {len(used)}  (human+machine rows: {len(rows)})")

    Z, mean, std = standardize(rows)
    w, b = fit(Z, labels)

    # Train accuracy.
    correct = 0
    for xi, yi in zip(Z, labels):
        z = b + sum(w[j] * xi[j] for j in range(len(w)))
        correct += (1 if z > 0 else 0) == yi
    print(f"train accuracy: {correct/len(labels):.0%}  (human=1 vs machine=0)\n")

    print("standardized weights (sign: + => higher value is more HUMAN-like):")
    order = sorted(range(len(FEATS)), key=lambda j: -abs(w[j]))
    for j in order:
        hm = sum(rows[k][j] for k in range(0, len(rows), 2)) / (len(rows) // 2)
        mm = sum(rows[k][j] for k in range(1, len(rows), 2)) / (len(rows) // 2)
        print(f"  {FEATS[j]:<20} w={w[j]:+6.2f}   human_mean={hm:8.3f}  machine_mean={mm:8.3f}")


if __name__ == "__main__":
    main()
