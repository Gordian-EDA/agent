#!/usr/bin/env python3
"""Inject a KNOWN functional defect into a circuit-YAML netlist, for ground-truth recall testing
of the design reviewer (tools/design_critic.py). We choose the defect, so we know exactly what a
correct reviewer must report — breaking the LLM-grading-LLM circularity. Stdlib only (no pyyaml):
the fixtures use flow-style `pins: {k: net, ...}`, so targeted text mutation is reliable.

Defect types:
  pinswap     swap the nets on two distinct signal pins of an IC (a realistic mis-wire)
  value       multiply a passive's value by 100 (order-of-magnitude error)
  disconnect  set a signal pin to `nc` (a missing connection)

  python3 tools/inject_defect.py IN.yaml pinswap OUT.yaml [--seed N]
prints JSON {refdes, type, detail} describing the injected defect (the ground truth).
"""
import argparse, json, re, sys

# True power/ground rails only — NOT signal nets like VOUT/VIN (which require a digit beside the V
# to count, e.g. 3V3/+5V). Over-matching here starves the injector of signal-pin targets.
POWERISH = re.compile(r"^(GND\w*|AGND|DGND|VSS\w*|VDD\w*|VCC\w*|VDDA|VEE|VREF\w*|VBUS|VBAT|\+?\d+V\d*|\+?\d*V\d+|nc)$", re.I)
# Common decoupling/bulk cap values — a 100x change here is not a real defect (10uF decoupling is
# plausible), so skip them as `value` targets and hit FUNCTIONAL passives (dividers, load/timing).
DECOUPLE_VALS = {"100nf", "0.1uf", "1uf", "10uf", "4.7uf", "2.2uf", "22uf", "47uf", "100uf",
                 "100n", "0.1u", "1u", "10u", "4.7u", "2.2u", "22u", "47u", "100u"}
REFDES = re.compile(r"^\s*([A-Za-z]+\d+):")
PINS = re.compile(r"pins:\s*\{([^}]*)\}")
VALUE = re.compile(r"value:\s*([0-9][0-9.]*)([A-Za-z%]*)")


def unq(s):
    return s.strip().strip("'").strip('"')


def is_power_or_nc(net):
    return bool(POWERISH.match(unq(net)))


def parse_pairs(inner):
    pairs = []
    for part in inner.split(","):
        if ":" in part:
            k, v = part.split(":", 1)
            pairs.append([k.strip(), v.strip()])
    return pairs


def fmt_pairs(pairs):
    return "{" + ", ".join(f"{k}: {v}" for k, v in pairs) + "}"


def find_pin_comps(lines):
    """[(refdes, line_idx, inner_str)] for every flow-style pins map, with its owning refdes."""
    refdes, out = None, []
    for i, line in enumerate(lines):
        m = REFDES.match(line)
        if m:
            refdes = m.group(1)
        pm = PINS.search(line)
        if pm and refdes:
            out.append((refdes, i, pm.group(1)))
    return out


def find_value_comps(lines):
    refdes, out = None, []
    for i, line in enumerate(lines):
        m = REFDES.match(line)
        if m:
            refdes = m.group(1)
        vm = VALUE.search(line)
        if vm and refdes:
            out.append((refdes, i, vm.group(1), vm.group(2)))
    return out


def inject_pinswap(lines, seed):
    cands = []
    for refdes, idx, inner in find_pin_comps(lines):
        pairs = parse_pairs(inner)
        sig = [(k, v) for k, v in pairs if not is_power_or_nc(v)]
        distinct = {unq(v): k for k, v in sig}
        if len(pairs) >= 4 and len(distinct) >= 2:
            cands.append((refdes, idx, pairs, sig))
    if not cands:
        return None
    refdes, idx, pairs, sig = cands[seed % len(cands)]
    seen, chosen = {}, []
    for k, v in sig:
        if unq(v) not in seen:
            seen[unq(v)] = k
            chosen.append(k)
        if len(chosen) == 2:
            break
    k1, k2 = chosen
    d = dict((p[0], i) for i, p in enumerate(pairs))
    pairs[d[k1]][1], pairs[d[k2]][1] = pairs[d[k2]][1], pairs[d[k1]][1]
    lines[idx] = PINS.sub("pins: " + fmt_pairs(pairs), lines[idx], count=1)
    return {"refdes": refdes, "type": "pinswap",
            "detail": f"swapped {refdes} pins {k1} and {k2} (their net assignments are exchanged)"}


def inject_value(lines, seed):
    cands = [c for c in find_value_comps(lines)
             if f"{c[2]}{c[3]}".lower().replace(" ", "") not in DECOUPLE_VALS]
    if not cands:
        return None
    refdes, idx, num, suffix = cands[seed % len(cands)]
    n = float(num) * 100
    newval = (f"{int(n)}" if n.is_integer() else f"{n}") + suffix
    lines[idx] = VALUE.sub(f"value: {newval}", lines[idx], count=1)
    return {"refdes": refdes, "type": "value",
            "detail": f"{refdes} value changed {num}{suffix} -> {newval} (100x, order-of-magnitude wrong)"}


def inject_disconnect(lines, seed):
    cands = []
    for refdes, idx, inner in find_pin_comps(lines):
        pairs = parse_pairs(inner)
        sig = [k for k, v in pairs if not is_power_or_nc(v)]
        if sig:
            cands.append((refdes, idx, pairs, sig))
    if not cands:
        return None
    refdes, idx, pairs, sig = cands[seed % len(cands)]
    k = sig[seed % len(sig)]
    old = next(v for kk, v in pairs if kk == k)
    for p in pairs:
        if p[0] == k:
            p[1] = "nc"
    lines[idx] = PINS.sub("pins: " + fmt_pairs(pairs), lines[idx], count=1)
    return {"refdes": refdes, "type": "disconnect",
            "detail": f"{refdes} pin {k} disconnected (was net {old}, now nc)"}


def inject_railswap(lines, seed):
    """Swap two POWER pins of an IC (e.g. VCC↔GND, or 3V3↔5V) — a power-domain / reversed-supply fault."""
    cands = []
    for refdes, idx, inner in find_pin_comps(lines):
        pairs = parse_pairs(inner)
        pwr = [(k, v) for k, v in pairs if is_power_or_nc(v) and unq(v).lower() != "nc"]
        distinct = {unq(v): k for k, v in pwr}
        if len(pairs) >= 4 and len(distinct) >= 2:
            cands.append((refdes, idx, pairs, pwr))
    if not cands:
        return None
    refdes, idx, pairs, pwr = cands[seed % len(cands)]
    seen, chosen = {}, []
    for k, v in pwr:
        if unq(v) not in seen:
            seen[unq(v)] = k
            chosen.append(k)
        if len(chosen) == 2:
            break
    k1, k2 = chosen
    d = {p[0]: i for i, p in enumerate(pairs)}
    pairs[d[k1]][1], pairs[d[k2]][1] = pairs[d[k2]][1], pairs[d[k1]][1]
    lines[idx] = PINS.sub("pins: " + fmt_pairs(pairs), lines[idx], count=1)
    return {"refdes": refdes, "type": "railswap",
            "detail": f"swapped {refdes} power pins {k1} and {k2} — its supply rails are exchanged (e.g. VCC/GND reversed)"}


def inject_reverse(lines, seed):
    """Reverse a polarized part (diode/LED/electrolytic): swap its two terminals — a backwards-part fault."""
    cands = []
    for i, line in enumerate(lines):
        rd = REFDES.match(line)
        if rd and "positive:" in line and "negative:" in line:
            cands.append(("pn", rd.group(1), i))
    for refdes, idx, inner in find_pin_comps(lines):
        if refdes[0] in "Dd" and refdes[1:].isdigit():
            pairs = parse_pairs(inner)
            if len(pairs) == 2 and all(unq(v).lower() != "nc" for _, v in pairs):
                cands.append(("pins", refdes, idx))
    if not cands:
        return None
    kind, refdes, idx = cands[seed % len(cands)]
    if kind == "pn":
        line = lines[idx]
        pos = re.search(r"positive:\s*([^,}\s]+)", line).group(1)
        neg = re.search(r"negative:\s*([^,}\s]+)", line).group(1)
        line = re.sub(r"positive:\s*[^,}\s]+", f"positive: {neg}", line, count=1)
        line = re.sub(r"negative:\s*[^,}\s]+", f"negative: {pos}", line, count=1)
        lines[idx] = line
    else:
        inner = PINS.search(lines[idx]).group(1)
        pairs = parse_pairs(inner)
        pairs[0][1], pairs[1][1] = pairs[1][1], pairs[0][1]
        lines[idx] = PINS.sub("pins: " + fmt_pairs(pairs), lines[idx], count=1)
    return {"refdes": refdes, "type": "reverse",
            "detail": f"reversed {refdes} polarity — its two terminals are swapped (part installed backwards)"}


INJECTORS = {
    "pinswap": inject_pinswap,
    "value": inject_value,
    "disconnect": inject_disconnect,
    "railswap": inject_railswap,
    "reverse": inject_reverse,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("input")
    ap.add_argument("type", choices=list(INJECTORS))
    ap.add_argument("output")
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()

    lines = open(args.input).read().split("\n")
    info = INJECTORS[args.type](lines, args.seed)
    if info is None:
        print(json.dumps({"error": f"no target for {args.type}"}))
        sys.exit(2)
    with open(args.output, "w") as f:
        f.write("\n".join(lines))
    print(json.dumps(info))


if __name__ == "__main__":
    main()
