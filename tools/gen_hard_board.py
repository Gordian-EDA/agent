#!/usr/bin/env python3
"""Generate a large/hard PCB circuit spec (a fine-pitch IC + a big supporting
cast of decoupling caps, series resistors, and break-out headers) for the
board_harness stress tests. Writes a {bounds, parts:[...]} JSON the agent's
create_board accepts.

    python3 tools/gen_hard_board.py bga64   > crates/gordian-kicad/examples/pcb_circuits/bga64-stress.json
    python3 tools/gen_hard_board.py tqfp64  > .../tqfp64-stress.json
"""
import json
import sys
import string

R0402 = "Resistor_SMD:R_0402_1005Metric"
C0402 = "Capacitor_SMD:C_0402_1005Metric"
HDR = lambda n: f"Connector_PinHeader_2.54mm:PinHeader_1x{n:02d}_P2.54mm_Vertical"


def bga64_pads():
    # 8x8 grid, rows A-H, cols 1-8.
    return [f"{r}{c}" for r in string.ascii_uppercase[:8] for c in range(1, 9)]


def tqfp64_pads():
    return [str(i) for i in range(1, 65)]


def gen(kind):
    if kind == "bga64":
        ic_fp = "Package_BGA:BGA-64_9.0x9.0mm_Layout10x10_P0.8mm"
        pads = bga64_pads()
        bounds = {"min_x": 0.0, "max_x": 80.0, "min_y": 0.0, "max_y": 70.0}
    elif kind == "tqfp64":
        ic_fp = "Package_QFP:TQFP-64_14x14mm_P0.8mm"
        pads = tqfp64_pads()
        bounds = {"min_x": 0.0, "max_x": 90.0, "min_y": 0.0, "max_y": 80.0}
    else:
        sys.exit(f"unknown kind {kind}")

    # Net assignment: every 4th pad GND, every 4th+2 VCC, the rest signals.
    pad_nets, signals = {}, []
    for i, p in enumerate(pads):
        if i % 4 == 0:
            pad_nets[p] = "GND"
        elif i % 4 == 2:
            pad_nets[p] = "VCC"
        else:
            s = f"S{len(signals) + 1}"
            signals.append(s)
            pad_nets[p] = s

    parts = [{"reference": "U1", "footprint": ic_fp, "pad_nets": pad_nets}]

    # A fat decoupling bank: one cap per signal (a realistic dense bypass array),
    # all VCC↔GND.
    n_caps = max(24, len(signals))
    for i in range(n_caps):
        parts.append({"reference": f"C{i+1}", "footprint": C0402,
                      "pad_nets": {"1": "VCC", "2": "GND"}})

    # A series resistor on every signal (S → Sx_o), then header the outputs.
    routed_sigs = []
    for i, s in enumerate(signals):
        o = f"{s}_O"
        parts.append({"reference": f"R{i+1}", "footprint": R0402,
                      "pad_nets": {"1": s, "2": o}})
        routed_sigs.append(o)

    # Break out the resistor-terminated signals + power on 1x08 headers.
    rail = ["VCC", "GND"]
    to_break = rail + routed_sigs
    jn = 1
    for chunk_start in range(0, len(to_break), 8):
        chunk = to_break[chunk_start: chunk_start + 8]
        pn = {str(k + 1): net for k, net in enumerate(chunk)}
        parts.append({"reference": f"J{jn}", "footprint": HDR(len(chunk)), "pad_nets": pn})
        jn += 1

    return {
        "description": f"STRESS: {kind} fine-pitch IC + {n_caps} decoupling caps + "
                       f"{len(routed_sigs)} series resistors + {jn-1} break-out headers "
                       f"({len(parts)} parts total)",
        # Fine-pitch ICs need a 4-layer stackup to fan out inner pins.
        "rules": {"layers": 4},
        "bounds": bounds,
        "parts": parts,
    }


if __name__ == "__main__":
    kind = sys.argv[1] if len(sys.argv) > 1 else "bga64"
    print(json.dumps(gen(kind), indent=2))
