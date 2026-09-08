---
name: sallen-key-filter
description: 1 kHz second-order Sallen-Key low-pass filter on an LM358 with a gain-of-10 non-inverting output stage, single 9V battery, resistor-divider virtual ground, 3.5mm input/output jacks.
triggers: ["sallen-key", "sallen key filter", "sallen-key low-pass", "active low-pass filter", "second-order low-pass", "op-amp filter", "lm358 filter", "9v battery filter", "virtual ground filter", "audio low-pass filter", "gain of 10 amplifier"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| BT1 | `Device:Battery` | `Battery:BatteryHolder_MPD_BA9VPC_1xPP3` | 9V |
| R1, R2, R10 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 100k |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1M |
| R4, R5 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 15.8k |
| R6, R8 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 10k |
| R7 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 5.90k |
| R9 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 90k |
| C1 | `Device:C_Polarized` | `Capacitor_SMD:C_0805_2012Metric` | 47uF |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF |
| C3 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 1uF |
| C4, C5 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 10nF |
| C6 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 10uF |
| U1 | `Amplifier_Operational:LM358` | `Package_DIP:DIP-8_W7.62mm` | LM358 |
| J1, J2 | `Connector_Audio:AudioJack2` | `Connector_Audio:Jack_3.5mm_CUI_SJ1-3523N_Horizontal` | 3.5mm mono jack |

Pin-key notes verified against the installed libraries: `Device:Battery` pins are `1`(+)/`2`(-).
`Amplifier_Operational:LM358` is 3 units - unit 1 pins `1`(out)/`2`(-)/`3`(+), unit 2 pins
`7`(out)/`6`(-)/`5`(+), unit 3 (power) pins `8`(V+)/`4`(V-); instantiate all three units even
though only two op-amps are used electrically, because unit 3 carries the supply pins.
`Connector_Audio:AudioJack2` has exactly two pins, `T` (tip, signal) and `S` (sleeve, ground) -
it has no ring pin; do not invent a `R` pin. `Device:BatteryHolder_Keystone_1220_1x12mm` and
`Connector_Audio:Jack_3.5mm` footprints referenced by older drafts of this design do not exist
in KiCad 10 and must never be used - see Common mistakes.

## Pin map

```
+9V: BT1.1, R1.1, C2.1, U1.8
FILTER_FB: U1.2, R6.1, R7.2
FILTER_IN: C3.2, R3.1, R4.1
FILTER_OUT: C5.2, U1.1, R7.1, U1.5
FILTER_X: R4.2, R5.1, C5.1
FILTER_Y: R5.2, C4.1, U1.3
GAIN_FB: U1.6, R8.1, R9.2
GAIN_OUT: U1.7, R9.1, C6.1
GND: BT1.2, R2.2, C1.2, C2.2, J1.S, R10.2, J2.S, U1.4
IN_AC: J1.T, C3.1
OUT_AC: C6.2, R10.1, J2.T
VREF: R1.2, R2.1, C1.1, R3.2, C4.2, R6.2, R8.2
```

## Layout

The complete design JSON below builds with 0 layout errors, 0 issues and 0 ERC violations. Hand
it to `build` as-is, adapting values and jack footprints to the request:

```json
{
 "title": "1 kHz Sallen-Key LPF with Gain",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "9 V single-supply analog filter",
  "Virtual ground biased at 4.5 V",
  "Second-order low-pass followed by gain of 10"
 ],
 "parts": [
  {"id": "BT1", "lib": "Device:Battery", "value": "9V", "footprint": "Battery:BatteryHolder_MPD_BA9VPC_1xPP3", "pins": {"1": "+9V", "2": "GND"}},
  {"id": "R1", "lib": "Device:R", "value": "100k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "+9V", "2": "VREF"}},
  {"id": "R2", "lib": "Device:R", "value": "100k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "VREF", "2": "GND"}},
  {"id": "C1", "lib": "Device:C_Polarized", "value": "47uF", "footprint": "Capacitor_SMD:C_0805_2012Metric", "pins": {"1": "VREF", "2": "GND"}},
  {"id": "C2", "lib": "Device:C", "value": "100nF", "footprint": "Capacitor_SMD:C_0603_1608Metric", "pins": {"1": "+9V", "2": "GND"}},
  {"id": "J1", "lib": "Connector_Audio:AudioJack2", "value": "INPUT 3.5mm", "footprint": "Connector_Audio:Jack_3.5mm_CUI_SJ1-3523N_Horizontal", "pins": {"T": "IN_AC", "S": "GND"}},
  {"id": "C3", "lib": "Device:C", "value": "1uF", "footprint": "Capacitor_SMD:C_0603_1608Metric", "pins": {"1": "IN_AC", "2": "FILTER_IN"}},
  {"id": "R3", "lib": "Device:R", "value": "1M", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "FILTER_IN", "2": "VREF"}},
  {"id": "R4", "lib": "Device:R", "value": "15.8k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "FILTER_IN", "2": "FILTER_X"}},
  {"id": "R5", "lib": "Device:R", "value": "15.8k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "FILTER_X", "2": "FILTER_Y"}},
  {"id": "C4", "lib": "Device:C", "value": "10nF", "footprint": "Capacitor_SMD:C_0603_1608Metric", "pins": {"1": "FILTER_Y", "2": "VREF"}},
  {"id": "C5", "lib": "Device:C", "value": "10nF", "footprint": "Capacitor_SMD:C_0603_1608Metric", "pins": {"1": "FILTER_X", "2": "FILTER_OUT"}},
  {"id": "U1", "lib": "Amplifier_Operational:LM358", "unit": 1, "value": "LM358", "footprint": "Package_DIP:DIP-8_W7.62mm", "pins": {"3": "FILTER_Y", "2": "FILTER_FB", "1": "FILTER_OUT"}},
  {"id": "R6", "lib": "Device:R", "value": "10k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "FILTER_FB", "2": "VREF"}},
  {"id": "R7", "lib": "Device:R", "value": "5.90k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "FILTER_OUT", "2": "FILTER_FB"}},
  {"id": "U1", "lib": "Amplifier_Operational:LM358", "unit": 2, "value": "LM358", "pins": {"5": "FILTER_OUT", "6": "GAIN_FB", "7": "GAIN_OUT"}},
  {"id": "R8", "lib": "Device:R", "value": "10k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "GAIN_FB", "2": "VREF"}},
  {"id": "R9", "lib": "Device:R", "value": "90k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "GAIN_OUT", "2": "GAIN_FB"}},
  {"id": "C6", "lib": "Device:C", "value": "10uF", "footprint": "Capacitor_SMD:C_0603_1608Metric", "pins": {"1": "GAIN_OUT", "2": "OUT_AC"}},
  {"id": "R10", "lib": "Device:R", "value": "100k", "footprint": "Resistor_SMD:R_0603_1608Metric", "pins": {"1": "OUT_AC", "2": "GND"}},
  {"id": "J2", "lib": "Connector_Audio:AudioJack2", "value": "OUTPUT 3.5mm", "footprint": "Connector_Audio:Jack_3.5mm_CUI_SJ1-3523N_Horizontal", "pins": {"T": "OUT_AC", "S": "GND"}},
  {"id": "U1", "lib": "Amplifier_Operational:LM358", "unit": 3, "value": "LM358", "pins": {"8": "+9V", "4": "GND"}}
 ],
 "layout": [
  {
   "title": "POWER / VIRTUAL GROUND",
   "note": "100k divider and 47uF bypass create the 4.5 V signal reference",
   "tree": {
    "row": [
     {"part": "BT1"},
     {"row": [{"part": "C2"}, {"part": "U1", "unit": 3}], "gap": 6},
     {"part": "R1"},
     {"col": [{"part": "R2"}, {"part": "C1"}], "gap": 5}
    ],
    "gap": 6
   }
  },
  {
   "title": "FILTER",
   "note": "Equal 15.8k / 10nF Sallen-Key values set fc near 1 kHz; gain 1.59 sets Butterworth Q",
   "tree": {
    "row": [
     {"part": "J1"},
     {"col": [{"part": "C3"}, {"part": "R3"}], "gap": 5},
     {"col": [{"part": "R4"}, {"part": "C5"}], "gap": 5},
     {"col": [{"part": "R5"}, {"part": "C4"}], "gap": 5},
     {"part": "U1", "unit": 1},
     {"col": [{"part": "R7"}, {"part": "R6"}], "gap": 5}
    ],
    "gap": 6
   }
  },
  {
   "title": "GAIN / OUTPUT",
   "note": "Non-inverting stage gain is 1 + 90k/10k = 10; C6 removes the VREF DC bias",
   "tree": {
    "row": [
     {"col": [{"part": "R9"}, {"part": "R8"}], "gap": 5},
     {"part": "U1", "unit": 2},
     {"part": "C6"},
     {"col": [{"part": "J2"}, {"part": "R10"}], "gap": 5}
    ],
    "gap": 7
   }
  }
 ],
 "flags": ["+9V", "GND"],
 "notes": [
  "Signal path is referenced to VREF; input and output jacks remain ground referenced.",
  "Filter passband gain is 1.59, followed by an independent gain-of-10 amplifier.",
  "Use a shielded enclosure and keep the high-impedance FILTER_IN node short."
 ]
}
```

## Checklist

- Instantiate all three LM358 units - two used as op-amps plus the power unit (`4`/`8`) - even
  though the symbol has only two amplifier sections; the power pins live on unit 3.
- `"flags": ["+9V", "GND"]`: +9V is driven only by the battery's `+` pin and GND only by its `-`
  pin, so both need a PWR_FLAG or ERC reports no driver.
- Virtual ground (VREF) is a signal net, not a power net - do not add it to `flags` or `power`;
  it is biased through R1/R2 and bypassed by C1, not drawn with a power symbol.
- Every op-amp non-inverting input that should sit at VREF (R3, C4, R8) actually returns to VREF,
  not GND - a single-supply stage referenced to GND instead of VREF will clip immediately.
- C3 (1 uF) blocks the DC on the input jack from loading VREF through R3; C6 (10 uF) blocks the
  VREF DC bias from reaching the output jack - both are electrolytic-sized values, not 0603 NP0.
- R3 (1 M) biases FILTER_IN to VREF; keep it much larger than the Sallen-Key resistors (15.8k)
  so it does not load the filter's cutoff.
- Equal-component Sallen-Key: R4 = R5 and C4 = C5 gives a Butterworth response when the stage
  gain (1 + R7/R6 = 1.59) is set correctly - do not let R6/R7 drift from that ratio.
- C1 (47 uF) across R2 bypasses the virtual-ground divider so it presents a low impedance at
  audio frequencies; without it VREF wobbles with the signal and both stages distort.
- C2 (100 nF) decouples the +9V rail right at the LM358's power pins.
- J1/J2 use `AudioJack2` (T/S only, no ring) - correct for a mono TS 3.5mm jack; do not wire a
  ring pin that the symbol does not have.
- Power symbol orientation (+9V up, GND down) is automatic for those net names; no manual "rot"
  needed on BT1 or the power pins.

## Common mistakes

- Using `Battery:BatteryHolder_Keystone_1220_1x12mm` or `Connector_Audio:Jack_3.5mm` - neither
  footprint exists in KiCad 10. Use `Battery:BatteryHolder_MPD_BA9VPC_1xPP3` (a real PP3/9V clip)
  and `Connector_Audio:Jack_3.5mm_CUI_SJ1-3523N_Horizontal`.
- Assuming `AudioJack2` has a ring pin (TRS) - it is TS only, pins `T` and `S`.
- Referencing the filter's bias resistors to GND instead of VREF, which removes headroom on a
  single 9V supply and clips the signal on one half-cycle.
- Forgetting the LM358 power unit (unit 3) - an op-amp entry with no `4`/`8` pins leaves the
  supply unconnected and ERC silent about it (the pins are simply missing from the netlist).
- Skipping the input/output DC-blocking caps (C3, C6), which would put the 4.5 V bias directly
  on the jacks and into whatever is plugged in.
- Omitting the PWR_FLAG on +9V/GND, which fails ERC with "no driver" even though the battery
  is physically the driver - KiCad still wants an explicit flag on a supply from a connector.
- Splitting the filter's two RC stages and the op-amp into separate blocks - they are one
  signal path and belong in a single FILTER row so wires stay short and aligned.
