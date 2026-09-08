---
name: esp32-wroom-sensor-node
description: ESP32-WROOM-32 USB-C environmental sensor node - CP2102N-Axx-xQFN20 USB-UART bridge with two-transistor auto-program on EN/IO0, AP2112K-3.3 LDO, USBLC6 ESD, BME280 over I2C at 0x76, microSD over SPI with series damping, LEDs and GPIO headers.
triggers: ["esp32", "esp32-wroom", "esp32 devkit", "cp2102", "bme280", "sensor node", "microsd logger", "ap2112k", "esp32 usb-c"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U4 | `RF_Module:ESP32-WROOM-32` | `RF_Module:ESP32-WROOM-32` | ESP32-WROOM-32 |
| U3 | `Interface_USB:CP2102N-Axx-xQFN20` | `Package_DFN_QFN:SiliconLabs_QFN-20-1EP_3x3mm_P0.5mm_EP1.8x1.8mm` | CP2102N-A01-GQFN20 |
| U2 | `Regulator_Linear:AP2112K-3.3` | `Package_TO_SOT_SMD:SOT-23-5` | AP2112K-3.3 |
| U1 | `Power_Protection:USBLC6-2SC6` | `Package_TO_SOT_SMD:SOT-23-6` | USBLC6-2SC6 |
| U5 | `Sensor:BME280` | `Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering` | BME280 |
| J1 | `Connector:USB_C_Receptacle_USB2.0_16P` | `Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal` | USB-C 2.0 |
| J2 | `Connector:Micro_SD_Card_Det1` | `Connector_Card:microSD_HC_Hirose_DM3D-SF` | microSD |
| J3, J4 | `Connector_Generic:Conn_01x04` / `Conn_01x05` | `Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical` / `..._1x05_...` | IO headers |
| Q1, Q2 | `Transistor_BJT:MMBT3904` | `Package_TO_SOT_SMD:SOT-23` | auto-program NPNs |
| F1 | `Device:Polyfuse` | `Fuse:Fuse_1206_3216Metric` | 500 mA PTC |
| SW1, SW2 | `Switch:SW_Push` | `Button_Switch_SMD:SW_SPST_TL3342` | reset / boot |
| D1, D2 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | red power / blue status |
| R1, R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 5.1k CC1 / CC2 |
| R3-R6, R12-R14, R17 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k base, EN, IO0, SD and address straps |
| R7, R8 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 4.7k I2C pull-ups |
| R9-R11 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 33R SD series damping |
| R15, R16 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k LED series |
| C1-C3, C8, C14 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 10 uF bulk |
| C4-C7, C9-C13, C15 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 4.7 uF / 1 uF / 100 nF, see pin map |

Pin-key notes verified against the installed libraries; several of these symbols use unusual
names. `RF_Module:ESP32-WROOM-32`: the UART pins are `RXD0/IO3` and `TXD0/IO1`, the flash pins
are `SHD/SD2 SWP/SD3 SCS/CMD SCK/CLK SDO/SD0 SDI/SD1`, the analog pins are `SENSOR_VP`/
`SENSOR_VN`, the supply is `VDD`, and `GND` covers pins 1/15/38/39. Pin 32 is a hidden
`no_connect` pin - do **not** give it a key; add `"pins_default": "nc"` instead.
`Interface_USB:CP2102N-Axx-xQFN20`: keys `~{RST} ~{SUSPEND} SUSPEND ~{WAKEUP} ~{CTS} ~{RTS}
RXD TXD RS485/GPIO.1 CLK/GPIO.0 ~{RXT}/GPIO.3 ~{TXT}/GPIO.2 VREGIN VBUS VDD D+ D-`; `GND` covers
3/12/21 and pin 10 is again a hidden `no_connect` pin. `Regulator_Linear:AP2112K-3.3` pins are
`VIN GND EN VOUT` - its pin 4 `NC` is hidden too. `Sensor:BME280` pins are
`GND CSB SDI SCK SDO VDDIO VDD` (SDI is SDA and SCK is SCL in I2C mode).
`Connector:Micro_SD_Card_Det1` pins are `DAT2 DAT3/CD CMD VDD CLK VSS DAT0 DAT1 DET SHIELD`.
`USBLC6-2SC6` names its pairs `I/O1` (1,6) and `I/O2` (3,4). `Transistor_BJT:MMBT3904` pins are
`B E C`. `Device:LED` pins are `A`/`K`.

## Pin map

```
+3V3: U2.VOUT, C3.1, C4.1, U3.VDD, C5.1, U4.VDD, C8.1, C9.1, C10.1, R5.1, R6.1, U5.VDD, U5.VDDIO, U5.CSB, C12.1, C13.1, R7.1, R8.1, J2.VDD, C14.1, C15.1, R12.1, R13.1, R14.1, R15.1, R16.1, J3.1, J4.1
+5V: F1.2, C1.1, U2.VIN, U2.EN, C2.1, U3.VREGIN, U3.VBUS, C6.1, C7.1
BME_ADDR: U5.SDO, R17.1
CC1: J1.CC1, R1.1
CC2: J1.CC2, R2.1
DTR: U3.RS485/GPIO.1, Q1.E, R4.1
EN: Q1.C, U4.EN, R5.2, C11.1, SW1.1
ESP_RXD: U3.TXD, U4.RXD0/IO3
ESP_TXD: U3.RXD, U4.TXD0/IO1
GND: J1.GND, J1.SHIELD, R1.2, R2.2, C1.2, U1.GND, U2.GND, C2.2, C3.2, C4.2, U3.GND, C5.2, C6.2, C7.2, U4.GND, C8.2, C9.2, C10.2, C11.2, SW1.2, SW2.2, U5.GND, R17.2, C12.2, C13.2, J2.VSS, J2.SHIELD, C14.2, C15.2, D1.K, J3.4, J4.5
I2C_SCL: U4.IO22, U5.SCK, R8.2
I2C_SDA: U4.IO21, U5.SDI, R7.2
IO0: Q2.C, U4.IO0, R6.2, SW2.1
IO25: U4.IO25, D2.K, J3.2
IO26: U4.IO26, J3.3
IO27: U4.IO27, J4.2
IO32: U4.IO32, J4.3
IO33: U4.IO33, J4.4
LED_PWR_A: R15.2, D1.A
LED_STAT_A: R16.2, D2.A
Q1_B: R3.2, Q1.B
Q2_B: R4.2, Q2.B
RTS: U3.~{RTS}, R3.1, Q2.E
SD_CD: U4.IO4, J2.DET, R14.2
SD_CS: U4.IO5, R11.1
SD_CS_C: J2.DAT3/CD, R11.2, R12.2
SD_MISO: U4.IO19, J2.DAT0
SD_MOSI: U4.IO23, R10.1
SD_MOSI_C: J2.CMD, R10.2, R13.2
SD_SCK: U4.IO18, R9.1
SD_SCK_C: J2.CLK, R9.2
USB_DM: J1.D-, U1.I/O1, U3.D-
USB_DP: J1.D+, U1.I/O2, U3.D+
VBUS: J1.VBUS, F1.1, U1.VBUS
```

## Layout

The complete design JSON below builds with `layout_errors=0 issues=0 erc=0` on an A2 sheet.
Hand it to `build` as-is, adapting values and header assignments to the request:

```json
{
 "title": "ESP32-WROOM-32 Environmental Sensor Node",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A2",
 "comments": [
  "USB-C powered ESP32 node, CP2102N bridge, auto-program",
  "BME280 over I2C, microSD over SPI, AP2112K 3.3 V rail"
 ],
 "flags": [
  "VBUS",
  "+5V",
  "GND"
 ],
 "notes": [
  "Q1/Q2 cross-coupled emitters give the classic EN/IO0 auto-program without shorting RTS to DTR.",
  "CP2102N GPIO.1 is configured as DTR; TXD drives ESP_RXD and RXD listens on ESP_TXD.",
  "R17 straps BME280 SDO low for I2C address 0x76 and CSB is tied high for I2C mode."
 ],
 "parts": [
  {
   "id": "J1",
   "lib": "Connector:USB_C_Receptacle_USB2.0_16P",
   "value": "USB-C 2.0",
   "footprint": "Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal",
   "pins": {
    "VBUS": "VBUS",
    "GND": "GND",
    "SHIELD": "GND",
    "CC1": "CC1",
    "CC2": "CC2",
    "D+": "USB_DP",
    "D-": "USB_DM",
    "SBU1": "nc",
    "SBU2": "nc"
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
   "id": "C1",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "U1",
   "lib": "Power_Protection:USBLC6-2SC6",
   "value": "USBLC6-2SC6",
   "footprint": "Package_TO_SOT_SMD:SOT-23-6",
   "pins": {
    "I/O1": "USB_DM",
    "I/O2": "USB_DP",
    "VBUS": "VBUS",
    "GND": "GND"
   }
  },
  {
   "id": "U2",
   "lib": "Regulator_Linear:AP2112K-3.3",
   "value": "AP2112K-3.3",
   "footprint": "Package_TO_SOT_SMD:SOT-23-5",
   "pins": {
    "VIN": "+5V",
    "EN": "+5V",
    "GND": "GND",
    "VOUT": "+3V3"
   },
   "pins_default": "nc"
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "U3",
   "lib": "Interface_USB:CP2102N-Axx-xQFN20",
   "value": "CP2102N-A01-GQFN20",
   "footprint": "Package_DFN_QFN:SiliconLabs_QFN-20-1EP_3x3mm_P0.5mm_EP1.8x1.8mm",
   "pins": {
    "GND": "GND",
    "D+": "USB_DP",
    "D-": "USB_DM",
    "VDD": "+3V3",
    "VREGIN": "+5V",
    "VBUS": "+5V",
    "~{RST}": "nc",
    "~{SUSPEND}": "nc",
    "SUSPEND": "nc",
    "~{WAKEUP}": "nc",
    "~{CTS}": "nc",
    "~{RTS}": "RTS",
    "RXD": "ESP_TXD",
    "TXD": "ESP_RXD",
    "RS485/GPIO.1": "DTR",
    "CLK/GPIO.0": "nc",
    "~{RXT}/GPIO.3": "nc",
    "~{TXT}/GPIO.2": "nc"
   },
   "pins_default": "nc"
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
   "value": "4.7uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C7",
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
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "RTS",
    "2": "Q1_B"
   }
  },
  {
   "id": "Q1",
   "lib": "Transistor_BJT:MMBT3904",
   "value": "MMBT3904",
   "footprint": "Package_TO_SOT_SMD:SOT-23",
   "pins": {
    "B": "Q1_B",
    "E": "DTR",
    "C": "EN"
   }
  },
  {
   "id": "R4",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "DTR",
    "2": "Q2_B"
   }
  },
  {
   "id": "Q2",
   "lib": "Transistor_BJT:MMBT3904",
   "value": "MMBT3904",
   "footprint": "Package_TO_SOT_SMD:SOT-23",
   "pins": {
    "B": "Q2_B",
    "E": "RTS",
    "C": "IO0"
   }
  },
  {
   "id": "U4",
   "lib": "RF_Module:ESP32-WROOM-32",
   "value": "ESP32-WROOM-32",
   "footprint": "RF_Module:ESP32-WROOM-32",
   "pins": {
    "GND": "GND",
    "VDD": "+3V3",
    "EN": "EN",
    "SENSOR_VP": "nc",
    "SENSOR_VN": "nc",
    "IO34": "nc",
    "IO35": "nc",
    "IO32": "IO32",
    "IO33": "IO33",
    "IO25": "IO25",
    "IO26": "IO26",
    "IO27": "IO27",
    "IO14": "nc",
    "IO12": "nc",
    "IO13": "nc",
    "SHD/SD2": "nc",
    "SWP/SD3": "nc",
    "SCS/CMD": "nc",
    "SCK/CLK": "nc",
    "SDO/SD0": "nc",
    "SDI/SD1": "nc",
    "IO15": "nc",
    "IO2": "nc",
    "IO0": "IO0",
    "IO4": "SD_CD",
    "IO16": "nc",
    "IO17": "nc",
    "IO5": "SD_CS",
    "IO18": "SD_SCK",
    "IO19": "SD_MISO",
    "IO21": "I2C_SDA",
    "RXD0/IO3": "ESP_RXD",
    "TXD0/IO1": "ESP_TXD",
    "IO22": "I2C_SCL",
    "IO23": "SD_MOSI"
   },
   "pins_default": "nc"
  },
  {
   "id": "C8",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
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
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "R5",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "EN"
   }
  },
  {
   "id": "C11",
   "lib": "Device:C",
   "value": "1uF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "EN",
    "2": "GND"
   }
  },
  {
   "id": "SW1",
   "lib": "Switch:SW_Push",
   "value": "RESET",
   "footprint": "Button_Switch_SMD:SW_SPST_TL3342",
   "pins": {
    "1": "EN",
    "2": "GND"
   }
  },
  {
   "id": "R6",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "IO0"
   }
  },
  {
   "id": "SW2",
   "lib": "Switch:SW_Push",
   "value": "BOOT",
   "footprint": "Button_Switch_SMD:SW_SPST_TL3342",
   "pins": {
    "1": "IO0",
    "2": "GND"
   }
  },
  {
   "id": "U5",
   "lib": "Sensor:BME280",
   "value": "BME280",
   "footprint": "Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering",
   "pins": {
    "VDD": "+3V3",
    "VDDIO": "+3V3",
    "GND": "GND",
    "CSB": "+3V3",
    "SDO": "BME_ADDR",
    "SDI": "I2C_SDA",
    "SCK": "I2C_SCL"
   }
  },
  {
   "id": "R17",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "BME_ADDR",
    "2": "GND"
   }
  },
  {
   "id": "C12",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
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
   "id": "R7",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "I2C_SDA"
   }
  },
  {
   "id": "R8",
   "lib": "Device:R",
   "value": "4.7k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "I2C_SCL"
   }
  },
  {
   "id": "J2",
   "lib": "Connector:Micro_SD_Card_Det1",
   "value": "microSD",
   "footprint": "Connector_Card:microSD_HC_Hirose_DM3D-SF",
   "pins": {
    "VDD": "+3V3",
    "VSS": "GND",
    "SHIELD": "GND",
    "CLK": "SD_SCK_C",
    "CMD": "SD_MOSI_C",
    "DAT0": "SD_MISO",
    "DAT1": "nc",
    "DAT2": "nc",
    "DAT3/CD": "SD_CS_C",
    "DET": "SD_CD"
   }
  },
  {
   "id": "C14",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "C15",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "GND"
   }
  },
  {
   "id": "R9",
   "lib": "Device:R",
   "value": "33R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "SD_SCK",
    "2": "SD_SCK_C"
   }
  },
  {
   "id": "R10",
   "lib": "Device:R",
   "value": "33R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "SD_MOSI",
    "2": "SD_MOSI_C"
   }
  },
  {
   "id": "R11",
   "lib": "Device:R",
   "value": "33R",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "SD_CS",
    "2": "SD_CS_C"
   }
  },
  {
   "id": "R12",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "SD_CS_C"
   }
  },
  {
   "id": "R13",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "SD_MOSI_C"
   }
  },
  {
   "id": "R14",
   "lib": "Device:R",
   "value": "10k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "SD_CD"
   }
  },
  {
   "id": "R15",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "LED_PWR_A"
   }
  },
  {
   "id": "D1",
   "lib": "Device:LED",
   "value": "RED PWR",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "A": "LED_PWR_A",
    "K": "GND"
   }
  },
  {
   "id": "R16",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+3V3",
    "2": "LED_STAT_A"
   }
  },
  {
   "id": "D2",
   "lib": "Device:LED",
   "value": "BLUE STAT",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "A": "LED_STAT_A",
    "K": "IO25"
   }
  },
  {
   "id": "J3",
   "lib": "Connector_Generic:Conn_01x04",
   "value": "IO A",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "IO25",
    "3": "IO26",
    "4": "GND"
   }
  },
  {
   "id": "J4",
   "lib": "Connector_Generic:Conn_01x05",
   "value": "IO B",
   "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x05_P2.54mm_Vertical",
   "pins": {
    "1": "+3V3",
    "2": "IO27",
    "3": "IO32",
    "4": "IO33",
    "5": "GND"
   }
  }
 ],
 "layout": [
  {
   "title": "USB-C AND 3V3 SUPPLY",
   "note": "5.1k CC pulldowns, PTC fuse, USBLC6 ESD on D+/D-, AP2112K-3.3 LDO",
   "tree": {
    "col": [
     {
      "row": [
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
        "gap": 4
       },
       {
        "part": "U1"
       }
      ],
      "gap": 5
     },
     {
      "row": [
       {
        "part": "F1"
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
        "gap": 4
       },
       {
        "part": "U2"
       },
       {
        "row": [
         {
          "part": "C3"
         },
         {
          "part": "C4"
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
   "title": "USB-UART AND AUTO-PROGRAM",
   "note": "CP2102N bridge with crossed TXD/RXD; Q1/Q2 drive EN and IO0",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "C6"
       },
       {
        "part": "C7"
       },
       {
        "part": "C5"
       }
      ],
      "gap": 5
     },
     {
      "part": "U3"
     },
     {
      "col": [
       {
        "row": [
         {
          "part": "R3"
         },
         {
          "part": "Q1"
         }
        ],
        "gap": 6
       },
       {
        "row": [
         {
          "part": "R4"
         },
         {
          "part": "Q2"
         }
        ],
        "gap": 6
       }
      ],
      "gap": 8
     }
    ],
    "gap": 8
   }
  },
  {
   "title": "ESP32 MODULE",
   "note": "10uF plus two 100nF local caps; EN delay network and boot strap",
   "tree": {
    "col": [
     {
      "row": [
       {
        "part": "C8"
       },
       {
        "part": "C9"
       },
       {
        "part": "C10"
       }
      ],
      "gap": 4
     },
     {
      "row": [
       {
        "col": [
         {
          "part": "R5"
         },
         {
          "row": [
           {
            "part": "C11"
           },
           {
            "part": "SW1"
           }
          ],
          "gap": 4
         },
         {
          "part": "R6"
         },
         {
          "part": "SW2"
         }
        ],
        "gap": 4
       },
       {
        "part": "U4"
       }
      ],
      "gap": 6
     }
    ],
    "gap": 5
   }
  },
  {
   "title": "BME280 SENSOR",
   "note": "I2C mode on IO21/IO22, address 0x76, 4.7k bus pull-ups",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "R7"
       },
       {
        "part": "R8"
       }
      ],
      "gap": 5
     },
     {
      "part": "U5"
     },
     {
      "part": "R17"
     },
     {
      "col": [
       {
        "part": "C12"
       },
       {
        "part": "C13"
       }
      ],
      "gap": 5
     }
    ],
    "gap": 7
   }
  },
  {
   "title": "MICROSD STORAGE",
   "note": "SPI mode with 33R series damping and 10k pull-ups on CS, MOSI and detect",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "R9"
       },
       {
        "part": "R10"
       },
       {
        "part": "R11"
       }
      ],
      "gap": 5
     },
     {
      "col": [
       {
        "part": "R12"
       },
       {
        "part": "R13"
       },
       {
        "part": "R14"
       }
      ],
      "gap": 5
     },
     {
      "part": "J2"
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
      "gap": 5
     }
    ],
    "gap": 7
   }
  },
  {
   "title": "LEDS AND IO HEADERS",
   "note": "Red power and blue IO25 status LEDs; spare GPIO on two edge headers",
   "tree": {
    "row": [
     {
      "col": [
       {
        "row": [
         {
          "part": "R15"
         },
         {
          "part": "D1"
         }
        ],
        "gap": 4
       },
       {
        "row": [
         {
          "part": "R16"
         },
         {
          "part": "D2"
         }
        ],
        "gap": 4
       }
      ],
      "gap": 5
     },
     {
      "part": "J3"
     },
     {
      "part": "J4"
     }
    ],
    "gap": 6
   }
  }
 ]
}
```

## Checklist

- USB-C sink: independent 5.1 k pulldowns on CC1 and CC2, all four VBUS and all four GND pins
  tied through the name keys, `SHIELD` to GND, `SBU1`/`SBU2` no-connect.
- VBUS goes through a 500 mA polyfuse to +5V, which feeds the AP2112K (EN tied to VIN) and the
  CP2102N `VREGIN`/`VBUS` pins; +5V carries a 10 uF plus the bridge's 4.7 uF and 100 nF.
- USBLC6-2SC6 sits directly on the USB-C D+/D- nets, before the CP2102N. No series resistors
  are needed on a USB 2.0 full-speed bridge.
- The 3.3 V rail gets 10 uF in, 10 uF out and 100 nF; the ESP32 additionally gets a 10 uF bulk
  and two local 100 nF caps.
- Auto-program: Q1 base from RTS through 10 k, emitter on DTR, collector on EN; Q2 base from DTR
  through 10 k, emitter on RTS, collector on IO0. The cross-coupled emitters are what keeps RTS
  and DTR from ever being shorted; a pair of transistors both referenced to GND would do that.
- EN: 10 k pull-up, 1 uF delay cap, reset button to GND. IO0: 10 k pull-up, boot button to GND.
- Cross the UART: bridge `TXD` drives `ESP_RXD` (module `RXD0/IO3`) and bridge `RXD` listens on
  `ESP_TXD` (module `TXD0/IO1`).
- BME280 in I2C mode: `SDI` = SDA on IO21, `SCK` = SCL on IO22, `CSB` tied high, `SDO` pulled
  low through R17 for address 0x76, and both `VDD` and `VDDIO` decoupled with 100 nF.
  4.7 k pull-ups on SDA and SCL.
- microSD in SPI mode: IO18 SCK, IO19 MISO, IO23 MOSI, IO5 CS, mapped onto `CLK`, `DAT0`, `CMD`
  and `DAT3/CD`. Put the 33 R series resistors on the MCU side and the 10 k pull-ups on the card
  side of SCK/MOSI/CS, wire `DET` to IO4 with its own 10 k pull-up, and ground `SHIELD`.
  `DAT1`/`DAT2` are unused in SPI mode - mark them `nc`.
- Every genuinely unused module pin is `nc`, and every part carries `"pins_default": "nc"` only
  where the symbol has hidden no-connect pins (U2, U3, U4).
- `"flags": ["VBUS", "+5V", "GND"]`: +5V only comes through the fuse and GND only from the
  connector, so both need a PWR_FLAG; +3V3 is driven by the AP2112K output and does not.

## Common mistakes

- Giving the ESP32 pin 32, the CP2102N pin 10 or the AP2112K pin 4 a key. They are hidden
  `no_connect` pins that the layout engine does not instantiate, and the build fails with
  "no pin 'NC'". Use `"pins_default": "nc"` on those three parts.
- Writing `RXD0` or `IO3` instead of the full pin name `RXD0/IO3`.
- Tying BME280 `SDO` straight to a GND power symbol: that connects a bidirectional pin to the
  PWR_FLAG and ERC warns. Strap it with a 10 k resistor.
- Wiring TXD to TXD. The bridge and the module must cross.
- Putting both auto-program transistor emitters on GND, which shorts RTS to DTR through the
  base resistors and makes the board reset whenever the port is opened.
- Powering the CP2102N `VDD` from its own internal regulator while an AP2112K also drives the
  same net; tie `VDD` and the board rail together only through the external regulator.
- One shared 5.1 k CC resistor, or leaving `SHIELD` floating.
