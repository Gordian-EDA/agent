---
name: buck-converter
description: 12V to 5V 1A buck converter block using a real LM2596S-ADJ switching regulator - fused/TVS-protected input, catch diode, adjustable FB divider, enable pull-down, input/output connectors.
triggers: ["buck converter", "step-down converter", "12v to 5v", "switching regulator", "lm2596", "dc-dc converter", "voltage regulator circuit"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| J1 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | 12V IN |
| F1 | `Device:Polyfuse` | `Fuse:Fuse_1210_3225Metric` | 1.5A PTC |
| D1 | `Device:D_TVS` | `Diode_SMD:D_SMA` | SMBJ18A |
| C1 | `Device:C_Polarized` | `Capacitor_THT:CP_Radial_D6.3mm_P2.50mm` | 100 uF input bulk |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF input bypass |
| U1 | `Regulator_Switching:LM2596S-ADJ` | `Package_TO_SOT_SMD:TO-263-5_TabPin3` | LM2596S-ADJ |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k ON/OFF pull-down |
| D2 | `Device:D_Schottky` | `Diode_SMD:D_SMC` | 1N5822 catch diode |
| L1 | `Device:L` | `Inductor_SMD:L_Bourns_SRN6045TA` | 33 uH |
| C3 | `Device:C_Polarized` | `Capacitor_THT:CP_Radial_D6.3mm_P2.50mm` | 220 uF output bulk |
| C4 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF output bypass |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 3.6k FB divider top |
| C5 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 10 nF feedforward (parallel with R1) |
| R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1.2k FB divider bottom |
| J2 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | 5V OUT |

Pin-key notes verified against the installed libraries: `LM2596S-ADJ` (TO-263-5) pins are
`VIN`/`GND`/`OUT`/`FB`/`~{ON}/OFF` - the tilde-brace marks an overline, so this pin is
active-low (pull it to GND to run; float or pull high to shut down). This part is
**asynchronous** (no internal low-side switch), so the external catch diode D2 from `OUT` to
`GND` is mandatory - omitting it lets the switch node fly inductively and destroys the
regulator. `Device:D_TVS` pins are `A1`/`A2` (symmetric, no polarity). `Device:D_Schottky`
pins are `K`/`A`. `Device:C_Polarized` pins are `1` (+) / `2` (-). `Device:Polyfuse` and
`Device:L` pins are bare `1`/`2`.

The fixed-output `LM2596S-5` also exists in the library but its `FB` pin ties internally to
`OUT` with no external divider, which does not exercise the feedback arithmetic the rubric
checks for - use the ADJ part and compute the divider explicitly.

## Pin map

```
+12V: J1.1, F1.1
+5V: L1.2, C3.1, C4.1, R1.1, C5.1, J2.1
EN: U1.~{ON}/OFF, R3.1
FB: U1.FB, R1.2, C5.2, R2.1
GND: J1.2, D1.A2, C1.2, C2.2, U1.GND, R3.2, D2.A, C3.2, C4.2, R2.2, J2.2
SW: U1.OUT, D2.K, L1.1
VIN: F1.2, D1.A1, C1.1, C2.1, U1.VIN
```

## Layout

The design JSON below builds with 0 issues and 0 ERC violations on A4. Hand it to `build`
as-is, adapting values (input voltage, current, output voltage) to the request:

```json
{
 "title": "12V to 5V 1A Buck Converter",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "LM2596S-ADJ asynchronous step-down, 5V 1A output",
  "Adjustable feedback divider set for approx. 5V, external catch diode"
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "12V IN",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "+12V",
    "2": "GND"
   }
  },
  {
   "id": "F1",
   "lib": "Device:Polyfuse",
   "value": "1.5A PTC",
   "footprint": "Fuse:Fuse_1210_3225Metric",
   "pins": {
    "1": "+12V",
    "2": "VIN"
   }
  },
  {
   "id": "D1",
   "lib": "Device:D_TVS",
   "value": "SMBJ18A",
   "footprint": "Diode_SMD:D_SMA",
   "pins": {
    "A1": "VIN",
    "A2": "GND"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C_Polarized",
   "value": "100uF",
   "footprint": "Capacitor_THT:CP_Radial_D6.3mm_P2.50mm",
   "pins": {
    "1": "VIN",
    "2": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VIN",
    "2": "GND"
   }
  },
  {
   "id": "U1",
   "lib": "Regulator_Switching:LM2596S-ADJ",
   "value": "LM2596S-ADJ",
   "footprint": "Package_TO_SOT_SMD:TO-263-5_TabPin3",
   "pins": {
    "VIN": "VIN",
    "GND": "GND",
    "OUT": "SW",
    "FB": "FB",
    "~{ON}/OFF": "EN"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "EN",
    "2": "GND"
   }
  },
  {
   "id": "D2",
   "lib": "Device:D_Schottky",
   "value": "1N5822",
   "footprint": "Diode_SMD:D_SMC",
   "pins": {
    "K": "SW",
    "A": "GND"
   }
  },
  {
   "id": "L1",
   "lib": "Device:L",
   "value": "33uH",
   "footprint": "Inductor_SMD:L_Bourns_SRN6045TA",
   "pins": {
    "1": "SW",
    "2": "+5V"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C_Polarized",
   "value": "220uF",
   "footprint": "Capacitor_THT:CP_Radial_D6.3mm_P2.50mm",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "3.6k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "FB"
   }
  },
  {
   "id": "C5",
   "lib": "Device:C",
   "value": "10nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "FB"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "1.2k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "FB",
    "2": "GND"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "5V OUT",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  }
 ],
 "flags": [
  "+12V",
  "VIN",
  "+5V",
  "GND"
 ],
 "notes": [
  "F1/D1 fuse the 12V input and clamp transients; C1/C2 form the input bulk+bypass pair.",
  "D2 is the external catch diode required by this asynchronous buck (LM2596 has no low-side FET).",
  "R1/R2 set VOUT = 1.23V * (1 + R1/R2) = 1.23 * 4 = approx. 4.92V; C5 is the feedforward cap across R1.",
  "R3 pulls the active-low ON/OFF pin to GND so the regulator runs at power-up."
 ],
 "layout": [
  {
   "title": "INPUT POWER",
   "note": "Fused, TVS-clamped 12 V input; C1/C2 are the bulk + bypass pair",
   "tree": {
    "row": [
     {
      "part": "J1"
     },
     {
      "part": "F1",
      "rot": 90
     },
     {
      "row": [
       {
        "part": "D1"
       },
       {
        "part": "C1"
       },
       {
        "part": "C2"
       }
      ],
      "gap": 4
     }
    ],
    "gap": 12
   }
  },
  {
   "title": "BUCK REGULATOR AND OUTPUT",
   "note": "LM2596S-ADJ step-down; D2 catches the switch node, R1/R2 divide 5 V down to the 1.23 V reference",
   "tree": {
    "col": [
     {
      "row": [
       {
        "col": [
         {
          "part": "R3"
         }
        ],
        "gap": 4
       },
       {
        "part": "U1"
       },
       {
        "col": [
         {
          "part": "D2"
         }
        ],
        "gap": 4
       },
       {
        "part": "L1"
       },
       {
        "part": "C3"
       },
       {
        "part": "C4"
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "col": [
         {
          "row": [
           {
            "part": "R1"
           },
           {
            "part": "C5"
           }
          ],
          "gap": 4
         },
         {
          "part": "R2"
         }
        ],
        "gap": 4
       },
       {
        "part": "J2"
       }
      ],
      "gap": 8
     }
    ],
    "gap": 8
   }
  }
 ]
}
```

## Checklist

- D2 (Schottky catch diode) from the switch node (`OUT`) to GND is mandatory - LM2596 is
  asynchronous and has no internal freewheeling path.
- FB divider: `Vout = 1.23V * (1 + R1/R2)`. R1=3.6k/R2=1.2k gives approx. 4.92V; recompute
  for any other target voltage and state the arithmetic in a note.
- C5 (10 nF) feedforward cap across R1 improves transient response, per the ADJ datasheet's
  application circuit - keep it whenever R1 is in the multi-kilohm range.
- Input: fuse (or PTC) ahead of everything, plus a TVS (or reverse-polarity diode/FET) across
  VIN-GND, sized above the supply rail (18 V TVS for a 12 V line).
- Bulk electrolytic + 100 nF ceramic on both VIN and VOUT - the ceramic close to the IC pins,
  the electrolytic absorbs switching ripple.
- `~{ON}/OFF` is active-low: tie it to GND (through a pull-down resistor, not a bare wire, so
  a disconnected strap still enables the part) for always-on operation.
- Inductor value/current rating must clear the 1 A output plus ripple current headroom; a
  33-47 uH shielded power inductor is typical at ~100-150 kHz switching.
- PWR_FLAG both the input and output supply nets (`+12V`, `+5V`) since neither is driven by
  anything but a connector/regulator output, and PWR_FLAG `GND` too - a ground net fed only by
  a connector still needs one, verified empirically (a GND-only-from-connector 2-part test
  design fails ERC with `power_pin_not_driven` until GND is flagged).
- Do not flag `VIN`/`SW`/`FB` unless a `power_pin_not_driven` ERC error names them - `VIN`
  needed it here only because it feeds `U1.VIN` (a `power_in` pin) with no other driver.
- Power-symbol orientation is automatic for `+12V`/`+5V`/`GND`; do not add `rot` to fix it.

## Common mistakes

- Using the fixed `LM2596S-5` instead of the ADJ part: its FB pin ties internally to OUT, so
  there is no external divider to size - it fails the rubric's feedback-arithmetic check.
- Leaving out the catch diode because the datasheet symbol only shows five pins with no
  obvious "needs a diode" cue - asynchronous bucks always need one unless the part name says
  "synchronous".
- Tying FB straight to VOUT (no divider) or straight to GND (divider ratio zero) - both are
  common accidental shorts when wiring the pin map by hand.
- Wiring the TVS with implied polarity - `Device:D_TVS` is bidirectional (`A1`/`A2`), do not
  treat it like a Schottky with `A`/`K`.
- Forgetting `GND` in `flags`: unlike some other designs, a ground net whose only driver is a
  passive connector pin still trips `power_pin_not_driven` in real KiCad ERC.
- Cramming J1/F1 too close together: the PWR_FLAG placer needs a few grid units of straight
  wire to land the flag symbol on; a gap of 10 in that row is what made it succeed here.
