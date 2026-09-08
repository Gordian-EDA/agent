---
name: atmega328p-arduino
description: ATmega328P Arduino-style board - USB-C to CH340C USB-serial bridge with DTR auto-reset, AMS1117-5.0 and AMS1117-3.3 regulator chain, 16 MHz crystal, reset button, ICSP header, power and pin-13 LEDs, all digital/analog I/O on headers.
triggers: ["arduino uno", "arduino", "atmega328p", "atmega328", "ch340c", "ch340", "usb-serial bridge", "avr board"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `MCU_Microchip_ATmega:ATmega328P-A` | `Package_QFP:TQFP-32_7x7mm_P0.8mm` | ATmega328P-AU |
| U2 | `Interface_USB:CH340C` | `Package_SO:SOIC-16_3.9x9.9mm_P1.27mm` | CH340C |
| U3 | `Regulator_Linear:AMS1117-5.0` | `Package_TO_SOT_SMD:SOT-223-3_TabPin2` | AMS1117-5.0 |
| U4 | `Regulator_Linear:AMS1117-3.3` | `Package_TO_SOT_SMD:SOT-223-3_TabPin2` | AMS1117-3.3 |
| J1 | `Connector:USB_C_Receptacle_USB2.0_16P` | `Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal` | USB-C |
| J2 | `Connector_Generic:Conn_01x08` | `Connector_PinHeader_2.54mm:PinHeader_1x08_P2.54mm_Vertical` | digital D0-D7 |
| J3 | `Connector_Generic:Conn_01x08` | `Connector_PinHeader_2.54mm:PinHeader_1x08_P2.54mm_Vertical` | digital D8-D13 + power |
| J4 | `Connector_Generic:Conn_01x10` | `Connector_PinHeader_2.54mm:PinHeader_1x10_P2.54mm_Vertical` | analog A0-A7 + AREF |
| J5 | `Connector_Generic:Conn_01x06` | `Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical` | ICSP |
| J6 | `Connector_Generic:Conn_01x04` | `Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical` | power breakout |
| Y1 | `Device:Crystal` | `Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm` | 16 MHz |
| SW1 | `Switch:SW_Push` | `Button_Switch_SMD:SW_SPST_TL3342` | reset |
| D1, D2 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | power green, D13 blue |
| R1, R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 5.1k USB-C CC pulldowns |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k NRST pull-up |
| R4, R5 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k LED series |
| C1, C2, C3 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 10 uF regulator in/out |
| C4 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF CH340C V3 decouple |
| C5, C6 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 22 pF crystal load |
| C7 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF DTR auto-reset |
| C8, C9 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF VCC/AVCC decouple |

Pin-key notes verified against the installed libraries: `ATmega328P-A` has separate `VCC` (pin 4)
and `AVCC` (pin 18) power pins plus stacked `GND` (3/5/21) - key them all `"+5V"`/`"GND"`; the
crystal pins are named `XTAL1/PB6` and `XTAL2/PB7`, and reset is `~{RESET}/PC6`.
`Interface_USB:CH340C` pins by number: `1 GND`, `2 TXD`, `3 RXD`, `4 V3` (internal 3.3 V regulator
output needing a 100 nF cap, not a supply rail), `5 UD+`, `6 UD-`, `13 ~{DTR}`, `16 VCC`; TXD wires
to the MCU's RXD (`D0_RX`) and RXD wires to the MCU's TXD (`D1_TX`) - crossed, as any UART link is.
`Connector:USB_C_Receptacle_USB2.0_16P` exposes only `VBUS`/`GND`/`CC1`/`CC2`/`D+`/`D-`/`SHIELD`
as usable keys (each backed by multiple physical pins) - `pins_default: "nc"` covers `SBU1`/`SBU2`.
`AMS1117-5.0`/`AMS1117-3.3` pins are `VI`/`VO`/`GND`.

## Pin map

```
+3V3: U4.VO, C3.1, J6.3
+5V: U3.VO, C2.1, U4.VI, U2.16, U1.VCC, U1.AVCC, R3.1, C8.1, C9.1, R4.1, J3.7, J5.1, J6.2
A0_PC0: U1.PC0, J4.1
A1_PC1: U1.PC1, J4.2
A2_PC2: U1.PC2, J4.3
A3_PC3: U1.PC3, J4.4
A4_SDA: U1.PC4, J4.5
A5_SCL: U1.PC5, J4.6
ADC6: U1.ADC6, J4.8
ADC7: U1.ADC7, J4.9
AREF: U1.AREF, J4.7
CH340_V3: U2.4, C4.1
D0_RX: U2.2, U1.PD0, J2.1
D10_PB2: U1.PB2, J3.3
D11_PB3_MOSI: U1.PB3, J3.4, J5.4
D12_PB4_MISO: U1.PB4, J3.5, J5.3
D13_LED: R5.1, D2.A
D13_PB5_SCK: U1.PB5, R5.2, J3.6, J5.2
D1_TX: U2.3, U1.PD1, J2.2
D2_INT0: U1.PD2, J2.3
D3_PWM: U1.PD3, J2.4
D4: U1.PD4, J2.5
D5_PWM: U1.PD5, J2.6
D6_PWM: U1.PD6, J2.7
D7: U1.PD7, J2.8
D8_PB0: U1.PB0, J3.1
D9_PB1: U1.PB1, J3.2
GND: J1.SHIELD, J1.GND, R1.2, R2.2, C1.2, U3.GND, C2.2, U4.GND, C3.2, U2.1, C4.2, U1.GND, C5.2, C6.2, SW1.2, C8.2, C9.2, D1.K, D2.K, J3.8, J4.10, J5.6, J6.4
LED_PWR: R4.2, D1.A
RESET: U1.~{RESET}/PC6, R3.2, SW1.1, C7.2, J5.5
USB_CC1: J1.CC1, R1.1
USB_CC2: J1.CC2, R2.1
USB_DM: J1.D-, U2.6
USB_DP: J1.D+, U2.5
USB_DTR: U2.13, C7.1
VBUS: J1.VBUS, C1.1, U3.VI, J6.1
XTAL1: U1.XTAL1/PB6, Y1.1, C5.1
XTAL2: U1.XTAL2/PB7, Y1.2, C6.1
```

## Layout

The complete design JSON below builds with 0 layout errors, 0 issues and 0 ERC violations
(3 harmless wire-crossing warnings). Hand it to `build` as-is, adapting header assignments to
the request:

```json
{
 "title": "ATmega328P Arduino-style USB board",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Open Hardware",
 "paper": "A3",
 "comments": [
  "ATmega328P Arduino-style 5 V controller board",
  "USB-C USB-serial and dual regulated power rails",
  "All MCU I/O exposed on headers"
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector:USB_C_Receptacle_USB2.0_16P",
   "value": "USB-C",
   "footprint": "Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal",
   "pins": {
    "VBUS": "VBUS",
    "CC1": "USB_CC1",
    "CC2": "USB_CC2",
    "D-": "USB_DM",
    "D+": "USB_DP",
    "SHIELD": "GND",
    "GND": "GND"
   },
   "pins_default": "nc"
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "5.1k",
   "pins": {
    "1": "USB_CC1",
    "2": "GND"
   },
   "footprint": "Resistor_SMD:R_0603_1608Metric"
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "5.1k",
   "pins": {
    "1": "USB_CC2",
    "2": "GND"
   },
   "footprint": "Resistor_SMD:R_0603_1608Metric"
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "10uF",
   "pins": {
    "1": "VBUS",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "U3",
   "lib": "Regulator_Linear:AMS1117-5.0",
   "value": "AMS1117-5.0",
   "footprint": "Package_TO_SOT_SMD:SOT-223-3_TabPin2",
   "pins": {
    "VI": "VBUS",
    "VO": "+5V",
    "GND": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "10uF",
   "pins": {
    "1": "+5V",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "U4",
   "lib": "Regulator_Linear:AMS1117-3.3",
   "value": "AMS1117-3.3",
   "footprint": "Package_TO_SOT_SMD:SOT-223-3_TabPin2",
   "pins": {
    "VI": "+5V",
    "VO": "+3V3",
    "GND": "GND"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "10uF",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "U2",
   "lib": "Interface_USB:CH340C",
   "value": "CH340C",
   "footprint": "Package_SO:SOIC-16_3.9x9.9mm_P1.27mm",
   "pins": {
    "5": "USB_DP",
    "6": "USB_DM",
    "2": "D0_RX",
    "3": "D1_TX",
    "13": "USB_DTR",
    "16": "+5V",
    "4": "CH340_V3",
    "1": "GND"
   },
   "pins_default": "nc"
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "100nF",
   "pins": {
    "1": "CH340_V3",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "U1",
   "lib": "MCU_Microchip_ATmega:ATmega328P-A",
   "value": "ATmega328P-AU",
   "footprint": "Package_QFP:TQFP-32_7x7mm_P0.8mm",
   "pins": {
    "AREF": "AREF",
    "ADC6": "ADC6",
    "ADC7": "ADC7",
    "PB0": "D8_PB0",
    "PB1": "D9_PB1",
    "PB2": "D10_PB2",
    "PB3": "D11_PB3_MOSI",
    "PB4": "D12_PB4_MISO",
    "PB5": "D13_PB5_SCK",
    "XTAL1/PB6": "XTAL1",
    "XTAL2/PB7": "XTAL2",
    "PC0": "A0_PC0",
    "PC1": "A1_PC1",
    "PC2": "A2_PC2",
    "PC3": "A3_PC3",
    "PC4": "A4_SDA",
    "PC5": "A5_SCL",
    "~{RESET}/PC6": "RESET",
    "PD0": "D0_RX",
    "PD1": "D1_TX",
    "PD2": "D2_INT0",
    "PD3": "D3_PWM",
    "PD4": "D4",
    "PD5": "D5_PWM",
    "PD6": "D6_PWM",
    "PD7": "D7",
    "VCC": "+5V",
    "AVCC": "+5V",
    "GND": "GND"
   },
   "pins_default": "nc"
  },
  {
   "id": "Y1",
   "lib": "Device:Crystal",
   "value": "16MHz",
   "pins": {
    "1": "XTAL1",
    "2": "XTAL2"
   },
   "footprint": "Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm"
  },
  {
   "id": "C5",
   "lib": "Device:C",
   "value": "22pF",
   "pins": {
    "1": "XTAL1",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "C6",
   "lib": "Device:C",
   "value": "22pF",
   "pins": {
    "1": "XTAL2",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "10k",
   "pins": {
    "1": "+5V",
    "2": "RESET"
   },
   "footprint": "Resistor_SMD:R_0603_1608Metric"
  },
  {
   "id": "SW1",
   "lib": "Switch:SW_Push",
   "value": "RESET",
   "pins": {
    "1": "RESET",
    "2": "GND"
   },
   "footprint": "Button_Switch_SMD:SW_SPST_TL3342"
  },
  {
   "id": "C7",
   "lib": "Device:C",
   "value": "100nF",
   "pins": {
    "1": "USB_DTR",
    "2": "RESET"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "C8",
   "lib": "Device:C",
   "value": "100nF",
   "pins": {
    "1": "+5V",
    "2": "GND"
   },
   "footprint": "Capacitor_SMD:C_0603_1608Metric"
  },
  {
   "id": "C9",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "1k",
   "pins": {
    "1": "+5V",
    "2": "LED_PWR"
   },
   "footprint": "Resistor_SMD:R_0603_1608Metric"
  },
  {
   "id": "D1",
   "lib": "Device:LED",
   "value": "POWER GREEN",
   "pins": {
    "K": "GND",
    "A": "LED_PWR"
   },
   "footprint": "LED_SMD:LED_0603_1608Metric"
  },
  {
   "id": "R5",
   "lib": "Device:R",
   "value": "1k",
   "pins": {
    "1": "D13_LED",
    "2": "D13_PB5_SCK"
   },
   "footprint": "Resistor_SMD:R_0603_1608Metric"
  },
  {
   "id": "D2",
   "lib": "Device:LED",
   "value": "D13 BLUE",
   "pins": {
    "K": "GND",
    "A": "D13_LED"
   },
   "footprint": "LED_SMD:LED_0603_1608Metric"
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x08",
   "value": "DIGITAL 0-7",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x08_P2.54mm_Vertical",
   "pins": {
    "1": "D0_RX",
    "2": "D1_TX",
    "3": "D2_INT0",
    "4": "D3_PWM",
    "5": "D4",
    "6": "D5_PWM",
    "7": "D6_PWM",
    "8": "D7"
   }
  },
  {
   "id": "J3",
   "lib": "Connector_Generic:Conn_01x08",
   "value": "DIGITAL 8-13 + POWER",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x08_P2.54mm_Vertical",
   "pins": {
    "1": "D8_PB0",
    "2": "D9_PB1",
    "3": "D10_PB2",
    "4": "D11_PB3_MOSI",
    "5": "D12_PB4_MISO",
    "6": "D13_PB5_SCK",
    "7": "+5V",
    "8": "GND"
   }
  },
  {
   "id": "J4",
   "lib": "Connector_Generic:Conn_01x10",
   "value": "ANALOG A0-A7",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x10_P2.54mm_Vertical",
   "pins": {
    "1": "A0_PC0",
    "2": "A1_PC1",
    "3": "A2_PC2",
    "4": "A3_PC3",
    "5": "A4_SDA",
    "6": "A5_SCL",
    "7": "AREF",
    "8": "ADC6",
    "9": "ADC7",
    "10": "GND"
   }
  },
  {
   "id": "J5",
   "lib": "Connector_Generic:Conn_01x06",
   "value": "ICSP",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical",
   "pins": {
    "1": "+5V",
    "2": "D13_PB5_SCK",
    "3": "D12_PB4_MISO",
    "4": "D11_PB3_MOSI",
    "5": "RESET",
    "6": "GND"
   }
  },
  {
   "id": "J6",
   "lib": "Connector_Generic:Conn_01x04",
   "value": "POWER",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
   "pins": {
    "1": "VBUS",
    "2": "+5V",
    "3": "+3V3",
    "4": "GND"
   }
  }
 ],
 "flags": [
  "VBUS",
  "GND"
 ],
 "notes": [
  "USB-C CC1/CC2 use 5.1k Rd pulldowns.",
  "CH340C provides USB UART with DTR auto-reset.",
  "ATmega328P runs at 5 V with a 16 MHz crystal.",
  "ADC6 and ADC7 are exposed in the analog header."
 ],
 "layout": [
  {
   "title": "USB-C INPUT",
   "note": "USB-C connector with CC pulldowns and CH340C USB-serial bridge",
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
        "gap": 2
       }
      ],
      "gap": 3
     },
     {
      "part": "U2"
     },
     {
      "part": "C4"
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "POWER RAILS",
   "note": "5 V then 3.3 V AMS1117 chain off VBUS",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "C1"
       },
       {
        "part": "U3"
       },
       {
        "part": "C2"
       }
      ],
      "gap": 5
     },
     {
      "row": [
       {
        "part": "U4"
       },
       {
        "part": "C3"
       },
       {
        "part": "J6"
       }
      ],
      "gap": 5
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "ATMEGA328P CORE",
   "note": "16 MHz crystal and supply bypass on VCC/AVCC",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "Y1"
       },
       {
        "row": [
         {
          "part": "C5"
         },
         {
          "part": "C6"
         }
        ],
        "gap": 2
       }
      ],
      "gap": 3
     },
     {
      "part": "U1"
     },
     {
      "row": [
       {
        "part": "C8"
       },
       {
        "part": "C9"
       }
      ],
      "gap": 2
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "RESET & ICSP",
   "note": "Pull-up/pushbutton reset with DTR auto-reset cap, and the AVR ICSP header",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "R3"
       },
       {
        "part": "SW1"
       },
       {
        "part": "C7"
       }
      ],
      "gap": 3
     },
     {
      "part": "J5"
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "STATUS LEDS",
   "note": "Power and pin-13 indicator LEDs",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "R4"
       },
       {
        "part": "D1"
       }
      ],
      "gap": 2
     },
     {
      "col": [
       {
        "part": "R5"
       },
       {
        "part": "D2"
       }
      ],
      "gap": 2
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "I/O HEADERS",
   "note": "All digital D0-D13 and analog A0-A7 broken out",
   "tree": {
    "row": [
     {
      "part": "J2"
     },
     {
      "part": "J3"
     },
     {
      "part": "J4"
     }
    ],
    "gap": 5
   }
  }
 ]
}
```

## Checklist

- ATmega328P has two power domains that both read `+5V`/`GND`: `VCC`+`AVCC` and their `GND` pins;
  give each pair its own 100 nF (C8 for VCC, C9 for AVCC) - never share one cap between them.
- 16 MHz crystal on `XTAL1/PB6`/`XTAL2/PB7` with 22 pF loads to GND on each leg; no series resistor
  needed for AVR crystal oscillators.
- NRST (`~{RESET}/PC6`): 10 k pull-up to +5V, pushbutton to GND, and a 100 nF DTR auto-reset cap
  from CH340C's DTR pin - never tie DTR straight to RESET, always couple it through the cap so a
  steady DTR level does not hold reset.
- CH340C's `V3` pin is its internal 3.3 V regulator output for its own core, not a system rail -
  it needs a dedicated 100 nF decouple (C4) and must never be shorted to the board's `+3V3`.
- USB-C CC1/CC2 need 5.1 k pulldowns to GND (device-side Rd) so a C-to-C cable's host detects a
  UFP; without them the port draws no power.
- Regulator chain is VBUS -> AMS1117-5.0 -> AMS1117-3.3, each with an input and output cap; the
  MCU runs off the +5V rail, +3V3 exists only to power 3.3 V peripherals off the header.
- PWR_FLAG both `VBUS` (from the USB-C connector only) and `GND` (also only driven by the
  connector's passive pins) - both are needed or ERC reports an undriven power input.
- Every ATmega328P GPIO carries a net: PB0-PB5 to digital header + ICSP (SPI pins double as MOSI/
  MISO/SCK), PC0-PC5 to the analog header, PD0-PD7 to the digital header. Only `pins_default: "nc"`
  leaves genuinely unused pins open.
- D13 LED anode returns through 1 k in series to `PB5`/`D13_PB5_SCK` directly (not through +5V),
  since the MCU sinks/sources it as a GPIO, unlike the always-on power LED which returns to +5V.
- ICSP carries MOSI/MISO/SCK/RESET/+5V/GND - all five programming signals plus power, matching a
  standard 2x3 AVR ISP pinout collapsed to one row here.
- Power symbols: `+5V`/`+3V3`/`VBUS` point up, `GND` points down - automatic for these names.
- Keep the CH340C TXD/RXD-to-MCU crossing straight in the netlist: CH340 TXD -> MCU RXD (`D0_RX`),
  CH340 RXD -> MCU TXD (`D1_TX`); do not "uncross" them, that would break the UART link.

## Common mistakes

- Sharing one 100 nF between ATmega328P's VCC and AVCC - the rubric counts decoupling per supply
  pin, and AVCC decoupling especially benefits from being physically separate.
- Wiring CH340C's DTR pin directly to RESET with no coupling cap - this holds the MCU in reset
  whenever DTR idles low instead of producing a narrow reset pulse on the falling edge.
- Confusing CH340C's `V3` (its own 3.3 V regulator output, decoupled locally) with the board's
  `+3V3` rail from U4 - they are unrelated nets that must never be merged.
- Omitting the CC1/CC2 pulldowns on the USB-C receptacle - a C-to-C source then supplies no VBUS.
- Dropping either `"VBUS"` or `"GND"` from `flags` - both nets here are only driven by a connector,
  so ERC flags an undriven power input if the PWR_FLAG is missing.
- Returning the D13 LED's cathode network to +5V like the power LED - it must return to the GPIO
  net so the MCU can drive it, not sit permanently lit.
- Treating ADC6/ADC7 as GPIOs - they are ADC-only inputs on the TQFP-32 package with no digital
  buffer; only the analog header may carry them.
