---
name: led-driver
description: Compact 5V low-side status LED driver - BC847 NPN switch, 1k base series, 10k base pulldown, 330R LED series
triggers: ["led driver", "status led", "npn led switch", "low-side switch", "bc847", "led indicator circuit", "transistor led"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| Q1 | `Transistor_BJT:BC847` | `Package_TO_SOT_SMD:SOT-23` | BC847 |
| J1 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | POWER (VCC/GND) |
| J2 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | CTRL (CTRL/GND) |
| C1 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF (VCC decouple) |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k (base series) |
| R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 330R (LED series) |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k (base pulldown) |
| D1 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | RED |

Pin-key notes verified against the installed library: `Transistor_BJT:BC847` pins are
`B`/`E`/`C` (base/emitter/collector) - the collector, not the emitter, sinks the LED cathode in
a low-side switch. `Device:LED` pins are `K`(cathode)/`A`(anode).

## Pin map

```
BASE: R1.2, Q1.B
CTRL: J2.1, R3.1, R1.1
GND: J1.2, C1.2, J2.2, R3.2, Q1.E
LED_A: R2.2, D1.A
LED_K: Q1.C, D1.K
VCC: J1.1, C1.1, R2.1
```

## Layout

Builds with 0 issues and 0 ERC violations:

```json
{
 "title": "5V Low-Side LED Driver",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "NPN low-side switch, 5V status LED",
  "1k base series, 10k base pulldown, 330R LED series"
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "POWER",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "VCC",
    "2": "GND"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VCC",
    "2": "GND"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "CTRL",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "CTRL",
    "2": "GND"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CTRL",
    "2": "GND"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CTRL",
    "2": "BASE"
   }
  },
  {
   "id": "Q1",
   "lib": "Transistor_BJT:BC847",
   "value": "BC847",
   "footprint": "Package_TO_SOT_SMD:SOT-23",
   "pins": {
    "B": "BASE",
    "C": "LED_K",
    "E": "GND"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "330R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VCC",
    "2": "LED_A"
   }
  },
  {
   "id": "D1",
   "lib": "Device:LED",
   "value": "RED",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "A": "LED_A",
    "K": "LED_K"
   }
  }
 ],
 "flags": [
  "VCC",
  "GND"
 ],
 "notes": [
  "Q1 sinks the LED cathode (low-side switch): CTRL high turns the LED on.",
  "R2 (330R) limits the red LED to ~9 mA from 5V; R1/R3 set base drive and a definite off state."
 ],
 "layout": [
  {
   "title": "POWER AND INPUT",
   "note": "Supply decoupling and the CTRL input with its pulldown",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "J1"
       },
       {
        "part": "C1"
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "part": "J2"
       },
       {
        "part": "R3"
       }
      ],
      "gap": 6
     }
    ],
    "gap": 10
   }
  },
  {
   "title": "DRIVER",
   "note": "Base series resistor into the NPN low-side switch, LED with its series resistor on the collector",
   "tree": {
    "row": [
     {
      "part": "R1"
     },
     {
      "part": "Q1"
     },
     {
      "col": [
       {
        "part": "R2"
       },
       {
        "part": "D1"
       }
      ],
      "gap": 5
     }
    ],
    "gap": 8
   }
  }
 ]
}
```

## Checklist

- Low-side means the transistor sinks current from the LED cathode to GND; the LED anode goes
  to VCC through its series resistor, the collector goes to the LED cathode, the emitter goes
  straight to GND.
- Base needs BOTH a series resistor from CTRL (limits drive current, ~1k for a logic-level
  source) and a pulldown to GND (~10k, guarantees OFF when CTRL is floating/high-Z).
- Size the LED series resistor for the actual rail: (5V - Vf_red~2V) / target current; 330R
  gives ~9mA, a normal indicator brightness. Recompute if the rail or LED color changes.
- Two separate connectors: POWER (VCC/GND) and CTRL (CTRL/GND) - never combine them into one
  4-pin header when the rubric asks for connectors that "expose only power and CTRL".
- Decoupling: 100nF on VCC/GND even for a circuit this small, since the LED switching edge can
  ring on a long supply lead.
- Reference designators follow convention (Q, R, C, D, J) numbered 1..n; keep base-network
  resistors close together in numbering (R1 series, R3 pulldown) for readability.
- Two blocks only (POWER AND INPUT, DRIVER) - never split down to one block per part on a
  7-8 part design; that fragments a design this small into unreadable pieces.

## Common mistakes

- Wiring the transistor high-side (emitter to LED, collector to GND) - that is not a low-side
  switch and inverts the control logic.
- Omitting the base pulldown: without R3 the LED can glow dimly or flicker from leakage/noise
  when CTRL is undriven.
- Undersizing the LED resistor (e.g. reusing a 3.3V-rail value like 220R on a 5V rail) and
  overdriving the LED, or oversizing it into invisibility.
- Merging POWER and CTRL onto one connector - the rubric explicitly wants them separate.
- Forgetting PWR_FLAG on VCC/GND when both rails are only ever driven by the POWER connector.
- Splitting this small circuit into more than two blocks (e.g. a block just for R1, a block
  just for D1) - group by function (input conditioning vs the switch itself) instead.
