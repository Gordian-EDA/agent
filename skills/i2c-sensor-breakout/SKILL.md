---
name: i2c-sensor-breakout
description: 3.3V I2C temperature-sensor breakout - TMP102 with 4.7k SDA/SCL pull-ups, ALERT pull-up and an ADD0 address strap
triggers: ["i2c sensor", "temperature sensor breakout", "tmp102", "i2c temperature", "sda scl pull-up", "address strap", "i2c breakout"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `Sensor_Temperature:TMP102xxDRL` | `Package_TO_SOT_SMD:SOT-563` | TMP102 |
| J1 | `Connector_Generic:Conn_01x04` | `Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical` | I2C (VCC/GND/SDA/SCL) |
| C1 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF (VCC decouple) |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1uF (VCC bulk) |
| R1, R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 4.7k (SDA/SCL pull-ups) |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k (ALERT pull-up) |
| JP1 | `Jumper:Jumper_2_Bridged` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | ADD0 SEL |

Pin-key notes verified against the installed library: `Sensor_Temperature:TMP102xxDRL` pins are
`SCL`, `GND`, `ALERT`(open-collector), `ADD0`(input strap - GND/V+/SDA/SCL select the four bus
addresses 0x48-0x4B), `V+`, `SDA`(bidirectional). `Jumper:Jumper_2_Bridged` pins are `A`/`B`.

## Pin map

```
ADD0: U1.ADD0, JP1.A
ALERT: U1.ALERT, R3.2
GND: U1.GND, C1.2, C2.2, JP1.B, J1.2
SCL: U1.SCL, R2.2, J1.4
SDA: U1.SDA, R1.2, J1.3
VCC: U1.V+, C1.1, C2.1, R1.1, R2.1, R3.1, J1.1
```

## Layout

Builds with 0 issues and 0 ERC violations:

```json
{
 "title": "TMP102 I2C Temperature Breakout",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "TMP102 3.3V I2C digital temperature sensor",
  "4.7k SDA/SCL pull-ups, ADD0 strapped to GND (0x48)"
 ],
 "parts": [
  {
   "id": "U1",
   "lib": "Sensor_Temperature:TMP102xxDRL",
   "value": "TMP102",
   "footprint": "Package_TO_SOT_SMD:SOT-563",
   "pins": {
    "V+": "VCC",
    "GND": "GND",
    "SDA": "SDA",
    "SCL": "SCL",
    "ADD0": "ADD0",
    "ALERT": "ALERT"
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
   "id": "R1",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VCC",
    "2": "SDA"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VCC",
    "2": "SCL"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VCC",
    "2": "ALERT"
   }
  },
  {
   "id": "JP1",
   "lib": "Jumper:Jumper_2_Bridged",
   "value": "ADD0 SEL",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "A": "ADD0",
    "B": "GND"
   }
  },
  {
   "id": "J1",
   "lib": "Connector_Generic:Conn_01x04",
   "value": "I2C",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
   "pins": {
    "1": "VCC",
    "2": "GND",
    "3": "SDA",
    "4": "SCL"
   }
  }
 ],
 "flags": [
  "VCC",
  "GND"
 ],
 "notes": [
  "ADD0 strapped to GND through JP1 sets the 0x48 address; move the jumper to VCC/SDA/SCL for the other three addresses.",
  "ALERT is pulled up to VCC through R3 for an open-drain comparator/interrupt output; leave unconnected downstream if unused."
 ],
 "layout": [
  {
   "title": "SUPPLY AND BUS",
   "note": "Connector, local decoupling and the SDA/SCL pull-ups",
   "tree": {
    "row": [
     {
      "part": "J1"
     },
     {
      "row": [
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
      "col": [
       {
        "part": "R1"
       },
       {
        "part": "R2"
       }
      ],
      "gap": 5
     }
    ],
    "gap": 12
   }
  },
  {
   "title": "SENSOR",
   "note": "TMP102 with an ALERT pull-up and the ADD0 address strap",
   "tree": {
    "row": [
     {
      "part": "U1"
     },
     {
      "part": "R3"
     },
     {
      "part": "JP1"
     }
    ],
    "gap": 10
   }
  }
 ]
}
```

## Checklist

- Confirm the sensor's real pin names before writing the netlist (`ADD0` here, not `A0`/`AS` -
  those belong to other sensor families like LM75B).
- SDA and SCL each need their own pull-up (4.7k for a short on-board bus at 3.3V); never share
  one resistor between both lines.
- Local decoupling at the sensor: 100nF ceramic plus a 1uF bulk cap on V+/GND.
- ADD0 must be strapped to a DEFINITE level (GND/V+/SDA/SCL, not left floating) - a jumper makes
  the choice visibly intentional and documents the resulting 7-bit address.
- ALERT is open-collector: pull it up if the design exposes it, or explicitly leave `nc` if
  unused - never wire it as if it were push-pull.
- PWR_FLAG on both VCC and GND: this connector-fed design has no other source for either rail.
- Connector pinout must expose all four required signals (VCC, GND, SDA, SCL) - never omit one
  to save a pin.
- Keep two blocks (supply/bus vs sensor), not one block per part - a 5.1k pull-up or a jumper is
  never its own block.

## Common mistakes

- Only one pull-up (SDA and SCL tied together, or SCL left floating) - I2C requires both.
- ADD0 left unconnected: the symbol's ADD0 is a real input pin, and a floating address strap
  will look ambiguous/undefined to a human reviewer even if the silicon has an internal bias.
- Confusing sensor supply pin names: TMP102 uses `V+`, not `VDD` or `VCC` - map by the verified
  pin name, then assign your own net name (`VCC`) to it.
- Putting the address jumper and the ALERT pull-up in the same `col` when they hang off
  different, unrelated nodes (ADD0 vs ALERT) - each shunt needs a `col`/`row` scoped to its own
  series part, or their power-symbol stubs can render close enough to look shorted together.
- Two shunt caps (VCC/GND decoupling) stacked in a `col` instead of a `row` can place one part's
  GND symbol directly beneath the other's VCC symbol - use a `row` for parallel decoupling caps.
- Declaring `GND` in `flags` on a very compact single-row layout can fail to find room for the
  PWR_FLAG placement (silent ERC "power pin not driven"); give the block extra `gap` (8-12) or
  split into two blocks so the connector's GND stub has clear space around it.
