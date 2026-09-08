---
name: usb-c-power-input
description: Reusable USB-C sink power-entry block - fused/TVS-protected VBUS, independent CC1/CC2 pull-downs, USBLC6-2SC6 ESD-clamped D+/D- broken out, VBUS-present sense divider.
triggers: ["usb-c power", "usb type-c input", "usb-c sink", "usb-c receptacle", "usbc power input", "usb c connector power", "cc pull-down"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| J1 | `Connector:USB_C_Receptacle_USB2.0_16P` | `Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal` | USB-C SINK |
| F1 | `Device:Polyfuse` | `Fuse:Fuse_1210_3225Metric` | 2A PTC |
| D1 | `Device:D_TVS` | `Diode_SMD:D_SOD-123` | PESD5V0X1BT |
| C1 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 10 uF VBUS bulk |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF VBUS bypass |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 5.1k CC1 pull-down |
| R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 5.1k CC2 pull-down |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k VBUS-sense divider top |
| R4 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 33k VBUS-sense divider bottom |
| U1 | `Power_Protection:USBLC6-2SC6` | `Package_TO_SOT_SMD:SOT-23-6` | USBLC6-2SC6 |
| J2 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | USB DATA |

Pin-key notes verified against the installed libraries: `Connector:USB_C_Receptacle_USB2.0_16P`
uses the A/B row numbering, not sequential pin numbers - confirmed pin set is
`GND(A1,B1,A12,B12) VBUS(A4,A9,B4,B9) CC1(A5) CC2(B5) D+(A6,B6) D-(A7,B7) SBU1(A8) SBU2(B8)
SHIELD(SH)`. Only ONE physical VBUS pin needs listing per key-by-name (`"VBUS"` in the pins
dict covers all four VBUS pins and both GND rows at once via name-key expansion), but **do
this only for VBUS/GND** - see Common mistakes for why D+/D- must be wired by pin NUMBER
(`A6`/`A7`), not by name. `Power_Protection:USBLC6-2SC6` (SOT-23-6) pins are
`1:I/O1 2:GND 3:I/O2 4:I/O2 5:VBUS 6:I/O1` - I/O1 exists at both pins 1 and 6 (electrically
identical, either can be used), I/O2 at both 3 and 4 likewise; use ONE pin per line (this
skill uses 1 and 4) and leave the other `nc`. `Device:D_TVS` pins are `A1`/`A2` (bidirectional,
no polarity).

## Pin map

```
CC1: J1.CC1, R1.1
CC2: J1.CC2, R2.1
GND: J1.GND, J1.SH, D1.A2, C1.2, C2.2, R1.2, R2.2, R4.2, U1.2
USB_DM: J1.A7, U1.4, J2.2
USB_DP: J1.A6, U1.1, J2.1
VBUS: F1.2, D1.A1, C1.1, C2.1, R3.1, U1.5
VBUS_IN: J1.VBUS, F1.1
VBUS_SENSE: R3.2, R4.1
```

## Layout

The design JSON below builds with 0 issues and 0 ERC violations on A4. Hand it to `build`
as-is; when embedding this block in a larger sheet, keep J1's block self-contained and bridge
`VBUS`/`GND`/`USB_DP`/`USB_DM` into the rest of the design by net name:

```json
{
 "title": "USB-C Power Input",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "USB-C sink power entry: fused/TVS-protected VBUS, CC pull-downs",
  "ESD-protected D+/D- broken out, VBUS-present sense divider"
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector:USB_C_Receptacle_USB2.0_16P",
   "value": "USB-C SINK",
   "footprint": "Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal",
   "pins": {
    "GND": "GND",
    "VBUS": "VBUS_IN",
    "CC1": "CC1",
    "CC2": "CC2",
    "SBU1": "nc",
    "SBU2": "nc",
    "SH": "GND",
    "A6": "USB_DP",
    "B6": "nc",
    "A7": "USB_DM",
    "B7": "nc"
   }
  },
  {
   "id": "F1",
   "lib": "Device:Polyfuse",
   "value": "2A PTC",
   "footprint": "Fuse:Fuse_1210_3225Metric",
   "pins": {
    "1": "VBUS_IN",
    "2": "VBUS"
   }
  },
  {
   "id": "D1",
   "lib": "Device:D_TVS",
   "value": "PESD5V0X1BT",
   "footprint": "Diode_SMD:D_SOD-123",
   "pins": {
    "A1": "VBUS",
    "A2": "GND"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "VBUS",
    "2": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VBUS",
    "2": "GND"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "5.1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CC1",
    "2": "GND"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "5.1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CC2",
    "2": "GND"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VBUS",
    "2": "VBUS_SENSE"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "33k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VBUS_SENSE",
    "2": "GND"
   }
  },
  {
   "id": "U1",
   "lib": "Power_Protection:USBLC6-2SC6",
   "value": "USBLC6-2SC6",
   "footprint": "Package_TO_SOT_SMD:SOT-23-6",
   "pins": {
    "1": "USB_DP",
    "2": "GND",
    "3": "nc",
    "4": "USB_DM",
    "5": "VBUS",
    "6": "nc"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "USB DATA",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "USB_DP",
    "2": "USB_DM"
   }
  }
 ],
 "flags": [
  "VBUS",
  "GND"
 ],
 "notes": [
  "F1/D1 fuse and clamp VBUS; C1/C2 are the bulk + bypass input caps.",
  "R1/R2 are independent 5.1k CC pull-downs, marking this port a power sink (never a shared resistor).",
  "R3/R4 divide VBUS 100k/33k to VBUS_SENSE (approx. 1.16V at 5V) for a GPIO presence check.",
  "U1 clamps D+/D- ESD and passes them through unbroken to the USB_DP/USB_DM nets on J2."
 ],
 "layout": [
  {
   "title": "USB-C POWER",
   "note": "Fused/TVS-clamped VBUS, CC pull-downs, VBUS-sense divider, ESD-clamped data header",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "J1"
       },
       {
        "row": [
         {
          "part": "R1"
         },
         {
          "part": "R2"
         }
        ],
        "gap": 3
       }
      ],
      "gap": 3
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
        "part": "C1"
       },
       {
        "part": "C2"
       }
      ],
      "gap": 3
     },
     {
      "col": [
       {
        "part": "R3"
       },
       {
        "part": "R4"
       }
      ],
      "gap": 3
     },
     {
      "part": "U1"
     },
     {
      "part": "J2"
     }
    ],
    "gap": 3
   }
  }
 ]
}
```

## Checklist

- Both VBUS pin groups and both GND pin groups tied together - use the name key (`"VBUS"`,
  `"GND"`) so every physical instance is covered in one line.
- CC1 and CC2 each get their OWN 5.1k pull-down to GND - two resistors, never one shared
  resistor across both lines (that would defeat orientation-independent sink detection).
- A fuse (or resettable PTC, 1.5-2 A) in series with VBUS ahead of the TVS/bulk cap.
- A 5 V-rated TVS or ESD array across VBUS-GND, plus bulk (approx. 10 uF) + 100 nF ceramic
  input capacitance.
- D+/D- routed through `USBLC6-2SC6` before reaching any header/MCU net - never wire the
  connector's D+/D- straight to a header, skipping the ESD clamp.
- A VBUS-present divider (100k/33k here, approx. 1.16 V at 5 V VBUS) feeding a labelled net
  meant for a GPIO input, not a power rail - do not draw it with a power symbol.
- Shield (`SH`) tied to GND; `SBU1`/`SBU2` left `nc` unless the design implements an alt-mode.
- PWR_FLAG both `VBUS` and `GND` - both are driven only by the connector's passive pins, and a
  GND net fed only by a connector still needs the flag (verified empirically; see the
  buck-converter skill's checklist for the minimal repro).
- Keep the whole thing as ONE block (11 parts fits the 3-12 rule) - splitting connector,
  protection and ESD into separate tiny blocks forces the auto-fold lint to re-merge them and
  produced a worse layout when tried here.

## Common mistakes

- Wiring D+/D- by NAME key (`"D+"`, `"D-"`) instead of by pin NUMBER. This connector's D+/D-
  pins are electrically interleaved with each other in the symbol geometry (A6/B6 for D+ sit
  between B7/A7 for D-, 2.54 mm apart each), and letting a name-key silently expand to both
  A/B instances tickles a real auto-router bug that bridges D+ and D- into one shorted net
  (KiCad ERC: `multiple_net_names` / "nets shorted together"). Fix: address `A6`/`A7`
  explicitly, wire only one side, and mark `B6`/`B7` `nc`.
- Wiring BOTH USBLC6 I/O1 pins (1 and 6) or both I/O2 pins (3 and 4) at once. They are
  redundant same-node options in the real part, but wiring both here reproduces the same
  short-circuit auto-router bug as above (adjacent pins on the same package side). Use one
  pin per data line and `nc` the other.
- A single shared resistor across CC1 and CC2 - always two independent 5.1k pull-downs.
- Treating VBUS_SENSE as a power rail (it is a signal net for a GPIO ADC/comparator input,
  not a supply - do not add it to `flags` or `power`).
- Forgetting the shield pin (`SH`) - an isolated shield floats ESD energy onto the board edge.
- Letting D+/D- skip the ESD IC entirely by routing straight from J1 to a header - defeats the
  point of including U1.
