---
name: bms-10s
description: 10-series Li-ion battery management board around a BQ76930DBT AFE - ten filtered cell channels, back-to-back CHG/DSG PowerPAK NMOS protection, 2 mOhm Kelvin shunt, MCP1799 3.3 V rail, I2C/ALERT interface, fused and TVS-clamped load output.
triggers: ["bms", "battery management", "bq76930", "bq769x0", "10s", "cell balancing", "li-ion pack", "afe monitor", "cell tap"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `Battery_Management:BQ76930DBT` | `Package_SO:TSSOP-30_4.4x7.8mm_P0.5mm` | 6-10 cell AFE |
| J1 | `Connector_Generic:Conn_01x11` | `Connector_PinHeader_2.54mm:PinHeader_1x11_P2.54mm_Vertical` | B-, C1..C10 taps |
| R1-R10 | `Device:R` | `Resistor_SMD:R_0805_2012Metric` | 100 R balancing / current limit |
| R11-R20 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100 R cell-tap filter |
| C1-C10 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100 nF differential cell filter |
| C11, C12, C13 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1 uF CAP1 / CAP2 / REGOUT |
| R21, C14 | `Device:R` / `Device:C` | `R_0805_2012Metric` / `C_0603_1608Metric` | 1 k + 100 nF BAT / REGSRC feed |
| RT1, RT2 | `Device:Thermistor_NTC` | `Resistor_SMD:R_0805_2012Metric` | 10 k NTC on TS1 / TS2 |
| U2 | `Regulator_Linear:MCP1799x-330xxTT` | `Package_TO_SOT_SMD:SOT-23` | 3.3 V HV LDO |
| C15, C16 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1 uF LDO in / out |
| R22, R23, R24 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10 k SDA / SCL / ALERT pull-ups |
| J4 | `Connector_Generic:Conn_01x05` | `Connector_PinHeader_2.54mm:PinHeader_1x05_P2.54mm_Vertical` | I2C + ALERT header |
| Q1, Q2 | `Transistor_FET:Q_NMOS_GSD` | `Package_SO:PowerPAK_SO-8_Single` | DSG / CHG protection FETs |
| R25, R26 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100 R gate resistors |
| R27, R28 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1 M gate-source pulldowns |
| RS1 | `Device:R_Shunt` | `Resistor_SMD:R_Shunt_Vishay_WSK2512_6332Metric_T1.19mm` | 2 mOhm Kelvin shunt |
| R29, R30, C17 | `Device:R` / `Device:C` | `R_0603_1608Metric` / `C_0603_1608Metric` | 100 R / 100 R / 100 nF sense filter |
| J2, J3 | `Connector_Generic:Conn_01x02` | `TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal` | PACK / LOAD terminals |
| F1 | `Device:Fuse` | `Fuse:Fuse_1206_3216Metric` | 5 A series fuse |
| D1 | `Device:D_TVS` | `Diode_SMD:D_SMB` | SMBJ43CA bidirectional |

Pin-key notes confirmed against the installed libraries. `BQ76930DBT` is a single-unit TSSOP-30
symbol; its pin NAMES are `DSG`(1), `CHG`(2), `VSS`(3), `SDA`(4), `SCL`(5), `TS1`(6), `CAP1`(7),
`REGOUT`(8), `REGSRC`(9), `VC5X`(10), `NC(CAP2)`(11 and 12 - one name key covers both), `TS2`(13),
`CAP2`(14), `BAT`(15), `VC10`(16) down to `VC6`(20), `VC5B`(21), `VC5`(22), `VC4`(23) down to
`VC0`(27), `SRP`(28), `SRN`(29), `ALERT`(30). There is no `VDD`, no `VCC` and no second ground.
`Device:R_Shunt` has FOUR pins: 1 and 4 are the high-current terminals, 2 and 3 are the Kelvin taps
(2 sits beside pin 1, 3 beside pin 4). `Transistor_FET:Q_NMOS_GSD` pins are `G`(1), `S`(2), `D`(3).
`MCP1799x-330xxTT` pins are `GND`(1), `VO`(2), `VI`(3). `Device:D_TVS` pins are `A1`, `A2`.
`Device:Fuse` and `Device:Thermistor_NTC` have unnamed pins `1` / `2`.

## Pin map

```
+3V3: U2.VO, C16.1, R22.1, R23.1, R24.1, J4.1
ALERT: U1.ALERT, R24.2, J4.4
CAP1: U1.CAP1, C11.1
CAP2: U1.CAP2, C12.1
CHG_DRV: U1.CHG, R26.1
CHG_G: Q2.G, R26.2, R28.1
CT1: J1.2, R1.1
CT10: J1.11, R10.1, R21.1, U2.VI, C15.1, J2.1, F1.1
CT2: J1.3, R2.1
CT3: J1.4, R3.1
CT4: J1.5, R4.1
CT5: J1.6, R5.1
CT6: J1.7, R6.1
CT7: J1.8, R7.1
CT8: J1.9, R8.1
CT9: J1.10, R9.1
DSG_DRV: U1.DSG, R25.1
DSG_G: Q1.G, R25.2, R27.1
FET_SRC: Q1.S, Q2.S, R27.2, R28.2
GND: J1.1, C1.2, U1.VSS, U1.VC0, C11.2, C13.2, C14.2, RT1.2, RT2.2, U2.GND, C15.2, C16.2, J4.5, RS1.1
LOAD_P: F1.2, D1.A1, J3.1
PACK_N: Q2.D, J2.2, D1.A2, J3.2
REGOUT: U1.REGOUT, C13.1
SCL: U1.SCL, R23.2, J4.3
SDA: U1.SDA, R22.2, J4.2
SHUNT_P: Q1.D, RS1.4
SRN: U1.SRN, R29.2, C17.2
SRN_K: RS1.2, R29.1
SRP: U1.SRP, R30.2, C17.1
SRP_K: RS1.3, R30.1
TAP1: R1.2, R11.1
TAP10: R10.2, R20.1
TAP2: R2.2, R12.1
TAP3: R3.2, R13.1
TAP4: R4.2, R14.1
TAP5: R5.2, R15.1
TAP6: R6.2, R16.1
TAP7: R7.2, R17.1
TAP8: R8.2, R18.1
TAP9: R9.2, R19.1
TS1: U1.TS1, RT1.1
TS2: U1.TS2, RT2.1
VBAT_S: U1.REGSRC, U1.BAT, R21.2, C14.1
VC1: R11.2, C1.1, C2.2, U1.VC1
VC10: R20.2, C10.1, U1.VC10
VC2: R12.2, C2.1, C3.2, U1.VC2
VC3: R13.2, C3.1, C4.2, U1.VC3
VC4: R14.2, C4.1, C5.2, U1.VC4
VC5: R15.2, C5.1, C6.2, U1.VC5X, U1.VC5B, U1.VC5, C12.2
VC6: R16.2, C6.1, C7.2, U1.VC6
VC7: R17.2, C7.1, C8.2, U1.VC7
VC8: R18.2, C8.1, C9.2, U1.VC8
VC9: R19.2, C9.1, C10.2, U1.VC9
```

## Layout

The design JSON below verifies clean (`layout_errors=0 issues=0 erc=0`, 60 parts, A2).
Hand it to `build` as-is:

```json
{
 "title": "10S Li-Ion Battery Management - BQ76930",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A2",
 "comments": [
  "10-series BQ76930 monitor with CHG/DSG protection FETs",
  "Ten filtered cell channels, 2 mOhm Kelvin shunt, 3.3 V interface rail"
 ],
 "parts": [
  {"id": "J1","lib": "Connector_Generic:Conn_01x11","value": "CELL TAPS","footprint": "Connector_PinHeader_2.54mm:PinHeader_1x11_P2.54mm_Vertical","pins": {"1": "GND","2": "CT1","3": "CT2","4": "CT3","5": "CT4","6": "CT5","7": "CT6","8": "CT7","9": "CT8","10": "CT9","11": "CT10"}},
  {"id": "R1","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT1","2": "TAP1"}},
  {"id": "R2","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT2","2": "TAP2"}},
  {"id": "R3","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT3","2": "TAP3"}},
  {"id": "R4","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT4","2": "TAP4"}},
  {"id": "R5","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT5","2": "TAP5"}},
  {"id": "R6","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT6","2": "TAP6"}},
  {"id": "R7","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT7","2": "TAP7"}},
  {"id": "R8","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT8","2": "TAP8"}},
  {"id": "R9","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT9","2": "TAP9"}},
  {"id": "R10","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT10","2": "TAP10"}},
  {"id": "R11","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP1","2": "VC1"}},
  {"id": "R12","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP2","2": "VC2"}},
  {"id": "R13","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP3","2": "VC3"}},
  {"id": "R14","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP4","2": "VC4"}},
  {"id": "R15","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP5","2": "VC5"}},
  {"id": "R16","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP6","2": "VC6"}},
  {"id": "R17","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP7","2": "VC7"}},
  {"id": "R18","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP8","2": "VC8"}},
  {"id": "R19","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP9","2": "VC9"}},
  {"id": "R20","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "TAP10","2": "VC10"}},
  {"id": "C1","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC1","2": "GND"}},
  {"id": "C2","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC2","2": "VC1"}},
  {"id": "C3","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC3","2": "VC2"}},
  {"id": "C4","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC4","2": "VC3"}},
  {"id": "C5","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC5","2": "VC4"}},
  {"id": "C6","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC6","2": "VC5"}},
  {"id": "C7","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC7","2": "VC6"}},
  {"id": "C8","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC8","2": "VC7"}},
  {"id": "C9","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC9","2": "VC8"}},
  {"id": "C10","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VC10","2": "VC9"}},
  {"id": "U1","lib": "Battery_Management:BQ76930DBT","value": "BQ76930DBT","footprint": "Package_SO:TSSOP-30_4.4x7.8mm_P0.5mm","pins": {"DSG": "DSG_DRV","CHG": "CHG_DRV","VSS": "GND","SDA": "SDA","SCL": "SCL","TS1": "TS1","CAP1": "CAP1","REGOUT": "REGOUT","REGSRC": "VBAT_S","VC5X": "VC5","NC(CAP2)": "nc","TS2": "TS2","CAP2": "CAP2","BAT": "VBAT_S","VC5B": "VC5","VC0": "GND","SRP": "SRP","SRN": "SRN","ALERT": "ALERT","VC1": "VC1","VC2": "VC2","VC3": "VC3","VC4": "VC4","VC5": "VC5","VC6": "VC6","VC7": "VC7","VC8": "VC8","VC9": "VC9","VC10": "VC10"}},
  {"id": "C11","lib": "Device:C","value": "1uF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "CAP1","2": "GND"}},
  {"id": "C12","lib": "Device:C","value": "1uF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "CAP2","2": "VC5"}},
  {"id": "C13","lib": "Device:C","value": "1uF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "REGOUT","2": "GND"}},
  {"id": "R21","lib": "Device:R","value": "1k","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "CT10","2": "VBAT_S"}},
  {"id": "C14","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "VBAT_S","2": "GND"}},
  {"id": "RT1","lib": "Device:Thermistor_NTC","value": "10k NTC","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "TS1","2": "GND"}},
  {"id": "RT2","lib": "Device:Thermistor_NTC","value": "10k NTC","footprint": "Resistor_SMD:R_0805_2012Metric","pins": {"1": "TS2","2": "GND"}},
  {"id": "U2","lib": "Regulator_Linear:MCP1799x-330xxTT","value": "MCP1799-3.3","footprint": "Package_TO_SOT_SMD:SOT-23","pins": {"VI": "CT10","VO": "+3V3","GND": "GND"}},
  {"id": "C15","lib": "Device:C","value": "1uF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "CT10","2": "GND"}},
  {"id": "C16","lib": "Device:C","value": "1uF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "+3V3","2": "GND"}},
  {"id": "R22","lib": "Device:R","value": "10k","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "+3V3","2": "SDA"}},
  {"id": "R23","lib": "Device:R","value": "10k","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "+3V3","2": "SCL"}},
  {"id": "R24","lib": "Device:R","value": "10k","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "+3V3","2": "ALERT"}},
  {"id": "J4","lib": "Connector_Generic:Conn_01x05","value": "I2C / ALERT","footprint": "Connector_PinHeader_2.54mm:PinHeader_1x05_P2.54mm_Vertical","pins": {"1": "+3V3","2": "SDA","3": "SCL","4": "ALERT","5": "GND"}},
  {"id": "Q1","lib": "Transistor_FET:Q_NMOS_GSD","value": "DSG 100V NMOS","footprint": "Package_SO:PowerPAK_SO-8_Single","pins": {"G": "DSG_G","S": "FET_SRC","D": "SHUNT_P"}},
  {"id": "Q2","lib": "Transistor_FET:Q_NMOS_GSD","value": "CHG 100V NMOS","footprint": "Package_SO:PowerPAK_SO-8_Single","pins": {"G": "CHG_G","S": "FET_SRC","D": "PACK_N"}},
  {"id": "R25","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "DSG_DRV","2": "DSG_G"}},
  {"id": "R26","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "CHG_DRV","2": "CHG_G"}},
  {"id": "R27","lib": "Device:R","value": "1M","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "DSG_G","2": "FET_SRC"}},
  {"id": "R28","lib": "Device:R","value": "1M","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "CHG_G","2": "FET_SRC"}},
  {"id": "RS1","lib": "Device:R_Shunt","value": "2mR 2512","footprint": "Resistor_SMD:R_Shunt_Vishay_WSK2512_6332Metric_T1.19mm","pins": {"1": "GND","2": "SRN_K","3": "SRP_K","4": "SHUNT_P"}},
  {"id": "R29","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "SRN_K","2": "SRN"}},
  {"id": "R30","lib": "Device:R","value": "100R","footprint": "Resistor_SMD:R_0603_1608Metric","pins": {"1": "SRP_K","2": "SRP"}},
  {"id": "C17","lib": "Device:C","value": "100nF","footprint": "Capacitor_SMD:C_0603_1608Metric","pins": {"1": "SRP","2": "SRN"}},
  {"id": "J2","lib": "Connector_Generic:Conn_01x02","value": "PACK","footprint": "TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal","pins": {"1": "CT10","2": "PACK_N"}},
  {"id": "F1","lib": "Device:Fuse","value": "5A 1206","footprint": "Fuse:Fuse_1206_3216Metric","pins": {"1": "CT10","2": "LOAD_P"}},
  {"id": "D1","lib": "Device:D_TVS","value": "SMBJ43CA","footprint": "Diode_SMD:D_SMB","pins": {"A1": "LOAD_P","A2": "PACK_N"}},
  {"id": "J3","lib": "Connector_Generic:Conn_01x02","value": "LOAD","footprint": "TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal","pins": {"1": "LOAD_P","2": "PACK_N"}}
 ],
 "flags": ["GND","CT10","VBAT_S"],
 "notes": [
  "CT10 is the pack positive: cell tap 10, BAT/REGSRC feed, LDO input and the PACK+ terminal.",
  "VC5, VC5B and VC5X tie together for 10-series operation; pins 11/12 are true NC.",
  "Q1 and Q2 share a source node so the AFE can drive both gates against VSS.",
  "SRP and SRN come off the shunt's Kelvin pads only, never off the high-current pads."
 ],
 "layout": [
  {"title": "CELL TAPS AND FILTER BANK",
   "note": "Ten identical channels: 100R 0805 balancing resistor, 100R 0603 filter resistor, 100nF differential cap",
   "tree":
    {"row": [
      {"part": "J1"},
      {"col": [
        {"row": [{"part": "R10"},{"col": [{"part": "R20"},{"part": "C10"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R9"},{"col": [{"part": "R19"},{"part": "C9"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R8"},{"col": [{"part": "R18"},{"part": "C8"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R7"},{"col": [{"part": "R17"},{"part": "C7"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R6"},{"col": [{"part": "R16"},{"part": "C6"}],"gap": 8}],"gap": 8}
       ], "gap": 8},
      {"col": [
        {"row": [{"part": "R5"},{"col": [{"part": "R15"},{"part": "C5"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R4"},{"col": [{"part": "R14"},{"part": "C4"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R3"},{"col": [{"part": "R13"},{"part": "C3"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R2"},{"col": [{"part": "R12"},{"part": "C2"}],"gap": 8}],"gap": 8},
        {"row": [{"part": "R1"},{"col": [{"part": "R11"},{"part": "C1"}],"gap": 8}],"gap": 8}
       ], "gap": 8}
     ], "gap": 10}},
  {"title": "CELL MONITOR",
   "note": "BQ76930 AFE: CAP1/CAP2/REGOUT bypass, BAT/REGSRC feed through 1k, 10k NTC on TS1 and TS2",
   "tree":
    {"row": [
      {"col": [{"part": "R21"},{"part": "C14"}],"gap": 9},
      {"part": "U1"},
      {"col": [
        {"row": [{"part": "C11"},{"part": "C12"},{"part": "C13"}],"gap": 10},
        {"row": [{"part": "RT1"},{"part": "RT2"}],"gap": 10}
       ], "gap": 11}
     ], "gap": 12}},
  {"title": "3.3 V RAIL AND I2C",
   "note": "MCP1799 high-voltage LDO off the pack, 10k pull-ups on SDA, SCL and ALERT",
   "tree":
    {"row": [
      {"part": "C15"},
      {"part": "U2"},
      {"part": "C16"},
      {"col": [{"part": "R22"},{"part": "R23"},{"part": "R24"}],"gap": 7},
      {"part": "J4"}
     ], "gap": 12}},
  {"title": "CHG / DSG PROTECTION FETS",
   "note": "Back-to-back common-source NMOS in the pack-negative path; 100R gate resistors, 1M gate-source pulldowns",
   "tree":
    {"row": [
      {"col": [{"part": "R25"},{"part": "R27"}],"gap": 12},
      {"part": "Q1"},
      {"part": "Q2"},
      {"col": [{"part": "R26"},{"part": "R28"}],"gap": 12}
     ], "gap": 14}},
  {"title": "CURRENT SENSE",
   "note": "2 mOhm shunt with Kelvin taps into a symmetric 100R / 100nF differential filter",
   "tree":
    {"row": [{"part": "RS1"},{"col": [{"part": "R30"},{"part": "R29"}],"gap": 13},{"part": "C17"}],"gap": 14}},
  {"title": "PACK AND LOAD TERMINALS",
   "note": "Pack terminal, 5 A series fuse and a 43 V bidirectional TVS across the protected load output",
   "tree":
    {"row": [{"part": "J2"},{"part": "F1"},{"col": [{"part": "D1"},{"part": "J3"}],"gap": 10}],"gap": 12}}
 ]
}
```

## Checklist

- Ten channels, ten identical rows: `CT{i}` -> 100 R 0805 (R{i}) -> `TAP{i}` -> 100 R 0603 (R{10+i})
  -> `VC{i}`, with a 100 nF differential cap from `VC{i}` to `VC{i-1}` (C1 goes to GND).
- `VC0` and the AFE `VSS` both sit on GND, which is the battery negative `B-` and cell-tap pin 1.
- For 10-series operation `VC5`, `VC5B` and `VC5X` are ONE net at the top of cell 5. Pins 11/12
  (`NC(CAP2)`) are the only genuine no-connects on the part.
- Bypass CAP1 with 1 uF to VSS, CAP2 with 1 uF returned to `VC5` (not to ground), and REGOUT with
  1 uF to VSS.
- `BAT` and `REGSRC` share one node fed from the pack top through 1 k with 100 nF to VSS.
- Two 10 k NTCs, one from TS1 to VSS and one from TS2 to VSS. 10 k pull-ups to +3V3 on SDA, SCL and
  ALERT; the AFE has open-drain outputs and will not drive them.
- Protection FETs are common-source back-to-back: Q1 drain on the shunt output, Q2 drain on `PACK_N`,
  both sources on `FET_SRC`. That shared source is what lets the AFE drive both gates against VSS.
- Each gate gets a 100 R series resistor from the AFE pin and a 1 M pulldown to `FET_SRC`, so the
  FETs stay off before the AFE boots.
- `SRP`/`SRN` come off the shunt's Kelvin pins 3 and 2 only, each through 100 R, with one 100 nF
  differential cap between them. Never tap SRP/SRN from the high-current pins 1/4.
- `"flags": ["GND", "CT10", "VBAT_S"]` - GND arrives from a connector, and CT10 / VBAT_S feed power
  inputs (`U2.VI`, `U1.REGSRC`) through passives only, so all three need a PWR_FLAG.
- `CT10` is the pack positive: cell tap 11, the BAT/REGSRC feed, the LDO input and the PACK+
  terminal are all on it. The load output is `LOAD_P`, after the 5 A fuse, clamped to `PACK_N` by
  the 43 V bidirectional TVS.
- The sheet is A2 with 60 parts and one text-through-wire warning. Two cols of five channels keeps
  the array aligned and the block 116 x 185; one col of ten is 277 tall and empties half the sheet.

## Common mistakes

- Making the cell channels ten separate blocks, or scattering them across the sheet. They are ONE
  block whose tree is a col of five identical rows twice over; the rubric grades the array for
  consistent ordering and alignment.
- Wiring the differential filter caps from every `VC{i}` to ground. They must go cell-to-cell
  (`VC{i}` to `VC{i-1}`) or the filter loads each tap against the whole stack.
- Marking `VC5B` or `VC5X` as no-connect. Only pins 11 and 12 are NC on this part; leaving VC5B
  floating breaks the upper bank.
- Putting the two FETs drain-to-drain, or giving them separate source nets. Then the CHG/DSG outputs,
  which are referenced to VSS, cannot enhance the upper device.
- Using `Resistor_SMD:R_2512_6332Metric` for the shunt: it has two pads while `Device:R_Shunt` has
  four pins, so the Kelvin pins land nowhere. `R_Shunt_Vishay_WSK2512_6332Metric_T1.19mm` is the
  same 2512 body with four pads.
- Forgetting the PWR_FLAGs on `CT10` and `VBAT_S`; ERC then reports the LDO input and REGSRC as
  power inputs with no driver.
- Returning the CAP2 bypass capacitor to ground instead of `VC5`.
- Numbering the cell-tap connector the other way round: pin 1 is B-, pin 11 is C10.
