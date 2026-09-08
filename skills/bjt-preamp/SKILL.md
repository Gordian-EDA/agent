---
name: bjt-preamp
description: Common-emitter 2N3904 audio preamplifier with voltage-divider bias, emitter degeneration and bypass cap, input/output coupling caps, 9 V supply, and input/output headers.
triggers: ["bjt preamp", "2n3904", "common emitter", "common-emitter amplifier", "voltage divider bias", "emitter degeneration", "audio preamplifier", "transistor amplifier"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| J1 | `Connector:Conn_01x02_Pin` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | 9 V power in |
| J2 | `Connector:Conn_01x02_Pin` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | audio in |
| J3 | `Connector:Conn_01x02_Pin` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | audio out |
| Q1 | `Transistor_BJT:2N3904` | `Package_TO_SOT_THT:TO-92_Inline` | 2N3904 |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k base divider high side |
| R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 22k base divider low side |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 4.7k collector load |
| R4 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k emitter degeneration |
| C1 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1uF input coupling |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100uF emitter bypass |
| C3 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 10uF output coupling |
| C4 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF supply bypass |

Pin-key notes verified against the installed libraries: `Transistor_BJT:2N3904` pins are numbered
`1`=E, `2`=B, `3`=C (name keys `E`/`B`/`C` also work, but the numeric keys read directly off the
symbol pin table from `pins.py`). `Connector:Conn_01x02_Pin` pins are `1`=`Pin_1`, `2`=`Pin_2`;
number keys route to them directly. `Device:R` and `Device:C` are the plain `1`/`2` passives.

## Pin map

```
0V: J1.2, J2.2, R2.2, C2.2, C4.2, J3.2
BASE: C1.2, R1.2, R2.1, Q1.2
COL: Q1.3, R3.2, C3.1
EM: Q1.1, R4.1
E_RAW: R4.2, C2.1
IN: J2.1, C1.1
OUT: C3.2, J3.1
V9: J1.1, R1.1, R3.1, C4.1
```

## Layout

The complete design JSON below builds with 0 layout errors, 0 issues and 0 ERC violations. Hand
it to `build` as-is, adapting values and the header pinout to the request:

```json
{
 "title": "Common-Emitter BJT Audio Preamplifier",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "comments": [
  "9 V biased 2N3904 voltage amplifier",
  "Emitter degeneration with AC bypass",
  "Input and output on 2-pin headers"
 ],
 "paper": "A4",
 "parts": [
  {
   "id": "J1",
   "lib": "Connector:Conn_01x02_Pin",
   "value": "POWER 9V",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "V9",
    "2": "0V"
   }
  },
  {
   "id": "J2",
   "lib": "Connector:Conn_01x02_Pin",
   "value": "AUDIO IN",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "IN",
    "2": "0V"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "1uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "IN",
    "2": "BASE"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "V9",
    "2": "BASE"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "22k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "BASE",
    "2": "0V"
   }
  },
  {
   "id": "Q1",
   "lib": "Transistor_BJT:2N3904",
   "value": "2N3904",
   "footprint": "Package_TO_SOT_THT:TO-92_Inline",
   "pins": {
    "1": "EM",
    "2": "BASE",
    "3": "COL"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "V9",
    "2": "COL"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "EM",
    "2": "E_RAW"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "100uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "E_RAW",
    "2": "0V"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "COL",
    "2": "OUT"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "V9",
    "2": "0V"
   }
  },
  {
   "id": "J3",
   "lib": "Connector:Conn_01x02_Pin",
   "value": "AUDIO OUT",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "OUT",
    "2": "0V"
   }
  }
 ],
 "layout": [
  {
   "title": "POWER",
   "tree": {
    "row": [
     {
      "part": "J1"
     },
     {
      "part": "C4"
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "AUDIO AMPLIFIER",
   "note": "Signal flows left to right; bias and degeneration branches hang below the path.",
   "tree": {
    "row": [
     {
      "part": "J2"
     },
     {
      "part": "C1"
     },
     {
      "col": [
       {
        "part": "R1",
        "rot": 0
       },
       {
        "part": "R2",
        "rot": 0
       }
      ],
      "gap": 4
     },
     {
      "part": "Q1"
     },
     {
      "part": "R3",
      "rot": 0
     },
     {
      "col": [
       {
        "part": "R4",
        "rot": 0
       },
       {
        "part": "C2",
        "rot": 0
       }
      ],
      "gap": 4
     },
     {
      "part": "C3"
     },
     {
      "part": "J3"
     }
    ],
    "gap": 4
   }
  }
 ],
 "notes": [
  "Designed for a 9 V single supply; connect POWER 9V between V9 and 0V.",
  "Approximate midband gain: -4 to -6 V/V depending on load.",
  "Use shielded wiring for the audio input and output."
 ],
 "flags": [
  "0V"
 ]
}
```

## Checklist

- Base divider: 100k from V9 to BASE, 22k from BASE to 0V, sized for a quiescent base voltage of
  roughly a tenth of the supply plus one diode drop.
- Collector load 4.7k from V9 to COL sets the DC operating point together with the emitter
  resistor; midpoint COL should sit near V9/2 for maximum symmetric swing.
- Emitter degeneration: 1k from EM to E_RAW for bias stability, bypassed with a 100uF cap from
  E_RAW to 0V so gain is not throttled by the degeneration resistor at signal frequencies.
- Input coupling cap (1uF) blocks DC from the source into BASE; output coupling cap (10uF) blocks
  the collector's DC offset from the load. Both sized so their -3dB corner sits well below the
  lowest audio frequency of interest given the driving/load impedance.
- 100nF supply bypass (C4) directly across V9/0V, placed electrically at the amplifier stage, not
  only at the connector.
- `"flags": ["0V"]` is required: 0V renders as a GND power symbol (its Input Power pin) but the
  only thing driving it is the passive J1 connector pin, so ERC needs the PWR_FLAG. V9 is a plain
  net label (not a recognized power-symbol name), so it needs no flag.
- Every 2N3904 pin is used: pin 1 (E) to EM, pin 2 (B) to BASE, pin 3 (C) to COL. No `nc` pins on
  a 3-terminal device.
- Headers: J1 is the 9V/0V power in, J2 is the AC-coupled audio in, J3 is the AC-coupled audio
  out. Keep pin 2 of every header on 0V for a consistent ground pinout.
- Bias branch (R1/R2) sits in a col under the input coupling cap/BASE node; degeneration branch
  (R4/C2) sits in a col under the emitter node; the collector load (R3) hangs off the COL node -
  this keeps the row a single left-to-right signal path.
- Part count is 12 (3 headers, 1 transistor, 4 resistors, 4 capacitors), satisfying "part_count
  >= 12" without padding the design with unused parts.

## Common mistakes

- Omitting `"flags": ["0V"]` and leaving ERC reporting `power_pin_not_driven` on the GND power
  symbol - 0V is auto-drawn with a power symbol because its name is recognized as ground-like,
  but nothing else on the sheet outputs it.
- Skipping the bypass cap on the emitter resistor: without C2, the emitter resistor cuts AC gain
  to roughly R3/R4 instead of the much larger intrinsic-transistor gain.
- Putting the output coupling cap before the collector load in the row, which breaks the
  left-to-right signal-flow convention and produces crossed wires.
- Using the same net name for the DC supply and a power-symbol name like `+9V`/`VCC`: naming it
  `V9` deliberately keeps it a plain label so only 0V needs the PWR_FLAG treatment; if renamed to
  a recognized power name, flags must be updated to include it too.
- Forgetting the divider low-side resistor's ground return: R2 must land on 0V, not float or
  attach to the emitter node.
- Giving the 2N3904 named pin keys (`E`/`B`/`C`) inconsistently with the numeric keys used
  elsewhere in the file - either is valid, but pick one convention and confirm it with `pins.py`
  before writing the design JSON.
- Sizing the input/output coupling caps too small for the intended load, producing a bass-rolloff
  that a critic or ERC will not catch but a careful engineer would flag.
