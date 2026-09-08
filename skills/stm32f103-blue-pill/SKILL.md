---
name: stm32f103-blue-pill
description: STM32F103C8T6 "Blue Pill" board - AMS1117 3.3 V LDO, micro-USB device with 22R series and 1k5 D+ pull-up, 8 MHz HSE and 32.768 kHz LSE crystals, BOOT0/BOOT1 straps, reset button, SWD header and two 20-pin GPIO headers.
triggers: ["blue pill", "bluepill", "stm32f103", "stm32f103c8t6", "stm32f1", "stm32 development board", "cortex-m3 board"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `MCU_ST_STM32F1:STM32F103C8Tx` | `Package_QFP:LQFP-48_7x7mm_P0.5mm` | STM32F103C8T6 |
| U2 | `Regulator_Linear:AMS1117-3.3` | `Package_TO_SOT_SMD:SOT-223-3_TabPin2` | AMS1117-3.3 |
| J5 | `Connector:USB_B_Micro` | `Connector_USB:USB_Micro-B_Molex-105017-0001` | USB micro-B |
| F1 | `Device:Polyfuse` | `Fuse:Fuse_1206_3216Metric` | 500 mA PTC |
| J3 | `Connector_Generic:Conn_01x02` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | 5 V input |
| J1, J2 | `Connector_Generic:Conn_01x20` | `Connector_PinHeader_2.54mm:PinHeader_1x20_P2.54mm_Vertical` | GPIO headers |
| J4 | `Connector_Generic:Conn_02x05_Odd_Even` | `Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm_Vertical` | Cortex 10-pin SWD |
| Y1 | `Device:Crystal` | `Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm` | 8 MHz HSE |
| Y2 | `Device:Crystal` | `Crystal:Crystal_SMD_3215-2Pin_3.2x1.5mm` | 32.768 kHz LSE |
| SW1 | `Switch:SW_Push` | `Button_Switch_SMD:SW_SPST_TL3342` | reset |
| JP1, JP2 | `Connector_Generic:Conn_01x03` | `Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical` | BOOT0 / BOOT1 3-pin strap (+3V3 / signal / GND) |
| JP3 | `Jumper:SolderJumper_2_Bridged` | `Jumper:SolderJumper-2_P1.3mm_Bridged_RoundedPad1.0x1.5mm` | VBAT_RTC to +3V3, bridged |
| D1, D2 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | PC13 user, power |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k NRST pull-up |
| R2, R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k BOOT0 / BOOT1 pull-down |
| R4, R5 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k LED series |
| R6 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k5 USB D+ pull-up |
| R7, R8 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 22R USB series |
| C1, C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 22 pF HSE load |
| C10, C11 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 12 pF LSE load |
| C3 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 4.7 uF +5V bulk |
| C4 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 10 uF +3V3 bulk |
| C5-C8 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF per VDD / VDDA pin |
| C9 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF on VBAT_RTC, at the header pin |
| C12 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF NRST |

Pin-key notes verified against the installed libraries: `STM32F103C8Tx` has stacked `VSS` pins
23/35/47 and `VDD` pins 24/36/48, so the name keys `"VSS"`/`"VDD"` cover all of them at once;
it has **no BOOT1 pin** - BOOT1 is PB2 (pin 20). `Connector:USB_B_Micro` pins are
`VBUS D- D+ ID GND SH`. `Switch:SW_Push` pins are
`1`/`2`. `AMS1117-3.3` pins are `GND`/`VO`/`VI`. `SolderJumper_2_Bridged` pins are `1`/`2`.

## Pin map

```
+3V3: U1.VDD, U1.VDDA, U2.VO, R6.1, C4.1, C5.1, C6.1, C7.1, C8.1, R4.1, R5.1, R1.1, JP1.1, JP2.1, JP3.1, J4.1, J1.20, J2.18
+5V: U2.VI, F1.2, J3.1, C3.1, J1.18
BOOT0: U1.BOOT0, JP1.2, R2.1
BOOT1: U1.PB2, JP2.2, R3.1
GND: U1.VSS, U1.VSSA, U2.GND, J5.GND, J5.SH, J3.2, C3.2, C4.2, C5.2, C6.2, C7.2, C8.2, C9.2, D2.1, C1.2, C2.2, C10.2, C11.2, C12.2, SW1.2, JP1.3, R2.2, JP2.3, R3.2, J4.3, J4.5, J4.9, J1.19, J2.19, J2.20
LED_A: R4.2, D1.2
LED_K: U1.PC13, D1.1, J2.15
NRST: U1.NRST, R1.2, C12.1, SW1.1, J4.10, J2.17
OSC32_IN: U1.PC14, Y2.1, C10.1, J2.14
OSC32_OUT: U1.PC15, Y2.2, C11.1, J2.13
OSC_IN: U1.PD0, Y1.1, C1.1
OSC_OUT: U1.PD1, Y1.2, C2.1
PA0: U1.PA0, J2.12
PA1: U1.PA1, J2.11
PA10: U1.PA10, J1.7
PA15: U1.PA15, J1.10
PA2: U1.PA2, J2.10
PA3: U1.PA3, J2.9
PA4: U1.PA4, J2.8
PA5: U1.PA5, J2.7
PA6: U1.PA6, J2.6
PA7: U1.PA7, J2.5
PA8: U1.PA8, J1.5
PA9: U1.PA9, J1.6
PB0: U1.PB0, J2.4
PB1: U1.PB1, J2.3
PB10: U1.PB10, J2.2
PB11: U1.PB11, J2.1
PB12: U1.PB12, J1.1
PB13: U1.PB13, J1.2
PB14: U1.PB14, J1.3
PB15: U1.PB15, J1.4
PB3: U1.PB3, J4.6, J1.11
PB4: U1.PB4, J1.12
PB5: U1.PB5, J1.13
PB6: U1.PB6, J1.14
PB7: U1.PB7, J1.15
PB8: U1.PB8, J1.16
PB9: U1.PB9, J1.17
PWR_LED_A: R5.2, D2.2
SWCLK: U1.PA14, J4.4
SWDIO: U1.PA13, J4.2
USB_DM: U1.PA11, R7.2, J1.8
USB_DM_CON: J5.D-, R7.1
USB_DP: U1.PA12, R8.2, R6.2, J1.9
USB_DP_CON: J5.D+, R8.1
VBAT_RTC: U1.VBAT, C9.1, JP3.2, J2.16
VBUS: J5.VBUS, F1.1
```

## Layout

The design JSON below builds with 0 issues, 0 warnings and 0 ERC violations and packs onto A3
in eight labelled blocks. Hand it to `build` as-is, adapting values and header assignments to
the request:

```json
{
 "title": "STM32F103C8T6 Blue Pill",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A3",
 "comments": [
  "STM32F103C8T6 development board",
  "USB device, SWD, HSE + LSE, boot straps"
 ],
 "parts": [
  {
   "id": "U1",
   "lib": "MCU_ST_STM32F1:STM32F103C8Tx",
   "value": "STM32F103C8T6",
   "footprint": "Package_QFP:LQFP-48_7x7mm_P0.5mm",
   "pins": {
    "VBAT": "VBAT_RTC",
    "VDD": "+3V3",
    "VDDA": "+3V3",
    "VSS": "GND",
    "VSSA": "GND",
    "PA0": "PA0",
    "PA1": "PA1",
    "PA2": "PA2",
    "PA3": "PA3",
    "PA4": "PA4",
    "PA5": "PA5",
    "PA6": "PA6",
    "PA7": "PA7",
    "PA8": "PA8",
    "PA9": "PA9",
    "PA10": "PA10",
    "PA11": "USB_DM",
    "PA12": "USB_DP",
    "PA13": "SWDIO",
    "PA14": "SWCLK",
    "PA15": "PA15",
    "PB0": "PB0",
    "PB1": "PB1",
    "PB2": "BOOT1",
    "PB3": "PB3",
    "PB4": "PB4",
    "PB5": "PB5",
    "PB6": "PB6",
    "PB7": "PB7",
    "PB8": "PB8",
    "PB9": "PB9",
    "PB10": "PB10",
    "PB11": "PB11",
    "PB12": "PB12",
    "PB13": "PB13",
    "PB14": "PB14",
    "PB15": "PB15",
    "PC13": "LED_K",
    "PC14": "OSC32_IN",
    "PC15": "OSC32_OUT",
    "PD0": "OSC_IN",
    "PD1": "OSC_OUT",
    "NRST": "NRST",
    "BOOT0": "BOOT0"
   }
  },
  {
   "id": "U2",
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
   "id": "J5",
   "lib": "Connector:USB_B_Micro",
   "value": "USB MICRO-B",
   "footprint": "Connector_USB:USB_Micro-B_Molex-105017-0001",
   "pins": {
    "VBUS": "VBUS",
    "D-": "USB_DM_CON",
    "D+": "USB_DP_CON",
    "ID": "nc",
    "GND": "GND",
    "SH": "GND"
   }
  },
  {
   "id": "F1",
   "lib": "Device:Polyfuse",
   "value": "500mA PTC",
   "footprint": "Fuse:Fuse_1206_3216Metric",
   "pins": {
    "1": "VBUS",
    "2": "+5V"
   }
  },
  {
   "id": "J3",
   "lib": "Connector_Generic:Conn_01x02",
   "value": "5V IN",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "R7",
   "lib": "Device:R",
   "value": "22R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "USB_DM_CON",
    "2": "USB_DM"
   }
  },
  {
   "id": "R8",
   "lib": "Device:R",
   "value": "22R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "USB_DP_CON",
    "2": "USB_DP"
   }
  },
  {
   "id": "R6",
   "lib": "Device:R",
   "value": "1k5",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "USB_DP"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "4.7uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C5",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C6",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C7",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C8",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C9",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VBAT_RTC",
    "2": "GND"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "LED_A"
   }
  },
  {
   "id": "D1",
   "lib": "Device:LED",
   "value": "PC13 USER",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "1": "LED_K",
    "2": "LED_A"
   }
  },
  {
   "id": "R5",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "PWR_LED_A"
   }
  },
  {
   "id": "D2",
   "lib": "Device:LED",
   "value": "PWR RED",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "1": "GND",
    "2": "PWR_LED_A"
   }
  },
  {
   "id": "Y1",
   "lib": "Device:Crystal",
   "value": "8MHz HSE",
   "footprint": "Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm",
   "pins": {
    "1": "OSC_IN",
    "2": "OSC_OUT"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "22pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "OSC_IN",
    "2": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "22pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "OSC_OUT",
    "2": "GND"
   }
  },
  {
   "id": "Y2",
   "lib": "Device:Crystal",
   "value": "32.768kHz LSE",
   "footprint": "Crystal:Crystal_SMD_3215-2Pin_3.2x1.5mm",
   "pins": {
    "1": "OSC32_IN",
    "2": "OSC32_OUT"
   }
  },
  {
   "id": "C10",
   "lib": "Device:C",
   "value": "12pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "OSC32_IN",
    "2": "GND"
   }
  },
  {
   "id": "C11",
   "lib": "Device:C",
   "value": "12pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "OSC32_OUT",
    "2": "GND"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "NRST"
   }
  },
  {
   "id": "C12",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "NRST",
    "2": "GND"
   }
  },
  {
   "id": "SW1",
   "lib": "Switch:SW_Push",
   "value": "RESET",
   "footprint": "Button_Switch_SMD:SW_SPST_TL3342",
   "pins": {
    "1": "NRST",
    "2": "GND"
   }
  },
  {
   "id": "JP1",
   "lib": "Connector_Generic:Conn_01x03",
   "value": "BOOT0 STRAP",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "BOOT0",
    "3": "GND"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "BOOT0",
    "2": "GND"
   }
  },
  {
   "id": "JP2",
   "lib": "Connector_Generic:Conn_01x03",
   "value": "BOOT1 STRAP",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "BOOT1",
    "3": "GND"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "BOOT1",
    "2": "GND"
   }
  },
  {
   "id": "J4",
   "lib": "Connector_Generic:Conn_02x05_Odd_Even",
   "value": "SWD 10-PIN",
   "footprint": "Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "SWDIO",
    "3": "GND",
    "4": "SWCLK",
    "5": "GND",
    "6": "PB3",
    "7": "nc",
    "8": "nc",
    "9": "GND",
    "10": "NRST"
   }
  },
  {
   "id": "J1",
   "lib": "Connector_Generic:Conn_01x20",
   "value": "HEADER B/A",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x20_P2.54mm_Vertical",
   "pins": {
    "1": "PB12",
    "2": "PB13",
    "3": "PB14",
    "4": "PB15",
    "5": "PA8",
    "6": "PA9",
    "7": "PA10",
    "8": "USB_DM",
    "9": "USB_DP",
    "10": "PA15",
    "11": "PB3",
    "12": "PB4",
    "13": "PB5",
    "14": "PB6",
    "15": "PB7",
    "16": "PB8",
    "17": "PB9",
    "18": "+5V",
    "19": "GND",
    "20": "+3V3"
   }
  },
  {
   "id": "J2",
   "lib": "Connector_Generic:Conn_01x20",
   "value": "HEADER A/C",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x20_P2.54mm_Vertical",
   "pins": {
    "1": "PB11",
    "2": "PB10",
    "3": "PB1",
    "4": "PB0",
    "5": "PA7",
    "6": "PA6",
    "7": "PA5",
    "8": "PA4",
    "9": "PA3",
    "10": "PA2",
    "11": "PA1",
    "12": "PA0",
    "13": "OSC32_OUT",
    "14": "OSC32_IN",
    "15": "LED_K",
    "16": "VBAT_RTC",
    "17": "NRST",
    "18": "+3V3",
    "19": "GND",
    "20": "GND"
   }
  },
  {
   "id": "JP3",
   "lib": "Jumper:SolderJumper_2_Bridged",
   "value": "VBAT=3V3",
   "footprint": "Jumper:SolderJumper-2_P1.3mm_Bridged_RoundedPad1.0x1.5mm",
   "pins": {
    "1": "+3V3",
    "2": "VBAT_RTC"
   }
  }
 ],
 "flags": [
  "+5V",
  "VBAT_RTC"
 ],
 "notes": [
  "USB VBUS is fused to +5V and regulated to 3.3 V by the AMS1117.",
  "R6 (1k5) on D+ signals a full-speed USB device; R7/R8 are 22R series terminations.",
  "JP1/JP2 are 3-pin straps: BOOT0/BOOT1 idle low through 10k, jumper to 3.3 V to boot the loader.",
  "VBAT_RTC is the RTC backup rail: JP3 bridges it to +3V3, C9 decouples it and J2-16 brings it out."
 ],
 "layout": [
  {
   "title": "POWER",
   "note": "USB VBUS fused to +5V and regulated to 3.3 V by the AMS1117",
   "tree": {
    "row": [
     {
      "part": "J3"
     },
     {
      "part": "F1"
     },
     {
      "col": [
       {
        "part": "C3"
       }
      ],
      "gap": 4
     },
     {
      "part": "U2"
     },
     {
      "col": [
       {
        "part": "C4"
       }
      ],
      "gap": 4
     }
    ],
    "gap": 4,
    "wrap": 400
   }
  },
  {
   "title": "USB DEVICE",
   "note": "22R series terminations and the 1k5 full-speed D+ pull-up",
   "tree": {
    "row": [
     {
      "part": "J5"
     },
     {
      "col": [
       {
        "row": [
         {
          "part": "R8"
         },
         {
          "col": [
           {
            "part": "R6"
           }
          ]
         }
        ],
        "gap": 3
       },
       {
        "row": [
         {
          "part": "R7"
         }
        ]
       }
      ],
      "gap": 0
     }
    ],
    "gap": 3
   }
  },
  {
   "title": "MCU CORE",
   "note": "One 100nF per VDD pin plus VDDA; the PC13 user LED and the power LED",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "C5"
       },
       {
        "part": "C6"
       },
       {
        "part": "C7"
       },
       {
        "part": "C8"
       }
      ],
      "gap": 4
     },
     {
      "row": [
       {
        "col": [
         {
          "part": "R4"
         },
         {
          "part": "D1"
         },
         {
          "part": "R5"
         },
         {
          "part": "D2"
         }
        ],
        "gap": 4
       },
       {
        "part": "U1"
       }
      ],
      "gap": 3,
      "align": "start",
      "wrap": 400
     }
    ],
    "gap": 4
   }
  },
  {
   "title": "CLOCK",
   "note": "8 MHz HSE on PD0/PD1 and 32.768 kHz LSE on PC14/PC15",
   "tree": {
    "col": [
     {
      "row": [
       {
        "col": [
         {
          "part": "C1"
         }
        ]
       },
       {
        "part": "Y1"
       },
       {
        "col": [
         {
          "part": "C2"
         }
        ]
       }
      ],
      "gap": 3
     },
     {
      "row": [
       {
        "col": [
         {
          "part": "C10"
         }
        ]
       },
       {
        "part": "Y2"
       },
       {
        "col": [
         {
          "part": "C11"
         }
        ]
       }
      ],
      "gap": 3
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "RESET, BOOT AND SWD",
   "note": "10k/100nF reset, BOOT straps and the 10-pin Cortex SWD connector",
   "tree": {
    "row": [
     {
      "row": [
       {
        "part": "SW1"
       },
       {
        "col": [
         {
          "part": "R1"
         },
         {
          "part": "C12"
         }
        ],
        "gap": 4
       }
      ],
      "gap": 4
     },
     {
      "part": "J4"
     },
     {
      "row": [
       {
        "col": [
         {
          "part": "JP1"
         },
         {
          "part": "R2"
         }
        ],
        "gap": 3
       },
       {
        "col": [
         {
          "part": "JP2"
         },
         {
          "part": "R3"
         }
        ],
        "gap": 3
       }
      ],
      "gap": 5
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "HEADER LEFT",
   "note": "20-pin B/A GPIO header",
   "tree": {
    "part": "J1"
   }
  },
  {
   "title": "HEADER RIGHT",
   "note": "20-pin A/C header, the VBAT backup pin, its 100nF and the +3V3 solder bridge",
   "tree": {
    "row": [
     {
      "part": "J2"
     },
     {
      "row": [
       {
        "part": "JP3"
       },
       {
        "part": "C9"
       }
      ],
      "gap": 4
     }
    ],
    "gap": 2,
    "align": "end"
   }
  }
 ],
 "power": []
}
```

## Checklist

- One 100 nF per VDD pin (24/36/48) plus VDDA (9) and VBAT (1), and a 10 uF bulk on +3V3.
- VDDA/VSSA tie to +3V3/GND on this board; add a ferrite bead only if the request asks for a
  low-noise analog supply.
- 8 MHz HSE on PD0/PD1 (`OSC_IN`/`OSC_OUT`) with 22 pF loads; 32.768 kHz LSE on PC14/PC15
  (`OSC32_IN`/`OSC32_OUT`) with 12 pF loads. Never put 22 pF on the watch crystal.
- NRST: 10 k pull-up to +3V3, 100 nF to GND, push button to GND, and NRST on the SWD header.
- BOOT0 (44) and BOOT1/PB2 (20) each idle low through 10 k and sit on the centre pin of a
  3-pin strap (+3V3 / signal / GND), as on the real board. Instantiate both straps even when
  only the default boot mode is wanted - never branch the topology.
- USB: 22 R in series with PA11 (D-) and PA12 (D+); the 1k5 pull-up goes from +3V3 to D+ on the
  **MCU side** of the series resistor. Shield and pin 5 to GND, `ID` marked `nc`.
- Fuse VBUS with a 500 mA polyfuse ahead of the LDO, and declare
  `"flags": ["+5V", "VBAT_RTC"]` - both nets reach the board only through passive parts, so ERC
  needs a PWR_FLAG on each. Do NOT flag GND: J5's shield/GND pins already drive it and a second
  driver is an ERC error.
- The RTC backup rail is its own net **named `VBAT_RTC`, not `VBAT`**: it carries its own 100 nF,
  is bridged to +3V3 by JP3 (`Jumper:SolderJumper_2_Bridged`, bridged by default, exactly as the
  real board ties VBAT to 3V3) and comes out on J2-16. The name matters: the engine treats
  `VBAT` as a supply rail, which draws a boxed global label at the header pin that overprints the
  neighbouring NRST label, and which forbids wiring the cap and the jumper to the header pin at
  all (supply nets never wire a power-only part beyond 8 units). As a signal net the same three
  parts wire into one short chain with a single plain label.
- Power symbols: `+3V3`/`+5V` point up, `GND` points down. The engine does this automatically
  for the names `GND`, `+3V3`, `+5V`, `VBUS`.
- Every MCU pin carries a net: GPIOs go to the header nets `PA0..PB15`, SWD to `SWDIO`/`SWCLK`,
  PC13 to the user LED. Only USB `ID` and SWD header pins 7/8 are `nc`.
- PC13 sinks the user LED (anode through 1 k to +3V3), as on the real board.
- **POWER must hold only supply parts** (J3, F1, C3, U2, C4). The engine wires a supply chain
  end to end only when *every* part of the block is on power nets alone (a "pure power group");
  one signal net in the block - the power LED's `PWR_LED_A`, say - makes every cap and the fuse
  "power-only" again, and the chain falls apart into five islands each with its own +5V/+3V3
  symbol. Both indicator LEDs therefore live in MCU CORE.
- Short nets are drawn as wires only up to 32 units (about 40 mm), so put a part beside the pin
  it feeds: the R4/D1 column sits directly left of U1 with `"align": "start"` so `LED_K` reaches
  PC13, and R1/SW1/C12 stay in one column so NRST is one wired star instead of three labels.
- A shunt cap at the end of a row is laid flat by the engine ("rail-terminated series element").
  Wrap each load cap in its own `{"col": [...]}` - `row[col[C1], Y1, col[C2]]` - and both stand
  upright with their grounds straight down, which is what a reviewer checks on a crystal.
- Keep series resistors horizontal by wrapping each in a `{"row": [...]}`: a bare part inside a
  `col` is stood vertical, which forces bent wires and rotated net labels on J5 -> R7/R8.
- Paper A3: at 36 parts the eight blocks pack onto A3 only while MCU CORE stays about 120 mm
  tall. Set `"wrap": 400` on any row you mean to stay one row: the engine silently wraps rows
  wider than 150 units and drops the tail beside the tall part.

## Common mistakes

- Inventing a "BOOT1" pin on the MCU. It is PB2, and PB2 must then not also appear as a plain
  GPIO on the headers.
- Breaking PA13/PA14 out on a GPIO header: SWDIO/SWCLK belong on the debug connector only.
- One 100 nF for the whole MCU - the rubric counts decoupling per supply pin.
- Tying the 1k5 pull-up to VBUS, or to the connector side of the 22 R resistors.
- Omitting `"flags"`, which leaves ERC reporting that +5V or VBAT_RTC has no driver - or adding
  GND to it, which trips `pin_to_pin: Power output and Power output are connected`.
- Merging VBAT_RTC into +3V3, which defeats the RTC backup domain: bridge it with JP3 instead.
- Naming a header net differently from the MCU net (`PA_0` vs `PA0`): read the NETLIST in the
  build report - a net with one pin is always a mistake.
- Putting J1 and J2 in one block. Two 20-pin headers side by side make the engine run a 53 mm
  rail wire from J1-20 to J2-18 straight through the neighbouring +5V/GND labels; give each
  header its own block (HEADER LEFT / HEADER RIGHT) and each rail stays local.
