---
name: hbridge
description: Discrete 12 V brushed DC motor H-bridge - two P-channel high-side and two N-channel low-side MOSFETs, NPN level-shifter drivers from 3.3 V logic, gate resistors, flyback diodes, bulk cap and a 2-pin motor connector.
triggers: ["h-bridge", "hbridge", "h bridge", "motor driver", "brushed dc motor", "mosfet bridge", "discrete motor driver"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| J1 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | 12 V power in |
| C1 | `Device:C_Polarized` | `Capacitor_THT:CP_Radial_D8.0mm_P3.50mm` | 470 uF bulk |
| J2 | `Connector_Generic:Conn_01x04` | `Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical` | 3.3 V logic in |
| R1, R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k base series |
| Q5, Q6 | `Transistor_BJT:Q_NPN_BCE` | `Package_TO_SOT_THT:TO-92_Inline` | 2N3904 level-shifter |
| Q1, Q2 | `Transistor_FET:Q_PMOS_GDS` | `Package_TO_SOT_THT:TO-220-3_Vertical` | FQP27P06 high-side |
| Q3, Q4 | `Transistor_FET:Q_NMOS_GDS` | `Package_TO_SOT_THT:TO-220-3_Vertical` | FQP30N06L low-side |
| R3, R4 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100R P-gate series |
| R5, R6 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100R N-gate series |
| R7, R8 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k P-gate pull-up |
| R9, R10 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k N-gate pull-down |
| D1-D4 | `Device:D` | `Diode_THT:D_DO-41_SOD81_P10.16mm_Horizontal` | 1N5819 flyback |
| J3 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | motor out |

Pin-key notes verified against the installed libraries: `Q_NPN_BCE` pins are `B`/`C`/`E`;
`Q_PMOS_GDS`/`Q_NMOS_GDS` pins are `G`/`D`/`S`; `Device:D` pins are `A`/`K` (anode/cathode);
`Device:C_Polarized` pins are plain `1`/`2` (pin 1 is the positive terminal - route it to the
higher-potential net); `Conn_01x02`/`Conn_01x04` pins are plain `1..n`.

## Pin map

```
BASE_A: R1.2, Q5.B
BASE_B: R2.2, Q6.B
GND: J1.2, C1.2, J2.2, Q5.E, Q6.E, Q3.S, Q4.S, R9.2, R10.2, D2.A, D4.A
IN_A: J2.3, R1.1, R5.1
IN_B: J2.4, R2.1, R6.1
N_GATE_A: Q3.G, R5.2, R9.1
N_GATE_B: Q4.G, R6.2, R10.1
OUT_A: Q1.D, Q3.D, D1.A, D2.K, J3.1
OUT_B: Q2.D, Q4.D, D3.A, D4.K, J3.2
PDRV_A: Q5.C, R3.2
PDRV_B: Q6.C, R4.2
P_GATE_A: Q1.G, R3.1, R7.2
P_GATE_B: Q2.G, R4.1, R8.2
VMOT_12V: J1.1, C1.1, Q1.S, Q2.S, R7.1, R8.1, D1.K, D3.K
```

`J2.1` (the 3V3 reference pin on the logic header) is `nc`: nothing in this circuit actually
draws current from the 3.3 V rail, so leave it unconnected rather than giving it a single-pin
net - ERC flags a net with exactly one pin.

## Layout

The complete design JSON below builds with 0 issues and 0 ERC violations. Hand it to `build`
as-is, adapting values (voltage rail, MOSFET/diode part numbers) to the request:

```json
{
 "title": "Discrete 12 V Brushed DC Motor H-Bridge",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A3",
 "comments": [
  "Four-MOSFET H-bridge with NPN high-side gate pull-downs",
  "3.3 V complementary half-bridge control inputs",
  "External flyback diodes and bulk supply bypass"
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "12V POWER IN",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "VMOT_12V",
    "2": "GND"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C_Polarized",
   "value": "470uF",
   "footprint": "Capacitor_THT:CP_Radial_D8.0mm_P3.50mm",
   "pins": {
    "1": "VMOT_12V",
    "2": "GND"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x04",
   "value": "3V3 LOGIC IN",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
   "pins": {
    "1": "nc",
    "2": "GND",
    "3": "IN_A",
    "4": "IN_B"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "IN_A",
    "2": "BASE_A"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "IN_B",
    "2": "BASE_B"
   }
  },
  {
   "id": "Q5",
   "lib": "Transistor_BJT:Q_NPN_BCE",
   "value": "2N3904",
   "footprint": "Package_TO_SOT_THT:TO-92_Inline",
   "pins": {
    "B": "BASE_A",
    "C": "PDRV_A",
    "E": "GND"
   }
  },
  {
   "id": "Q6",
   "lib": "Transistor_BJT:Q_NPN_BCE",
   "value": "2N3904",
   "footprint": "Package_TO_SOT_THT:TO-92_Inline",
   "pins": {
    "B": "BASE_B",
    "C": "PDRV_B",
    "E": "GND"
   }
  },
  {
   "id": "Q1",
   "lib": "Transistor_FET:Q_PMOS_GDS",
   "value": "FQP27P06",
   "footprint": "Package_TO_SOT_THT:TO-220-3_Vertical",
   "pins": {
    "G": "P_GATE_A",
    "D": "OUT_A",
    "S": "VMOT_12V"
   }
  },
  {
   "id": "Q2",
   "lib": "Transistor_FET:Q_PMOS_GDS",
   "value": "FQP27P06",
   "footprint": "Package_TO_SOT_THT:TO-220-3_Vertical",
   "pins": {
    "G": "P_GATE_B",
    "D": "OUT_B",
    "S": "VMOT_12V"
   }
  },
  {
   "id": "Q3",
   "lib": "Transistor_FET:Q_NMOS_GDS",
   "value": "FQP30N06L",
   "footprint": "Package_TO_SOT_THT:TO-220-3_Vertical",
   "pins": {
    "G": "N_GATE_A",
    "D": "OUT_A",
    "S": "GND"
   }
  },
  {
   "id": "Q4",
   "lib": "Transistor_FET:Q_NMOS_GDS",
   "value": "FQP30N06L",
   "footprint": "Package_TO_SOT_THT:TO-220-3_Vertical",
   "pins": {
    "G": "N_GATE_B",
    "D": "OUT_B",
    "S": "GND"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "100",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "P_GATE_A",
    "2": "PDRV_A"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "100",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "P_GATE_B",
    "2": "PDRV_B"
   }
  },
  {
   "id": "R5",
   "lib": "Device:R",
   "value": "100",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "IN_A",
    "2": "N_GATE_A"
   }
  },
  {
   "id": "R6",
   "lib": "Device:R",
   "value": "100",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "IN_B",
    "2": "N_GATE_B"
   }
  },
  {
   "id": "R7",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VMOT_12V",
    "2": "P_GATE_A"
   }
  },
  {
   "id": "R8",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VMOT_12V",
    "2": "P_GATE_B"
   }
  },
  {
   "id": "R9",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "N_GATE_A",
    "2": "GND"
   }
  },
  {
   "id": "R10",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "N_GATE_B",
    "2": "GND"
   }
  },
  {
   "id": "D1",
   "lib": "Device:D",
   "value": "1N5819",
   "footprint": "Diode_THT:D_DO-41_SOD81_P10.16mm_Horizontal",
   "pins": {
    "K": "VMOT_12V",
    "A": "OUT_A"
   }
  },
  {
   "id": "D2",
   "lib": "Device:D",
   "value": "1N5819",
   "footprint": "Diode_THT:D_DO-41_SOD81_P10.16mm_Horizontal",
   "pins": {
    "K": "OUT_A",
    "A": "GND"
   }
  },
  {
   "id": "D3",
   "lib": "Device:D",
   "value": "1N5819",
   "footprint": "Diode_THT:D_DO-41_SOD81_P10.16mm_Horizontal",
   "pins": {
    "K": "VMOT_12V",
    "A": "OUT_B"
   }
  },
  {
   "id": "D4",
   "lib": "Device:D",
   "value": "1N5819",
   "footprint": "Diode_THT:D_DO-41_SOD81_P10.16mm_Horizontal",
   "pins": {
    "K": "OUT_B",
    "A": "GND"
   }
  },
  {
   "id": "J3",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "MOTOR",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "OUT_A",
    "2": "OUT_B"
   }
  }
 ],
 "flags": [
  "GND"
 ],
 "notes": [
  "Q1/Q3 form the left half bridge; Q2/Q4 form the right half bridge.",
  "The NPN collectors pull P-MOSFET gates low; 100k resistors turn them off.",
  "External Schottky diodes clamp inductive motor current at each output node."
 ],
 "layout": [
  {
   "title": "POWER",
   "note": "12 V motor supply with bulk bypass",
   "tree": {
    "row": [
     {
      "part": "J1"
     },
     {
      "part": "C1"
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "LOGIC DRIVE",
   "note": "NPN level shifters pull the P-MOSFET gates low",
   "tree": {
    "row": [
     {
      "part": "J2"
     },
     {
      "col": [
       {
        "part": "R1"
       },
       {
        "part": "R2"
       }
      ],
      "gap": 8
     },
     {
      "col": [
       {
        "part": "Q5"
       },
       {
        "part": "Q6"
       }
      ],
      "gap": 8
     }
    ],
    "gap": 10
   }
  },
  {
   "title": "H-BRIDGE",
   "note": "Q1/Q3 left half, Q2/Q4 right half, motor between the two output nodes",
   "tree": {
    "row": [
     {
      "col": [
       {
        "row": [
         {
          "col": [
           {
            "part": "R7"
           },
           {
            "part": "R3"
           }
          ],
          "gap": 4
         },
         {
          "part": "Q1"
         },
         {
          "part": "D1"
         }
        ],
        "gap": 5
       },
       {
        "row": [
         {
          "col": [
           {
            "part": "R5"
           },
           {
            "part": "R9"
           }
          ],
          "gap": 4
         },
         {
          "part": "Q3"
         },
         {
          "part": "D2"
         }
        ],
        "gap": 5
       }
      ],
      "gap": 6
     },
     {
      "part": "J3"
     },
     {
      "col": [
       {
        "row": [
         {
          "part": "D3"
         },
         {
          "part": "Q2"
         },
         {
          "col": [
           {
            "part": "R8"
           },
           {
            "part": "R4"
           }
          ],
          "gap": 4
         }
        ],
        "gap": 5
       },
       {
        "row": [
         {
          "part": "D4"
         },
         {
          "part": "Q4"
         },
         {
          "col": [
           {
            "part": "R6"
           },
           {
            "part": "R10"
           }
          ],
          "gap": 4
         }
        ],
        "gap": 5
       }
      ],
      "gap": 6
     }
    ],
    "gap": 12,
    "wrap": 400
   }
  }
 ]
}
```

## Checklist

- Every gate has both a pull network AND a series resistor: 100k P_GATE-to-VMOT_12V pull-up
  plus 100R gate series on the P-channel; 100k N_GATE-to-GND pull-down plus 100R gate series
  on the N-channel. Without the pulls, a floating driver output leaves a MOSFET half-on.
- One flyback diode per MOSFET (cathode to the rail it protects, anode at the switch node):
  D1/D3 clamp OUT_A/OUT_B high to VMOT_12V, D2/D4 clamp them low to GND. Do not substitute a
  single diode across the motor connector - each switching node needs its own clamp. 1N5819
  (Schottky) is chosen for low forward drop and fast recovery under PWM.
- 470 uF bulk cap (C1) directly across VMOT_12V/GND at the power connector, not inside the
  bridge block, absorbs motor commutation transients.
- Bridge symmetry: Q1/Q3 (left half) and Q2/Q4 (right half) are mirror images sharing
  VMOT_12V and GND rails only through power symbols, never a direct wire between the two
  cols - lay them out as two mirrored cols flanking the motor connector J3.
- The NPN level-shifter (Q5/Q6) inverts logic: IN_A high turns Q5 on, pulling PDRV_A (and the
  P-gate through R3) toward GND, which turns the P-channel ON. IN_A also feeds N_GATE_A
  directly through R5, turning the N-channel on the SAME side on too - this is the intended
  "one high-side and one low-side on together" diagonal-pair drive, never both sides of one
  leg simultaneously (add external dead-time / interlock logic before driving both inputs).
- Declare `"flags": ["GND"]` - ground on this sheet is only ever sourced from the J1/J2
  connectors, so ERC needs the PWR_FLAG. VMOT_12V does not need one: it is only a source net
  for passive gate networks and MOSFET sources, and the connector pin already qualifies as a
  driver for ERC here.
- Leave `J2` pin 1 (`VLOGIC_3V3` reference) as `"nc"`: nothing in the bridge consumes 3.3 V
  power directly, only logic-level voltage at IN_A/IN_B, so wiring it to a rail creates an
  isolated single-pin net that ERC flags.
- P-channel source, N-channel source: P-channel S ties to VMOT_12V (high-side), N-channel S
  ties to GND (low-side) - reversing S/D on a MOSFET symbol silently swaps which pin is the
  rail connection in the netlist even though the device is symmetric in the schematic body.
- Gate resistors go in series with the driver, never in series with the rail pull - R3/R4
  are the drive-side series resistors, R7/R8/R9/R10 the passive pull networks; each gate
  network is a col (pull + series resistor) on the gate side of that MOSFET's row.
- Motor connector J3 carries OUT_A/OUT_B only - never tie a motor terminal to GND or
  VMOT_12V directly, the bridge alone must switch it.

## Common mistakes

- Wiring both flyback diodes of one output node to the same rail (e.g. two cathodes to
  VMOT_12V) instead of one to each rail - the clamp only works in one current direction then.
- Omitting the 100k gate pull networks: without R7-R10 a MOSFET gate floats between switching
  events and can partially conduct, overheating the device.
- Swapping P-channel and N-channel part numbers between the high-side and low-side positions -
  FQP27P06 is P-channel (high-side, source to VMOT_12V) and FQP30N06L is N-channel (low-side,
  source to GND); swapping them means the "on" logic level is inverted from what the driver
  expects.
- Tying `J2` pin 1 to a `VLOGIC_3V3` net when nothing else uses it, producing a single-pin net
  ERC warning - mark it `nc` instead.
- Missing the PWR_FLAG on `GND`: with only connector-sourced ground, ERC reports GND as
  undriven without `"flags": ["GND"]`.
- Placing the two half-bridge cols in separate blocks instead of one mirrored H-BRIDGE block -
  they share the motor connector and must read as one symmetric unit, not two disconnected
  fragments joined by labels.
- Using `Device:C` (non-polarized) for the 470 uF bulk cap - an electrolytic that size needs
  `Device:C_Polarized` with pin 1 (positive) oriented to VMOT_12V.
- Driving the N-channel gate straight from IN_A/IN_B with no series resistor at all - R5/R6
  (100R) damp gate-drive ringing into the MOSFET's input capacitance.
