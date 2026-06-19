#!/usr/bin/env python3
"""Summarise a circuit YAML as a MODULE GRAPH for the VLM floorplanner's surface.

A vision LLM reads connectivity poorly off a cluttered render (it mis-grouped c19's inputs).
Giving it the module graph explicitly — the major parts, their roles, and what connects to
what — is the "better surface" (the structured representation LLM design tools hand the model
alongside the picture). Major-to-major links are traced THROUGH series passives (a coupling
cap / series resistor bridges two signal nets), so e.g. J1 →C1→ U1 reads as J1↔U1.

    python3 tools/circuit_summary.py IN.yaml      # prints the text summary

No YAML lib: the corpus uses one-line `REFDES: {part: .., value: .., pins: {n: NET, ..}}`.
"""
import re
import sys

POWER_RE = re.compile(r"^(GND|AGND|DGND|PGND|VBUS|VCC|VDD|VSS|VEE|VBAT|VPP)$|^[+-]?\d*\.?\d+V", re.I)


def is_power(net):
    return bool(POWER_RE.match(net)) or "GND" in net.upper()


def parse(path):
    parts = {}
    line_re = re.compile(r"^\s+(\w+):\s*\{(.*)\}\s*$")
    for ln in open(path):
        m = line_re.match(ln)
        if not m:
            continue
        ref, body = m.group(1), m.group(2)
        pm = re.search(r"part:\s*([^,}]+)", body)
        vm = re.search(r"value:\s*([^,}]+)", body)
        pins = re.findall(r"\d+:\s*([A-Za-z0-9_.+-]+)", re.search(r"pins:\s*\{([^}]*)\}", body).group(1)) \
            if "pins:" in body else []
        parts[ref] = {"part": (pm.group(1).strip() if pm else ""),
                      "value": (vm.group(1).strip() if vm else ""),
                      "nets": pins}
    return parts


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


def summarize(path):
    parts = parse(path)
    # union signal nets bridged by a 2-pin series passive (coupling cap / series R / L)
    uf = UF()
    for ref, d in parts.items():
        is_passive = ref[0] in "CRL" and len(d["nets"]) == 2
        sig = [n for n in d["nets"] if not is_power(n)]
        if is_passive and len(sig) == 2:
            uf.union(sig[0], sig[1])
    # major parts: ICs/connectors (U*/J*) or anything with >=3 pins
    major = {r: d for r, d in parts.items() if r[0] in "UJ" or len(d["nets"]) >= 3}
    # net-group -> major parts on it (signal only)
    groups = {}
    for r, d in major.items():
        for n in d["nets"]:
            if not is_power(n):
                groups.setdefault(uf.find(n), set()).add(r)
    links = {}
    for members in groups.values():
        ms = sorted(members)
        for i in range(len(ms)):
            for j in range(i + 1, len(ms)):
                links[(ms[i], ms[j])] = links.get((ms[i], ms[j]), 0) + 1
    lines = ["Major parts (refdes = role):"]
    for r in sorted(major):
        d = major[r]
        ic = d["part"].split(":")[-1]
        lines.append(f"  {r} = {d['value'] or ic}" + (f" ({ic})" if d["value"] else ""))
    lines.append("Signal connections (traced through series passives):")
    if links:
        for (a, b), n in sorted(links.items(), key=lambda kv: (-kv[1], kv[0])):
            lines.append(f"  {a} <-> {b}" + (f" ({n} nets)" if n > 1 else ""))
    else:
        lines.append("  (none)")
    powernly = [r for r in sorted(major) if all(is_power(n) for n in major[r]["nets"])]
    if powernly:
        lines.append("Power-only (no signal nets): " + ", ".join(powernly))
    return "\n".join(lines)


IN_KW = ("IN", "RX", "MISO", "SDI", "SENSE", "LINE", "AIN", "ADC", "MIC", "CLKIN", "USB")
OUT_KW = ("OUT", "TX", "MOSI", "SDO", "SPK", "SPEAK", "LED", "DISP", "MOT", "PWM", "DAC", "BUZ")
PWR_KW = ("PWR", "VIN", "VCC", "BATT", "SUPPLY", "5V", "3V3", "12V", "VBUS")


def auto_zones(path):
    """DETERMINISTIC coarse zones {refdes:[fx,fy]} from the module graph + connector role-names —
    no VLM. Power/inputs LEFT, IC(s) CENTRE, outputs RIGHT, bidir/bus near the IC; same-column
    parts spread over rows. Captures the LLM's 'rough direction' for free, so every board (incl.
    agent-generated) can get the soft-bias lift automatically."""
    import json as _json
    parts = parse(path)
    major = {r: d for r, d in parts.items() if r[0] in "UJ" or len(d["nets"]) >= 3}
    pow_only = {r for r in major if all(is_power(n) for n in major[r]["nets"])}

    def role_fx(r, d):
        if r[0] == "U":                                # ICs -> centre (connectors never count as ICs)
            return 0.5
        if r in pow_only:                              # power input -> far left
            return 0.12
        v = (d["value"] + " " + r).upper()
        if any(k in v for k in PWR_KW):
            return 0.12
        if any(k in v for k in IN_KW):
            return 0.18
        if any(k in v for k in OUT_KW):
            return 0.85
        return 0.6                                     # bus/bidir header -> just right of IC

    fx = {r: role_fx(r, d) for r, d in major.items()}
    zones = {}
    # spread parts sharing a column over rows so they don't stack
    from collections import defaultdict
    col = defaultdict(list)
    for r in sorted(major):
        col[round(fx[r], 2)].append(r)
    for c, refs in col.items():
        n = len(refs)
        for i, r in enumerate(refs):
            fy = 0.5 if n == 1 else 0.25 + 0.5 * i / (n - 1)
            zones[r] = [c, round(fy, 3)]
    return zones


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--zones":
        import json as _json
        print(_json.dumps(auto_zones(sys.argv[2])))
    elif len(sys.argv) == 2:
        print(summarize(sys.argv[1]))
    else:
        sys.exit("usage: circuit_summary.py IN.yaml  |  circuit_summary.py --zones IN.yaml")
