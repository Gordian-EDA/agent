---
name: can-node
description: 3.3V CAN bus transceiver node - SN65HVD230, TXD/RXD MCU header, CANH/CANL connector, switchable split-120R termination, bidirectional TVS
triggers: ["can bus", "can transceiver", "can node", "sn65hvd230", "tja1051", "canh canl", "can termination"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `Interface_CAN_LIN:SN65HVD230` | `Package_SO:SOIC-8_3.9x4.9mm_P1.27mm` | SN65HVD230 |
| J1 | `Connector_Generic:Conn_01x04` | `Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical` | MCU IF (VCC/GND/TXD/RXD) |
| J2 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | CAN BUS (CANH/CANL) |
| D1 | `Device:D_TVS` | `Diode_SMD:D_SOD-123F` | PESD1CAN (bidirectional TVS) |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k (Rs slope select) |
| R2, R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 60R (split 120R termination) |
| C1 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF (VCC decouple) |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1uF (VCC bulk) |
| C3 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 4.7nF (termination midpoint, common-mode filter) |
| SW1 | `Switch:SW_SPST` | `Button_Switch_THT:SW_DIP_SPSTx01_Slide_9.78x4.72mm_W7.62mm_P2.54mm` | TERM EN |

Pin-key notes verified against the installed library: `Interface_CAN_LIN:SN65HVD230` (a genuine
3.3V-supply part, unlike the 5V TJA1051T/MCP2551 family) has pins `D`(driver input = TXD),
`R`(receiver output = RXD), `Rs`(slope control), `Vref`(bias reference output, left `nc`),
`CANH`/`CANL`(bidirectional bus), `VCC`/`GND`. `Device:D_TVS` pins are `A1`/`A2` (bidirectional,
either orientation is correct). `Switch:SW_SPST` pins are `1`/`2`.

## Pin map

```
CANH: U1.CANH, J2.1, D1.A1, R2.1
CANL: U1.CANL, J2.2, D1.A2, SW1.2
GND: U1.GND, R1.2, C1.2, C2.2, J1.2, C3.2
RS: U1.Rs, R1.1
RXD: U1.R, J1.4
TERM_MID: R2.2, R3.1, C3.1
TERM_SW: R3.2, SW1.1
TXD: U1.D, J1.3
VCC: U1.VCC, C1.1, C2.1, J1.1
```

## Layout

Builds with 0 issues and 0 ERC violations:

```json
{
 "title": "3.3V CAN Transceiver Node",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "SN65HVD230 3.3V CAN transceiver",
  "Switchable 120R split termination, bus TVS"
 ],
 "parts": [
  {
   "id": "U1",
   "lib": "Interface_CAN_LIN:SN65HVD230",
   "value": "SN65HVD230",
   "footprint": "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
   "pins": {
    "VCC": "VCC",
    "GND": "GND",
    "D": "TXD",
    "R": "RXD",
    "Rs": "RS",
    "Vref": "nc",
    "CANH": "CANH",
    "CANL": "CANL"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "RS",
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
   "id": "C2",
   "lib": "Device:C",
   "value": "1uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VCC",
    "2": "GND"
   }
  },
  {
   "id": "J1",
   "lib": "Connector_Generic:Conn_01x04",
   "value": "MCU IF",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
   "pins": {
    "1": "VCC",
    "2": "GND",
    "3": "TXD",
    "4": "RXD"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "CAN BUS",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "CANH",
    "2": "CANL"
   }
  },
  {
   "id": "D1",
   "lib": "Device:D_TVS",
   "value": "PESD1CAN",
   "footprint": "Diode_SMD:D_SOD-123F",
   "pins": {
    "A1": "CANH",
    "A2": "CANL"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "60R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CANH",
    "2": "TERM_MID"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "60R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "TERM_MID",
    "2": "TERM_SW"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "4.7nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "TERM_MID",
    "2": "GND"
   }
  },
  {
   "id": "SW1",
   "lib": "Switch:SW_SPST",
   "value": "TERM EN",
   "footprint": "Button_Switch_THT:SW_DIP_SPSTx01_Slide_9.78x4.72mm_W7.62mm_P2.54mm",
   "pins": {
    "1": "TERM_SW",
    "2": "CANL"
   }
  }
 ],
 "flags": [
  "VCC",
  "GND"
 ],
 "notes": [
  "Rs tied to GND through 10k selects high-speed slope on the SN65HVD230.",
  "120R split termination (60R+60R) with a common-mode filter cap at the midpoint; SW1 disconnects it when the node is not a bus end.",
  "D1 is a bidirectional TVS across CANH/CANL right at the bus connector."
 ],
 "layout": [
  {
   "title": "TRANSCEIVER",
   "note": "SN65HVD230 with MCU-side header, decoupling and slope-control resistor",
   "tree": {
    "row": [
     {
      "part": "J1"
     },
     {
      "col": [
       {
        "part": "C1"
       },
       {
        "part": "C2"
       }
      ],
      "gap": 5
     },
     {
      "part": "U1"
     },
     {
      "col": [
       {
        "part": "R1"
       }
      ],
      "gap": 5
     }
    ],
    "gap": 8
   }
  },
  {
   "title": "BUS TERMINATION AND PROTECTION",
   "note": "TVS at the connector, switchable split 120R termination behind it",
   "tree": {
    "col": [
     {
      "part": "J2"
     },
     {
      "part": "D1",
      "rot": 90
     },
     {
      "row": [
       {
        "part": "R2"
       },
       {
        "col": [
         {
          "part": "C3"
         }
        ],
        "gap": 5
       },
       {
        "part": "R3"
       },
       {
        "part": "SW1"
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

- Use a real 3.3V CAN transceiver (SN65HVD230 / TJA1051T-3 / TCAN33x). Do NOT use TJA1051T or
  MCP2551 bare - those are 5V-VCC parts and wrong for a 3.3V node.
- 100nF ceramic close to VCC/GND plus a 1uF bulk cap; this matches SN65HVD230's own datasheet
  decoupling recommendation.
- Rs (slope control) never floats: tie to GND (direct or through <=10k) for high-speed mode, to
  VCC for standby/silent mode. Never leave it unconnected on a real board.
- Vref is an output (internal bias reference) - leave `nc`, never drive it externally.
- The bus connector carries only CANH/CANL, never VCC/GND (galvanic isolation of the bus side).
- TVS goes at the physical bus connector, in parallel with CANH/CANL, ahead of the transceiver
  and termination - it must clamp transients arriving from off-board first.
- Termination must be a real bridge from CANH to CANL, not a resistor to a rail. Split 120R
  (60R+60R with a GND midpoint cap) also suppresses common-mode EMI; a lone 120R works too.
- The termination bridge must be genuinely switchable: an SPST switch or bridged jumper in
  series with it, not a fixed always-on resistor, since only bus-end nodes should terminate.
- TXD (MCU->transceiver, pin `D`) and RXD (transceiver->MCU, pin `R`) must not be swapped -
  `pins.py` names them `D`/`R`, not `TXD`/`RXD`, on the SN65HVD230 symbol.
- PWR_FLAG on both VCC and GND since both are only driven by the MCU-side connector.

## Common mistakes

- Picking TJA1051T (5V VCC) or MCP2551 (5V) for a "3.3V CAN transceiver" request - confirm the
  VCC rating in the symbol description before choosing.
- Wiring the TVS or termination directly to a MCU-side net instead of CANH/CANL at the bus
  connector - protection and termination both belong on the bus side.
- A termination resistor tied CANH-to-GND or CANL-to-GND instead of CANH-to-CANL: that does not
  terminate the differential pair and looks like a dead short to the checker.
- Fixed (non-switchable) termination on a node that is not guaranteed to be a bus end.
- Forgetting Rs: an open slope-control pin leaves the transceiver in an undefined mode.
- Stacking two shunt parts with different, unrelated nets (e.g. a slope resistor and a bus TVS)
  in the same `col` - engine composition rules require a `col`'s members share the series
  part's node; unrelated shunts placed too close can render with touching power-symbol stubs
  and falsely short two nets together. Give each net's local decoupling/shunt its own row/col.
