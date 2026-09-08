---
name: audio-preamp
description: Single-supply two-stage audio preamplifier - 9 V barrel-jack entry with PTC fuse, series Schottky, SMB TVS and ferrite bead, TLE2426 virtual-ground midrail, MCP6002 dual op-amp with switch-selectable x2/x11 gain, two 100k pots, 3.5 mm switched mono jacks.
triggers: ["audio preamp", "preamplifier", "mcp6002", "tle2426", "virtual ground", "midrail", "single supply op-amp", "volume pot", "guitar preamp"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| J1 | `Connector:Barrel_Jack` | `Connector_BarrelJack:BarrelJack_Horizontal` | 9 V DC, centre positive |
| F1 | `Device:Polyfuse` | `Fuse:Fuse_1206_3216Metric` | 500 mA PTC |
| D1 | `Device:D_Schottky` | `Diode_SMD:D_SMA` | SS34 series reverse-polarity |
| D2 | `Device:D_TVS` | `Diode_SMD:D_SMB` | SMBJ12A clamp to GND |
| FB1 | `Device:FerriteBead` | `Inductor_SMD:L_0805_2012Metric` | 600 R @ 100 MHz |
| C1, C2 | `Device:C_Polarized` | `Capacitor_SMD:CP_Elec_6.3x7.7` / `CP_Elec_4x5.4` | 100 uF / 10 uF bulk |
| C3, C6 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF supply bypass |
| C7 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 10 uF op-amp local bulk |
| R1, D3 | `Device:R` / `Device:LED` | `Resistor_SMD:R_0603_1608Metric` / `LED_SMD:LED_0603_1608Metric` | 2k2 + power LED |
| U1 | `Reference_Voltage:TLE2426xLP` | `Package_TO_SOT_THT:TO-92_Inline` | rail splitter, VREF = 4.5 V |
| C4, C5 | `Device:C` | `C_0805_2012Metric` / `C_0603_1608Metric` | 10 uF + 100 nF VREF bypass |
| U2 | `Amplifier_Operational:MCP6002-xSN` | `Package_SO:SOIC-8_3.9x4.9mm_P1.27mm` | dual op-amp, 3 units |
| J2, J3 | `Connector_Audio:AudioJack2_Switch` | `Connector_Audio:Jack_3.5mm_CUI_SJ1-3514N_Horizontal` | mono in / mono out |
| RV1, RV2 | `Device:R_Potentiometer` | `Potentiometer_THT:Potentiometer_Alps_RK097_Single_Horizontal` | 100 k audio taper |
| SW1 | `Switch:SW_SPDT` | `Button_Switch_THT:SW_Slide_SPDT_Angled_CK_OS102011MA1Q` | gain x2 / x11 |
| R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1 k input series |
| C8 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 pF RF shunt |
| C9, C11 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1 uF coupling |
| R3, R7 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1 M bias to VREF |
| R4, R5, R6 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10 k / 10 k / 100 k gain set |
| C10, C12 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 47 pF / 22 pF stability |
| R8 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1 k buffer feedback |
| R9, R10 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100 R isolation / 100 k pulldown |
| C13 | `Device:C_Polarized` | `Capacitor_SMD:CP_Elec_4x5.4` | 10 uF output coupling |
| TP1-TP3 | `Connector:TestPoint` | `TestPoint:TestPoint_Pad_D1.5mm` | +9V, VREF, GND |

Pin-key notes confirmed against the installed libraries. `MCP6002-xSN` is a **3-unit** symbol whose
amplifier pins have blank or one-character names, so key them by NUMBER: unit 1 = `1` (out), `2` (-),
`3` (+); unit 2 = `5` (+), `6` (-), `7` (out); unit 3 (power) = `4` (V-), `8` (V+). Emit one part
entry per unit, all with id `U2`. `TLE2426xLP` pins are `OUT` (1), `COMMON` (2), `IN` (3) - COMMON is
the ground return, not the output. `Connector_Audio:AudioJack2_Switch` pin numbers are `T`, `S`, `TN`,
`SN` (no numeric pins); the CUI SJ1-3514N footprint has pads T, R, S, TN and **no SN pad**, so `SN`
must be `"nc"`. `Device:R_Potentiometer` pins are `1`, `2` (wiper), `3`. `Switch:SW_SPDT` pins are
`A` (1), `B` (2), `C` (3) with C the common throw. `Connector:Barrel_Jack` pins are unnamed `1`
(centre) and `2` (sleeve). `Device:D_Schottky` pins are `K` (1) and `A` (2); `Device:D_TVS` are `A1`,
`A2`; `Device:LED` are `K`, `A`. `Connector:TestPoint` has the single pin `1`.

## Pin map

```
+9V: FB1.2, C1.1, C2.1, C3.1, R1.1, TP1.1, U1.IN, U2.8, C6.1, C7.1
A1_OUT: U2.1, R5.1, R6.1, C10.1, C11.1
A2_OUT: U2.7, R8.1, C12.1, R9.1
BUF_FB: U2.6, R8.2, C12.2
FB: U2.2, R4.1, SW1.C
GAIN_HI: R6.2, C10.2, SW1.B
GAIN_LO: R5.2, SW1.A
GND: J1.2, D2.A2, C1.2, C2.2, C3.2, D3.K, U1.COMMON, C4.2, C5.2, TP3.1, J2.S, J2.TN, C8.2, U2.4, C6.2, C7.2, R10.2, J3.S
IN_RF: R2.2, C8.1, C9.1
IN_TIP: J2.T, R2.1
LED_A: R1.2, D3.A
LVL_IN: C11.2, RV2.3, R7.1
LVL_W: RV2.2, U2.5
OUT_ISO: R9.2, C13.1
OUT_JACK: C13.2, R10.1, J3.T
VIN_F: F1.2, D1.A
VIN_P: D1.K, D2.A1, FB1.1
VIN_RAW: J1.1, F1.1
VOL_IN: C9.2, RV1.3, R3.1
VOL_W: RV1.2, U2.3
VREF: U1.OUT, C4.1, C5.1, TP2.1, RV1.1, R3.2, R4.2, RV2.1, R7.2
```

## Layout

The design JSON below verifies clean (`layout_errors=0 issues=0 erc=0`, 41 parts, A3).
Hand it to `build` as-is, adapting values to the request:

```json
{
 "title": "Single-Supply Dual Op-Amp Audio Preamplifier",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A3",
 "comments": [
  "9 V protected entry, TLE2426 midrail, MCP6002 gain + buffer",
  "Selectable gain x2 / x11, AC-coupled mono in and out"
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector:Barrel_Jack",
   "value": "9V DC IN",
   "footprint": "Connector_BarrelJack:BarrelJack_Horizontal",
   "pins": {
    "1": "VIN_RAW",
    "2": "GND"
   }
  },
  {
   "id": "F1",
   "lib": "Device:Polyfuse",
   "value": "500mA PTC",
   "footprint": "Fuse:Fuse_1206_3216Metric",
   "pins": {
    "1": "VIN_RAW",
    "2": "VIN_F"
   }
  },
  {
   "id": "D1",
   "lib": "Device:D_Schottky",
   "value": "SS34",
   "footprint": "Diode_SMD:D_SMA",
   "pins": {
    "A": "VIN_F",
    "K": "VIN_P"
   }
  },
  {
   "id": "D2",
   "lib": "Device:D_TVS",
   "value": "SMBJ12A",
   "footprint": "Diode_SMD:D_SMB",
   "pins": {
    "A1": "VIN_P",
    "A2": "GND"
   }
  },
  {
   "id": "FB1",
   "lib": "Device:FerriteBead",
   "value": "600R @100MHz",
   "footprint": "Inductor_SMD:L_0805_2012Metric",
   "pins": {
    "1": "VIN_P",
    "2": "+9V"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C_Polarized",
   "value": "100uF 25V",
   "footprint": "Capacitor_SMD:CP_Elec_6.3x7.7",
   "pins": {
    "1": "+9V",
    "2": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C_Polarized",
   "value": "10uF 25V",
   "footprint": "Capacitor_SMD:CP_Elec_4x5.4",
   "pins": {
    "1": "+9V",
    "2": "GND"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+9V",
    "2": "GND"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "2k2",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+9V",
    "2": "LED_A"
   }
  },
  {
   "id": "D3",
   "lib": "Device:LED",
   "value": "PWR GREEN",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "A": "LED_A",
    "K": "GND"
   }
  },
  {
   "id": "TP1",
   "lib": "Connector:TestPoint",
   "value": "TP +9V",
   "footprint": "TestPoint:TestPoint_Pad_D1.5mm",
   "pins": {
    "1": "+9V"
   }
  },
  {
   "id": "U1",
   "lib": "Reference_Voltage:TLE2426xLP",
   "value": "TLE2426CLP",
   "footprint": "Package_TO_SOT_THT:TO-92_Inline",
   "pins": {
    "IN": "+9V",
    "COMMON": "GND",
    "OUT": "VREF"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "VREF",
    "2": "GND"
   }
  },
  {
   "id": "C5",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VREF",
    "2": "GND"
   }
  },
  {
   "id": "TP2",
   "lib": "Connector:TestPoint",
   "value": "TP VREF",
   "footprint": "TestPoint:TestPoint_Pad_D1.5mm",
   "pins": {
    "1": "VREF"
   }
  },
  {
   "id": "TP3",
   "lib": "Connector:TestPoint",
   "value": "TP GND",
   "footprint": "TestPoint:TestPoint_Pad_D1.5mm",
   "pins": {
    "1": "GND"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Audio:AudioJack2_Switch",
   "value": "MONO IN",
   "footprint": "Connector_Audio:Jack_3.5mm_CUI_SJ1-3514N_Horizontal",
   "pins": {
    "T": "IN_TIP",
    "S": "GND",
    "TN": "GND",
    "SN": "nc"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "IN_TIP",
    "2": "IN_RF"
   }
  },
  {
   "id": "C8",
   "lib": "Device:C",
   "value": "100pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "IN_RF",
    "2": "GND"
   }
  },
  {
   "id": "C9",
   "lib": "Device:C",
   "value": "1uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "IN_RF",
    "2": "VOL_IN"
   }
  },
  {
   "id": "RV1",
   "lib": "Device:R_Potentiometer",
   "value": "100k",
   "footprint": "Potentiometer_THT:Potentiometer_Alps_RK097_Single_Horizontal",
   "pins": {
    "3": "VOL_IN",
    "2": "VOL_W",
    "1": "VREF"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "1M",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VOL_IN",
    "2": "VREF"
   }
  },
  {
   "id": "U2",
   "lib": "Amplifier_Operational:MCP6002-xSN",
   "unit": 1,
   "value": "MCP6002",
   "footprint": "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
   "pins": {
    "3": "VOL_W",
    "2": "FB",
    "1": "A1_OUT"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "FB",
    "2": "VREF"
   }
  },
  {
   "id": "R5",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "A1_OUT",
    "2": "GAIN_LO"
   }
  },
  {
   "id": "R6",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "A1_OUT",
    "2": "GAIN_HI"
   }
  },
  {
   "id": "C10",
   "lib": "Device:C",
   "value": "47pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "A1_OUT",
    "2": "GAIN_HI"
   }
  },
  {
   "id": "SW1",
   "lib": "Switch:SW_SPDT",
   "value": "GAIN x2/x11",
   "footprint": "Button_Switch_THT:SW_Slide_SPDT_Angled_CK_OS102011MA1Q",
   "pins": {
    "A": "GAIN_LO",
    "B": "GAIN_HI",
    "C": "FB"
   }
  },
  {
   "id": "U2",
   "lib": "Amplifier_Operational:MCP6002-xSN",
   "unit": 3,
   "value": "MCP6002",
   "footprint": "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
   "pins": {
    "8": "+9V",
    "4": "GND"
   }
  },
  {
   "id": "C6",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+9V",
    "2": "GND"
   }
  },
  {
   "id": "C7",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+9V",
    "2": "GND"
   }
  },
  {
   "id": "C11",
   "lib": "Device:C",
   "value": "1uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "A1_OUT",
    "2": "LVL_IN"
   }
  },
  {
   "id": "RV2",
   "lib": "Device:R_Potentiometer",
   "value": "100k",
   "footprint": "Potentiometer_THT:Potentiometer_Alps_RK097_Single_Horizontal",
   "pins": {
    "3": "LVL_IN",
    "2": "LVL_W",
    "1": "VREF"
   }
  },
  {
   "id": "R7",
   "lib": "Device:R",
   "value": "1M",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "LVL_IN",
    "2": "VREF"
   }
  },
  {
   "id": "U2",
   "lib": "Amplifier_Operational:MCP6002-xSN",
   "unit": 2,
   "value": "MCP6002",
   "footprint": "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
   "pins": {
    "5": "LVL_W",
    "6": "BUF_FB",
    "7": "A2_OUT"
   }
  },
  {
   "id": "R8",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "A2_OUT",
    "2": "BUF_FB"
   }
  },
  {
   "id": "C12",
   "lib": "Device:C",
   "value": "22pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "A2_OUT",
    "2": "BUF_FB"
   }
  },
  {
   "id": "R9",
   "lib": "Device:R",
   "value": "100R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "A2_OUT",
    "2": "OUT_ISO"
   }
  },
  {
   "id": "C13",
   "lib": "Device:C_Polarized",
   "value": "10uF 16V",
   "footprint": "Capacitor_SMD:CP_Elec_4x5.4",
   "pins": {
    "1": "OUT_ISO",
    "2": "OUT_JACK"
   }
  },
  {
   "id": "R10",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "OUT_JACK",
    "2": "GND"
   }
  },
  {
   "id": "J3",
   "lib": "Connector_Audio:AudioJack2_Switch",
   "value": "MONO OUT",
   "footprint": "Connector_Audio:Jack_3.5mm_CUI_SJ1-3514N_Horizontal",
   "pins": {
    "T": "OUT_JACK",
    "S": "GND",
    "TN": "nc",
    "SN": "nc"
   }
  }
 ],
 "flags": [
  "+9V",
  "GND"
 ],
 "power": [
  "VREF"
 ],
 "notes": [
  "Center-positive barrel jack: PTC fuse, series Schottky, 12 V SMB TVS, ferrite bead.",
  "TLE2426 rail splitter sets VREF = +4.5 V; every stage is biased to it.",
  "SW1 selects 10k or 100k feedback for a gain of about 2 or 11; C10 rolls off the high-gain path.",
  "J2 tip-normalling contact grounds the input when no plug is inserted."
 ],
 "layout": [
  {
   "title": "POWER ENTRY",
   "note": "PTC fuse, reverse-polarity Schottky, 12 V SMB TVS and ferrite bead ahead of the bulk filter",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "J1"
       },
       {
        "part": "F1"
       },
       {
        "col": [
         {
          "part": "D1"
         },
         {
          "part": "D2"
         }
        ],
        "gap": 6
       },
       {
        "part": "FB1"
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "part": "C1"
       },
       {
        "part": "C2"
       },
       {
        "part": "C3"
       },
       {
        "col": [
         {
          "part": "R1"
         },
         {
          "part": "D3"
         }
        ],
        "gap": 4
       },
       {
        "part": "TP1"
       }
      ],
      "gap": 6
     }
    ],
    "gap": 8
   }
  },
  {
   "title": "MIDRAIL AND OP-AMP SUPPLY",
   "note": "TLE2426 splits the rail to VREF = 4.5 V; MCP6002 power unit with 100nF and 10uF local bypass",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "U1"
       },
       {
        "part": "C4"
       },
       {
        "part": "C5"
       },
       {
        "part": "TP2"
       },
       {
        "part": "TP3"
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "part": "U2",
        "unit": 3
       },
       {
        "part": "C6"
       },
       {
        "part": "C7"
       }
      ],
      "gap": 6
     }
    ],
    "gap": 8
   }
  },
  {
   "title": "INPUT AND VOLUME",
   "note": "1k / 100pF RF filter, 1uF coupling, 100k audio-taper volume pot referenced to VREF",
   "tree": {
    "row": [
     {
      "part": "J2"
     },
     {
      "col": [
       {
        "part": "R2"
       },
       {
        "part": "C8"
       }
      ],
      "gap": 5
     },
     {
      "part": "C9"
     },
     {
      "col": [
       {
        "part": "R3"
       },
       {
        "part": "RV1",
        "rot": 90
       }
      ],
      "gap": 5
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "GAIN STAGE",
   "note": "Non-inverting stage biased at VREF; SW1 picks 10k or 100k feedback for a gain of x2 or x11",
   "tree": {
    "row": [
     {
      "part": "R4"
     },
     {
      "part": "U2",
      "unit": 1
     },
     {
      "col": [
       {
        "row": [
         {
          "part": "R5"
         },
         {
          "part": "SW1"
         }
        ],
        "gap": 6
       },
       {
        "row": [
         {
          "part": "R6"
         },
         {
          "part": "C10"
         }
        ],
        "gap": 6
       }
      ],
      "gap": 6
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "LEVEL, BUFFER AND OUTPUT",
   "note": "Second 100k pot into a unity-gain buffer, then 100R isolation, 10uF coupling and a 100k pulldown at the jack",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "C11"
       },
       {
        "col": [
         {
          "part": "R7"
         },
         {
          "part": "RV2",
          "rot": 90
         }
        ],
        "gap": 5
       },
       {
        "part": "U2",
        "unit": 2
       },
       {
        "col": [
         {
          "part": "R8"
         },
         {
          "part": "C12"
         }
        ],
        "gap": 5
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "part": "R9"
       },
       {
        "col": [
         {
          "part": "C13"
         },
         {
          "part": "R10"
         }
        ],
        "gap": 5
       },
       {
        "part": "J3",
        "rot": 180
       }
      ],
      "gap": 6
     }
    ],
    "gap": 8
   }
  }
 ]
}
```

## Checklist

- Power entry order is fixed: barrel jack, 500 mA PTC, series Schottky, TVS to GND, ferrite bead,
  then the 100 uF / 10 uF / 100 nF bank. The TVS sits on the diode's cathode node (`VIN_P`), not on
  the raw jack, so it also clamps what the ferrite passes.
- `"flags": ["+9V", "GND"]` - both rails only ever come from a connector through passives, so ERC
  wants exactly one PWR_FLAG on each.
- `"power": ["VREF"]` makes the 4.5 V midrail draw as a power symbol on every stage; without it
  VREF becomes nine loose labels.
- Every op-amp unit is instantiated: unit 1 (gain), unit 2 (buffer), unit 3 (V+ / V-). Leaving out
  unit 3 leaves the supply pins unconnected and fails `unconnected_pins == []`.
- Local supply bypass at the op-amp: 100 nF (C6) plus 10 uF (C7), in the same block as unit 3.
- VREF bypass is 10 uF plus 100 nF at the TLE2426 output; the rail splitter must see a low
  impedance or the midrail modulates with the signal.
- Both pots have their CCW end (pin 1) on VREF and the signal on pin 3, wiper (pin 2) to the amp.
  The 1 M bias resistor (R3, R7) sits on the coupling-cap node so the cap has a DC path to VREF.
- Non-inverting gain: R4 (10 k) from the inverting input to VREF, SW1 selecting R5 (10 k, gain 2)
  or R6 (100 k, gain 11). C10 (47 pF) goes across the 100 k path only.
- The buffer is unity gain through R8 (1 k) with C12 (22 pF) across it. Do not short pin 7 straight
  to pin 6 - the 22 pF the prompt asks for then has nowhere to live.
- Output chain: 100 R isolation, 10 uF coupling with its `+` on the VREF-biased side, 100 k
  pulldown on the jack side of the cap.
- Input jack: sleeve to GND and the tip-normalling contact `TN` to GND, which mutes the input when
  no plug is inserted. Output jack `TN` and both `SN` pins are `"nc"`.
- Three test points, one each on +9V, VREF and GND.
- The sheet lands on A3 with 41 parts in five blocks and one residual warning: the `MONO IN` jack
  drops both `S` and `TN` to GND, so one stub crosses the other's GND text. Every rotation that
  clears it turns the tip connection into a net label, which reads worse - keep the jack upright.
- Rotating both pots 90 deg keeps their ref/value text off the wires, and the shunt part goes
  ABOVE the pot in its col (`[R3, RV1]`, `[R7, RV2]`) so the coupling cap wires straight into the
  pot instead of meeting it through a `VOL_IN` / `LVL_IN` label pair.
- Gaps of 5-6 inside the blocks: at gap 7-9 the same five blocks no longer fit A3.

## Common mistakes

- Keying MCP6002 amplifier pins by name. Pins 1 and 7 have an EMPTY name and `+`/`-` appear on two
  units each, so a name key either fails or connects the wrong unit. Use pin numbers.
- Emitting one `U2` entry with all eight pins. The symbol has three units and the layout tree needs
  each unit placed in the block it belongs to.
- Wiring the TLE2426 `COMMON` pin to VREF and `OUT` to ground - it is a rail splitter, `IN` is the
  9 V rail, `COMMON` is ground, `OUT` is the midrail.
- Giving `AudioJack2_Switch` numeric pin keys, or leaving `SN` connected: the SJ1-3514N footprint
  has no SN pad, so any net there becomes an unconnected item on the board.
- Referencing the pots and the inverting-input resistor to GND instead of VREF. On a single supply
  that clips the whole signal against the negative rail.
- Forgetting the PWR_FLAG on +9V; the barrel jack is a passive connector so ERC reports the rail as
  undriven.
- Putting the 47 pF across the low-gain (10 k) feedback path, which rolls off the flat setting
  instead of taming the x11 setting.
- Splitting the power-entry chain and the bulk filter into two blocks - they are one signal path and
  belong in one block, as two stacked rows.
