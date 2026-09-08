---
name: stm32f4-buck
description: STM32F405RGTx LQFP-64 controller board - TPS54302 5 V to 3.3 V synchronous buck with 4.7 uH SRP5030T, USB-C 2.0 device with USBLC6 ESD and 5.1k CC pulldowns, 8 MHz HSE, VCAP/VDDA supply network, Cortex SWD header and four 6-pin GPIO headers.
triggers: ["stm32f405", "stm32f4", "stm32f405rgtx", "tps54302", "buck regulator mcu", "usb-c stm32", "cortex-m4 board", "switching regulator controller", "3.3v buck"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `MCU_ST_STM32F4:STM32F405RGTx` | `Package_QFP:LQFP-64_10x10mm_P0.5mm` | STM32F405RGTx |
| U2 | `Regulator_Switching:TPS54302` | `Package_TO_SOT_SMD:SOT-23-6` | TPS54302 |
| U3 | `Power_Protection:USBLC6-2SC6` | `Package_TO_SOT_SMD:SOT-23-6` | USBLC6-2SC6 |
| J2 | `Connector:USB_C_Receptacle_USB2.0_16P` | `Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal` | USB-C 2.0 |
| J3 | `Connector_Generic:Conn_02x05_Odd_Even` | `Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm_Vertical` | Cortex 10-pin SWD |
| J4-J7 | `Connector_Generic:Conn_01x06` | `Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical` | GPIO headers |
| L1 | `Device:L` | `Inductor_SMD:L_Bourns_SRP5030T` | 4.7 uH shielded |
| FB1 | `Device:FerriteBead` | `Inductor_SMD:L_0603_1608Metric` | 600R@100MHz VDDA |
| F1 | `Device:Polyfuse` | `Fuse:Fuse_1206_3216Metric` | 2 A PTC |
| D1 | `Device:D_TVS` | `Diode_SMD:D_SMA` | SMF5.0A 5 V TVS |
| Y1 | `Device:Crystal_GND24` | `Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm` | 8 MHz HSE |
| SW1 | `Switch:SW_Push` | `Button_Switch_SMD:SW_SPST_TL3342` | reset |
| JP1 | `Jumper:Jumper_2_Bridged` | `Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` | BOOT0 select |
| D2, D3 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | red power / green activity |
| R1, R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k / 22.1k feedback |
| R3, R8 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k EN pull-up / VBUS divider top |
| R4, R5 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 5.1k CC1 / CC2 |
| R6, R7 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 22R USB series |
| R9 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 33k VBUS divider bottom |
| R10, R11 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k NRST / 100k BOOT0 |
| R12, R13 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k LED series |
| R14, R15 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 4.7k I2C pull-ups |
| C1, C4, C5 | `Device:C` | `Capacitor_SMD:C_1206_3216Metric` | 10 uF in / 2x 22 uF out |
| C10 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 4.7 uF MCU bulk |
| C2, C3, C6-C9, C11-C18 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | see pin map |

Pin-key notes verified against the installed libraries. `STM32F405RGTx` is a **single unit**;
`VDD` covers pins 19/32/48/64 and `VSS` covers 18/63, so one name key each is enough. The
supply rails are named `VDDA`, `VSSA`, `VBAT`, and the internal-LDO pins are `VCAP_1` (31) and
`VCAP_2` (47) with an underscore. The 8 MHz oscillator is on `PH0`/`PH1` (pins 5/6), not PD0/PD1.
`TPS54302` pins are `GND VIN SW FB EN BOOT`. `USBLC6-2SC6` names its data pairs `I/O1` (1,6) and
`I/O2` (3,4), so the name key ties both pins of a pair automatically; the supply pin is `VBUS`.
`Connector:USB_C_Receptacle_USB2.0_16P` name keys `VBUS`, `GND`, `D+`, `D-` cover the duplicated
A/B pins in one entry; the shell pin is `SHIELD`. `Device:Crystal_GND24` numbers its pins 1/2/3/4
but names them `1 G 3 G`, so use keys `"1"`, `"3"` and `"G"` (G ties both case pads).
`Device:D_TVS` pins are `A1`/`A2` (keys `"1"`/`"2"`), `Device:LED` pins are `A`/`K`,
`Jumper:Jumper_2_Bridged` pins are `A`/`B`, `Switch:SW_Push` pins are `1`/`2`.

## Pin map

```
+3V3: L1.2, C4.1, C5.1, R1.1, U1.VDD, U1.VBAT, C6.1, C7.1, C8.1, C9.1, C10.1, FB1.1, C13.1, R10.1, JP1.B, R12.1, R13.1, R14.1, R15.1, J3.1, J5.1, J7.1
+5V: F1.2, D1.1, C1.1, C2.1, R3.1, U2.VIN
BOOT0: U1.BOOT0, R11.1, JP1.A
BUCK_BOOT: U2.BOOT, C3.1
BUCK_EN: R3.2, U2.EN
CC1: J2.CC1, R4.1
CC2: J2.CC2, R5.1
FB: U2.FB, R1.2, R2.1
GND: D1.2, C1.2, C2.2, U2.GND, C4.2, C5.2, R2.2, J2.GND, J2.SHIELD, R4.2, R5.2, U3.GND, R9.2, U1.VSS, U1.VSSA, C6.2, C7.2, C8.2, C9.2, C10.2, C11.2, C12.2, C13.2, C14.2, C15.2, C16.2, SW1.2, R11.2, Y1.G, C17.2, C18.2, D2.K, J3.3, J3.5, J3.9, J4.1, J6.1, J7.6
I2C1_SCL: U1.PB8, R14.2, J4.2
I2C1_SDA: U1.PB9, R15.2, J4.3
LED_ACT: U1.PC13, D3.K
LED_ACT_A: R13.2, D3.A
LED_PWR_A: R12.2, D2.A
NRST: U1.NRST, R10.2, C16.1, SW1.1, J3.10
OSC_IN: U1.PH0, Y1.1, C17.1
OSC_OUT: U1.PH1, Y1.3, C18.1
PA1: U1.PA1, J4.6
PA15: U1.PA15, J7.4
PA4: U1.PA4, J5.2
PB0: U1.PB0, J5.6
PB1: U1.PB1, J6.2
PB10: U1.PB10, J6.3
PB11: U1.PB11, J6.4
PB12: U1.PB12, J6.5
PB13: U1.PB13, J6.6
PB14: U1.PB14, J7.2
PB15: U1.PB15, J7.3
PD2: U1.PD2, J7.5
SPI1_MISO: U1.PA6, J5.4
SPI1_MOSI: U1.PA7, J5.5
SPI1_SCK: U1.PA5, J5.3
SW: U2.SW, C3.2, L1.1
SWCLK: U1.PA14, J3.4
SWDIO: U1.PA13, J3.2
SWO: U1.PB3, J3.6
USART2_RX: U1.PA3, J4.5
USART2_TX: U1.PA2, J4.4
USB_DM: R6.2, U1.PA11
USB_DM_CON: J2.D-, U3.I/O1, R6.1
USB_DP: R7.2, U1.PA12
USB_DP_CON: J2.D+, U3.I/O2, R7.1
VBUS: F1.1, J2.VBUS, U3.VBUS, R8.1
VBUS_SENSE: R8.2, R9.1, U1.PA0
VCAP1: U1.VCAP_1, C14.1
VCAP2: U1.VCAP_2, C15.1
VDDA: U1.VDDA, FB1.2, C11.1, C12.1
```

## Layout

The complete design JSON below builds with `layout_errors=0 issues=0 erc=0` on an A2 sheet.
Hand it to `build` as-is, adapting values and header assignments to the request:

```json
{
 "title": "STM32F405 Controller with TPS54302 Buck",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A2",
 "comments": [
  "STM32F405RGTx controller, USB-C device, SWD",
  "5 V USB to 3.3 V TPS54302 synchronous buck"
 ],
 "flags": [
  "VBUS",
  "+5V",
  "+3V3",
  "VDDA",
  "GND"
 ],
 "power": [
  "VDDA"
 ],
 "notes": [
  "TPS54302 feedback: 100k/22.1k sets 3.3 V from the 0.596 V reference.",
  "VDDA is fed from +3V3 through FB1 and decoupled with 100nF + 1uF.",
  "VCAP1/VCAP2 need 2.2 uF low-ESR ceramics for the internal 1.2 V LDO."
 ],
 "parts": [
  {
   "id": "F1",
   "lib": "Device:Polyfuse",
   "value": "2A PTC",
   "footprint": "Fuse:Fuse_1206_3216Metric",
   "pins": {
    "1": "VBUS",
    "2": "+5V"
   }
  },
  {
   "id": "D1",
   "lib": "Device:D_TVS",
   "value": "SMF5.0A",
   "footprint": "Diode_SMD:D_SMA",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_1206_3216Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "BUCK_EN"
   }
  },
  {
   "id": "U2",
   "lib": "Regulator_Switching:TPS54302",
   "value": "TPS54302",
   "footprint": "Package_TO_SOT_SMD:SOT-23-6",
   "pins": {
    "VIN": "+5V",
    "GND": "GND",
    "EN": "BUCK_EN",
    "BOOT": "BUCK_BOOT",
    "SW": "SW",
    "FB": "FB"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "BUCK_BOOT",
    "2": "SW"
   }
  },
  {
   "id": "L1",
   "lib": "Device:L",
   "value": "4.7uH",
   "footprint": "Inductor_SMD:L_Bourns_SRP5030T",
   "pins": {
    "1": "SW",
    "2": "+3V3"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "22uF",
   "footprint": "Capacitor_SMD:C_1206_3216Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C5",
   "lib": "Device:C",
   "value": "22uF",
   "footprint": "Capacitor_SMD:C_1206_3216Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "FB"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "22.1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "FB",
    "2": "GND"
   }
  },
  {
   "id": "J2",
   "lib": "Connector:USB_C_Receptacle_USB2.0_16P",
   "value": "USB-C 2.0",
   "footprint": "Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal",
   "pins": {
    "VBUS": "VBUS",
    "GND": "GND",
    "SHIELD": "GND",
    "CC1": "CC1",
    "CC2": "CC2",
    "D+": "USB_DP_CON",
    "D-": "USB_DM_CON",
    "SBU1": "nc",
    "SBU2": "nc"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "5.1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CC1",
    "2": "GND"
   }
  },
  {
   "id": "R5",
   "lib": "Device:R",
   "value": "5.1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "CC2",
    "2": "GND"
   }
  },
  {
   "id": "U3",
   "lib": "Power_Protection:USBLC6-2SC6",
   "value": "USBLC6-2SC6",
   "footprint": "Package_TO_SOT_SMD:SOT-23-6",
   "pins": {
    "I/O1": "USB_DM_CON",
    "I/O2": "USB_DP_CON",
    "VBUS": "VBUS",
    "GND": "GND"
   }
  },
  {
   "id": "R6",
   "lib": "Device:R",
   "value": "22R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "USB_DM_CON",
    "2": "USB_DM"
   }
  },
  {
   "id": "R7",
   "lib": "Device:R",
   "value": "22R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "USB_DP_CON",
    "2": "USB_DP"
   }
  },
  {
   "id": "R8",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "VBUS",
    "2": "VBUS_SENSE"
   }
  },
  {
   "id": "R9",
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
   "lib": "MCU_ST_STM32F4:STM32F405RGTx",
   "value": "STM32F405RGTx",
   "footprint": "Package_QFP:LQFP-64_10x10mm_P0.5mm",
   "pins": {
    "VDD": "+3V3",
    "VSS": "GND",
    "VBAT": "+3V3",
    "VDDA": "VDDA",
    "VSSA": "GND",
    "VCAP_1": "VCAP1",
    "VCAP_2": "VCAP2",
    "NRST": "NRST",
    "BOOT0": "BOOT0",
    "PH0": "OSC_IN",
    "PH1": "OSC_OUT",
    "PC13": "LED_ACT",
    "PC14": "nc",
    "PC15": "nc",
    "PC0": "nc",
    "PC1": "nc",
    "PC2": "nc",
    "PC3": "nc",
    "PA0": "VBUS_SENSE",
    "PA1": "PA1",
    "PA2": "USART2_TX",
    "PA3": "USART2_RX",
    "PA4": "PA4",
    "PA5": "SPI1_SCK",
    "PA6": "SPI1_MISO",
    "PA7": "SPI1_MOSI",
    "PC4": "nc",
    "PC5": "nc",
    "PB0": "PB0",
    "PB1": "PB1",
    "PB2": "nc",
    "PB10": "PB10",
    "PB11": "PB11",
    "PB12": "PB12",
    "PB13": "PB13",
    "PB14": "PB14",
    "PB15": "PB15",
    "PC6": "nc",
    "PC7": "nc",
    "PC8": "nc",
    "PC9": "nc",
    "PA8": "nc",
    "PA9": "nc",
    "PA10": "nc",
    "PA11": "USB_DM",
    "PA12": "USB_DP",
    "PA13": "SWDIO",
    "PA14": "SWCLK",
    "PA15": "PA15",
    "PC10": "nc",
    "PC11": "nc",
    "PC12": "nc",
    "PD2": "PD2",
    "PB3": "SWO",
    "PB4": "nc",
    "PB5": "nc",
    "PB6": "nc",
    "PB7": "nc",
    "PB8": "I2C1_SCL",
    "PB9": "I2C1_SDA"
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
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C10",
   "lib": "Device:C",
   "value": "4.7uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "FB1",
   "lib": "Device:FerriteBead",
   "value": "600R@100MHz",
   "footprint": "Inductor_SMD:L_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "VDDA"
   }
  },
  {
   "id": "C11",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VDDA",
    "2": "GND"
   }
  },
  {
   "id": "C12",
   "lib": "Device:C",
   "value": "1uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VDDA",
    "2": "GND"
   }
  },
  {
   "id": "C13",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C14",
   "lib": "Device:C",
   "value": "2.2uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VCAP1",
    "2": "GND"
   }
  },
  {
   "id": "C15",
   "lib": "Device:C",
   "value": "2.2uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VCAP2",
    "2": "GND"
   }
  },
  {
   "id": "R10",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "NRST"
   }
  },
  {
   "id": "C16",
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
   "id": "R11",
   "lib": "Device:R",
   "value": "100k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "BOOT0",
    "2": "GND"
   }
  },
  {
   "id": "JP1",
   "lib": "Jumper:Jumper_2_Bridged",
   "value": "BOOT0 SEL",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
   "pins": {
    "A": "BOOT0",
    "B": "+3V3"
   }
  },
  {
   "id": "Y1",
   "lib": "Device:Crystal_GND24",
   "value": "8MHz",
   "footprint": "Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm",
   "pins": {
    "1": "OSC_IN",
    "3": "OSC_OUT",
    "G": "GND"
   }
  },
  {
   "id": "C17",
   "lib": "Device:C",
   "value": "18pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "OSC_IN",
    "2": "GND"
   }
  },
  {
   "id": "C18",
   "lib": "Device:C",
   "value": "18pF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "OSC_OUT",
    "2": "GND"
   }
  },
  {
   "id": "R12",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "LED_PWR_A"
   }
  },
  {
   "id": "D2",
   "lib": "Device:LED",
   "value": "RED PWR",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "A": "LED_PWR_A",
    "K": "GND"
   }
  },
  {
   "id": "R13",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "LED_ACT_A"
   }
  },
  {
   "id": "D3",
   "lib": "Device:LED",
   "value": "GREEN ACT",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "A": "LED_ACT_A",
    "K": "LED_ACT"
   }
  },
  {
   "id": "R14",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "I2C1_SCL"
   }
  },
  {
   "id": "R15",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "I2C1_SDA"
   }
  },
  {
   "id": "J3",
   "lib": "Connector_Generic:Conn_02x05_Odd_Even",
   "value": "SWD",
   "footprint": "Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "SWDIO",
    "3": "GND",
    "4": "SWCLK",
    "5": "GND",
    "6": "SWO",
    "7": "nc",
    "8": "nc",
    "9": "GND",
    "10": "NRST"
   }
  },
  {
   "id": "J4",
   "lib": "Connector_Generic:Conn_01x06",
   "value": "IO A",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical",
   "pins": {
    "1": "GND",
    "2": "I2C1_SCL",
    "3": "I2C1_SDA",
    "4": "USART2_TX",
    "5": "USART2_RX",
    "6": "PA1"
   }
  },
  {
   "id": "J5",
   "lib": "Connector_Generic:Conn_01x06",
   "value": "IO B",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "PA4",
    "3": "SPI1_SCK",
    "4": "SPI1_MISO",
    "5": "SPI1_MOSI",
    "6": "PB0"
   }
  },
  {
   "id": "J6",
   "lib": "Connector_Generic:Conn_01x06",
   "value": "IO C",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical",
   "pins": {
    "1": "GND",
    "2": "PB1",
    "3": "PB10",
    "4": "PB11",
    "5": "PB12",
    "6": "PB13"
   }
  },
  {
   "id": "J7",
   "lib": "Connector_Generic:Conn_01x06",
   "value": "IO D",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "PB14",
    "3": "PB15",
    "4": "PA15",
    "5": "PD2",
    "6": "GND"
   }
  }
 ],
 "layout": [
  {
   "title": "POWER 5V TO 3V3",
   "note": "TPS54302 buck: fused VBUS, 4.7uH SRP5030T, 100k/22.1k feedback",
   "tree": {
    "col": [
     {
      "row": [
       {
        "row": [
         {
          "part": "F1"
         },
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
       },
       {
        "col": [
         {
          "part": "R3"
         },
         {
          "part": "C3"
         }
        ],
        "gap": 4
       },
       {
        "part": "U2"
       }
      ],
      "gap": 5
     },
     {
      "row": [
       {
        "part": "L1"
       },
       {
        "row": [
         {
          "part": "C4"
         },
         {
          "part": "C5"
         }
        ],
        "gap": 4
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
        "gap": 4
       }
      ],
      "gap": 5
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "USB-C DEVICE",
   "note": "5.1k CC pulldowns, USBLC6 ESD, 22R series to PA11/PA12, VBUS sense divider",
   "tree": {
    "row": [
     {
      "part": "J2"
     },
     {
      "col": [
       {
        "part": "R4"
       },
       {
        "part": "R5"
       }
      ],
      "gap": 4
     },
     {
      "part": "U3"
     },
     {
      "col": [
       {
        "part": "R6"
       },
       {
        "part": "R7"
       }
      ],
      "gap": 5
     },
     {
      "col": [
       {
        "part": "R8"
       },
       {
        "part": "R9"
       }
      ],
      "gap": 4
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "MCU CORE",
   "note": "One 100nF per VDD pin, 4.7uF bulk, ferrite-fed VDDA, VCAP ceramics",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "C6"
       },
       {
        "part": "C7"
       },
       {
        "part": "C8"
       },
       {
        "part": "C9"
       },
       {
        "part": "C10"
       },
       {
        "part": "C13"
       }
      ],
      "gap": 4
     },
     {
      "row": [
       {
        "col": [
         {
          "part": "FB1"
         },
         {
          "part": "C11"
         },
         {
          "part": "C12"
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
          "part": "C14"
         },
         {
          "part": "C15"
         }
        ],
        "gap": 4
       }
      ],
      "gap": 5
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "RESET BOOT CLOCK",
   "note": "10k/100nF reset, 100k BOOT0 pulldown with jumper, 8 MHz HSE on PH0/PH1",
   "tree": {
    "col": [
     {
      "row": [
       {
        "col": [
         {
          "part": "R10"
         },
         {
          "row": [
           {
            "part": "SW1"
           },
           {
            "part": "C16"
           }
          ],
          "gap": 4
         }
        ],
        "gap": 4
       },
       {
        "col": [
         {
          "part": "JP1"
         },
         {
          "part": "R11"
         }
        ],
        "gap": 4
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "part": "C17"
       },
       {
        "part": "Y1"
       },
       {
        "part": "C18"
       }
      ],
      "gap": 4
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "LEDS AND I2C PULLUPS",
   "note": "Red power LED, green PC13 activity LED, 4.7k pull-ups on PB8/PB9",
   "tree": {
    "row": [
     {
      "col": [
       {
        "row": [
         {
          "part": "R12"
         },
         {
          "part": "D2"
         }
        ],
        "gap": 4
       },
       {
        "row": [
         {
          "part": "R13"
         },
         {
          "part": "D3"
         }
        ],
        "gap": 4
       }
      ],
      "gap": 5
     },
     {
      "col": [
       {
        "part": "R14"
       },
       {
        "part": "R15"
       }
      ],
      "gap": 4
     }
    ],
    "gap": 6
   }
  },
  {
   "title": "SWD AND GPIO HEADERS",
   "note": "Cortex 10-pin debug plus four 6-pin GPIO headers, each with a supply pin",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "J3"
       },
       {
        "part": "J4"
       },
       {
        "part": "J5"
       }
      ],
      "gap": 6
     },
     {
      "row": [
       {
        "part": "J6"
       },
       {
        "part": "J7"
       }
      ],
      "gap": 6
     }
    ],
    "gap": 6
   }
  }
 ]
}
```

## Checklist

- One 100 nF per VDD pin (19/32/48/64) plus a 4.7 uF bulk on +3V3; a single shared cap fails
  the rubric, which counts decoupling per supply pin.
- VDDA is fed from +3V3 through FB1 and decoupled with 100 nF and 1 uF; VSSA goes straight to
  GND. VBAT gets its own 100 nF.
- VCAP_1 and VCAP_2 each need a 2.2 uF low-ESR ceramic to GND. These are `power_out` pins of the
  internal 1.2 V LDO - never tie them to a rail.
- TPS54302 feedback: 100 k top and 22.1 k bottom from +3V3 to GND gives ~3.3 V from the 0.596 V
  reference. The 100 nF bootstrap cap goes BOOT to SW, not BOOT to ground.
- Keep the switch node (`SW`) to exactly three pins: U2.SW, C3.2 and L1.1. Nothing else.
- Fuse VBUS with a 2 A polyfuse, clamp +5V with a TVS, and give +5V both a 10 uF and a 100 nF.
  EN is pulled up to +5V with 100 k so the buck starts as soon as VBUS is present.
- USB-C: independent 5.1 k pulldowns on CC1 and CC2 (never one shared resistor), 22 R in series
  between the USBLC6 and PA11/PA12, and a 100 k/33 k divider from VBUS to PA0 for cable sensing.
  `SBU1`/`SBU2` are `nc`; `SHIELD` and `GND` both go to GND.
- 8 MHz HSE on PH0/PH1 with two 18 pF loads and the crystal case pads (`G`) grounded.
- NRST: 10 k pull-up, 100 nF to GND, push button to GND, and NRST on SWD pin 10. BOOT0: 100 k
  pulldown plus a 2-pin jumper to +3V3 - fit the jumper even for the default boot mode.
- SWD header carries +3V3, SWDIO (PA13), SWCLK (PA14), SWO (PB3), NRST and three GND pins;
  pins 7 and 8 are `nc`.
- Twenty GPIOs across J4-J7, five signals plus one +3V3 or GND pin per header. Label the nets
  with the MCU signal name (`PB10`, `SPI1_SCK`, `I2C1_SCL`), not with a header pin number.
- 4.7 k pull-ups on PB8/PB9 (I2C1) to +3V3, and those two signals also appear on J4.
- `"flags": ["VBUS", "+5V", "+3V3", "VDDA", "GND"]`: +3V3 comes through L1 and VDDA through FB1,
  so both are only reached by passives and ERC needs a PWR_FLAG on each.
- Power symbols: `+3V3`/`+5V`/`VBUS` point up, `GND` points down; the engine handles this for
  those names and for the extra rail declared in `"power": ["VDDA"]`.

## Common mistakes

- Writing `VCAP1`/`VCAP2` as pin keys. The symbol spells them `VCAP_1` and `VCAP_2`; the *net*
  names may be anything.
- Putting the HSE on PD0/PD1 (that is the F103 pinout). On the F405 LQFP-64 it is PH0/PH1.
- Listing `Connector:USB_C_Receptacle_USB2.0_16P` pins one by one as A4/B4/A9/B9. Use the name
  keys so every duplicated VBUS, GND, D+ and D- pin lands on the same net.
- Bootstrap cap from BOOT to GND, or the inductor placed between VIN and SW. The chain is
  VIN -> U2 -> SW -> L1 -> +3V3, with C3 across BOOT-SW.
- Forgetting the PWR_FLAG on +3V3: the buck output is only driven through the inductor, so ERC
  reports "Input Power pin not driven" on every VDD.
- One 5.1 k resistor shared by CC1 and CC2, which makes the source see the wrong sink advert.
- Breaking PA13/PA14 out on a GPIO header as well as on SWD, or leaving PB2 wired as both a
  GPIO and a boot strap.
