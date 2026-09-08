---
name: ne555-blinker-ldo
description: NE555 astable ~1 Hz LED blinker (RED, current-limited) powered by an L7805 5 V LDO fed from a 12 V barrel jack, with input/output caps and a steady power LED.
triggers: ["ne555", "555 timer", "555 blinker", "astable", "led blinker", "7805", "l7805", "barrel jack 12v", "blinking led circuit"]
---

## Parts

| Ref | Symbol | Footprint | Value |
| --- | --- | --- | --- |
| U1 | `Timer:NE555P` | `Package_DIP:DIP-8_W7.62mm` | NE555 |
| R1 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k (VCC to DISCH) |
| R2 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 68k (DISCH to TRIG) |
| C1 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 10uF timing cap (TRIG to GND) |
| C2 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 10nF CONT bypass |
| R3 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 330 blink LED series |
| D1 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | RED blink LED |
| J1 | `Connector:Barrel_Jack` | `Connector_BarrelJack:BarrelJack_CUI_PJ-063AH_Horizontal` | 12 V IN |
| C3 | `Device:C` | `Capacitor_THT:CP_Radial_D6.3mm_P2.50mm` | 100uF input bulk |
| C4 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF input bypass |
| U2 | `Regulator_Linear:L7805` | `Package_TO_SOT_THT:TO-220-3_Vertical` | L7805 |
| C5 | `Device:C` | `Capacitor_SMD:C_0805_2012Metric` | 10uF output bulk |
| C6 | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | 100nF output bypass |
| R4 | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | 1k power LED series |
| D2 | `Device:LED` | `LED_SMD:LED_0603_1608Metric` | GREEN power LED |

Pin-key notes verified against the installed libraries: `Timer:NE555P` has separate `TRIG`/`THRES`
pins (tie both to the timing node) and `~{RST}` (tie to `+5V` when unused - never leave a reset
pin floating). `Device:LED` pin 1 is `K` (cathode) and pin 2 is `A` (anode) - the anode must sit on
the higher-potential side of the series resistor and the cathode on `GND`, else the LED never
forward-biases. `Connector:Barrel_Jack` pins are `1`/`2` (tip/sleeve); pin 1 is the positive tip.
`Regulator_Linear:L7805` pins are `IN`/`GND`/`OUT`.

Timing: `f = 1.44 / ((R1 + 2*R2) * C1)`. With R1=1k, R2=68k, C1=10uF: f ~= 1.05 Hz,
t_high = 0.693*(R1+R2)*C1 ~= 0.478 s, t_low = 0.693*R2*C1 ~= 0.471 s (~50% duty). R1 stays small
(1k) relative to R2 so duty cycle sits near 50%; making R1 comparable to R2 skews it toward 100%
high.

## Pin map

```
+5V: U1.VCC, U1.~{RST}, R1.1, U2.OUT, C5.1, C6.1, R4.1
CV: U1.CONT, C2.1
DIS: U1.DISCH, R1.2, R2.1
GND: U1.GND, C1.2, C2.2, D1.1, J1.2, C3.2, C4.2, U2.GND, C5.2, C6.2, D2.1
LED_A: R3.2, D1.2
OUT: U1.OUT, R3.1
PWR_LED_A: R4.2, D2.2
TRIG: U1.TRIG, U1.THRES, R2.2, C1.1
VIN: J1.1, C3.1, C4.1, U2.IN
```

## Layout

The complete design JSON below builds with 0 issues and 0 ERC violations (one benign warning: a
wire runs through the `+5V` power-flag text by 0.2 mm, inherent to the engine's power-symbol
placement above a rail's first stacked pin, not a connectivity problem). Hand it to `build` as-is,
adapting values to the request:

```json
{
 "title": "NE555 1 Hz LED Blinker with 5V LDO",
 "rev": "1.0",
 "date": "2026-09-08",
 "company": "Gordian EDA",
 "paper": "A4",
 "comments": [
  "NE555 astable ~1 Hz blinker, 5V LDO from 12V barrel jack"
 ],
 "parts": [
  {
   "id": "U1",
   "lib": "Timer:NE555P",
   "value": "NE555",
   "footprint": "Package_DIP:DIP-8_W7.62mm",
   "pins": {
    "GND": "GND",
    "VCC": "+5V",
    "TRIG": "TRIG",
    "THRES": "TRIG",
    "DISCH": "DIS",
    "OUT": "OUT",
    "~{RST}": "+5V",
    "CONT": "CV"
   }
  },
  {
   "id": "R1",
   "lib": "Device:R",
   "value": "1k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "DIS"
   }
  },
  {
   "id": "R2",
   "lib": "Device:R",
   "value": "68k",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "DIS",
    "2": "TRIG"
   }
  },
  {
   "id": "C1",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "TRIG",
    "2": "GND"
   }
  },
  {
   "id": "C2",
   "lib": "Device:C",
   "value": "10nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "CV",
    "2": "GND"
   }
  },
  {
   "id": "R3",
   "lib": "Device:R",
   "value": "330",
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "OUT",
    "2": "LED_A"
   }
  },
  {
   "id": "D1",
   "lib": "Device:LED",
   "value": "RED BLINK",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "1": "GND",
    "2": "LED_A"
   }
  },
  {
   "id": "J1",
   "lib": "Connector:Barrel_Jack",
   "value": "12V IN",
   "footprint": "Connector_BarrelJack:BarrelJack_CUI_PJ-063AH_Horizontal",
   "pins": {
    "1": "VIN",
    "2": "GND"
   }
  },
  {
   "id": "C3",
   "lib": "Device:C",
   "value": "100uF",
   "footprint": "Capacitor_THT:CP_Radial_D6.3mm_P2.50mm",
   "pins": {
    "1": "VIN",
    "2": "GND"
   }
  },
  {
   "id": "C4",
   "lib": "Device:C",
   "value": "100nF",
   "footprint": "Capacitor_SMD:C_0603_1608Metric",
   "pins": {
    "1": "VIN",
    "2": "GND"
   }
  },
  {
   "id": "U2",
   "lib": "Regulator_Linear:L7805",
   "value": "L7805",
   "footprint": "Package_TO_SOT_THT:TO-220-3_Vertical",
   "pins": {
    "IN": "VIN",
    "GND": "GND",
    "OUT": "+5V"
   }
  },
  {
   "id": "C5",
   "lib": "Device:C",
   "value": "10uF",
   "footprint": "Capacitor_SMD:C_0805_2012Metric",
   "pins": {
    "1": "+5V",
    "2": "GND"
   }
  },
  {
   "id": "C6",
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
   "footprint": "Resistor_SMD:R_0603_1608Metric",
   "pins": {
    "1": "+5V",
    "2": "PWR_LED_A"
   }
  },
  {
   "id": "D2",
   "lib": "Device:LED",
   "value": "PWR GREEN",
   "footprint": "LED_SMD:LED_0603_1608Metric",
   "pins": {
    "1": "GND",
    "2": "PWR_LED_A"
   }
  }
 ],
 "flags": [
  "VIN",
  "GND"
 ],
 "notes": [
  "R1=1k, R2=68k, C1=10uF gives f = 1.44/((R1+2R2)*C1) ~= 1.05 Hz, ~50% duty cycle.",
  "D1 lights during t_high (OUT sourcing); D2 is a steady 5V-rail power indicator.",
  "L7805 needs Vin-Vout >= 2V dropout; 12V in leaves ample margin for the 5V rail."
 ],
 "layout": [
  {
   "title": "POWER",
   "note": "12V barrel jack, bulk + bypass caps, L7805 5V regulator, power LED",
   "tree": {
    "row": [
     {
      "part": "J1"
     },
     {
      "col": [
       {
        "part": "C3"
       },
       {
        "part": "C4"
       }
      ],
      "gap": 6
     },
     {
      "part": "U2"
     },
     {
      "col": [
       {
        "part": "C6"
       },
       {
        "part": "C5"
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
        "part": "D2"
       }
      ],
      "gap": 5
     }
    ],
    "gap": 10
   }
  },
  {
   "title": "TIMER",
   "note": "NE555 astable ~1 Hz, R3/D1 current-limited blink LED on OUT",
   "tree": {
    "row": [
     {
      "col": [
       {
        "part": "R1"
       },
       {
        "part": "R2"
       },
       {
        "part": "C1"
       }
      ],
      "gap": 6
     },
     {
      "part": "U1"
     },
     {
      "col": [
       {
        "part": "C2"
       },
       {
        "row": [
         {
          "part": "R3"
         },
         {
          "part": "D1"
         }
        ],
        "gap": 6
       }
      ],
      "gap": 10
     }
    ],
    "gap": 12
   }
  }
 ]
}
```

## Checklist

- `flags: ["VIN", "GND"]` - VIN is only ever driven by the barrel jack and GND only enters through
  the jack, so both need a PWR_FLAG or ERC reports no-driver.
- LED orientation: anode toward the resistor/rail side, cathode toward GND (`Device:LED` pin 1 =
  K, pin 2 = A). Check both D1 and D2 - a swapped LED still passes ERC but never lights.
- D1's series resistor (R3) ties directly to OUT, not to a rail - the blink LED lights only while
  OUT sources current (t_high), which is the intended ~1 Hz flash.
- Tie `~{RST}` to `+5V` - a floating reset pin holds the 555 in permanent reset.
- `THRES` and `TRIG` are two separate pins on the symbol; both must land on the same timing node
  (here `TRIG`) even though the datasheet calls them one function.
- Size R1 much smaller than R2 (e.g. 1k vs 68k) to keep duty cycle near 50%; R1 comparable to R2
  skews the waveform toward mostly-high.
- Bulk + bypass cap pair on both sides of the LDO: input (100uF + 100nF) absorbs barrel-jack
  ripple, output (10uF + 100nF) keeps the 555 supply quiet during switching.
- L7805 needs >=2 V headroom (Vin - Vout); 12 V in against a 5 V rail leaves 7 V of margin, well
  above dropout.
- Barrel jack pin 1 is the positive tip - do not swap it with pin 2 (sleeve/GND) or polarity
  reverses at the LDO input.
- R3 and R4 both set LED current (`(Vsupply - Vf) / R`); at 330 R and 1k respectively with a
  ~5 V rail and ~2 V LED Vf, currents land near 9 mA and 3 mA - reasonable for 0603 LEDs.
- Keep the power LED (R4/D2) in the POWER block, not the TIMER block - it indicates the 5 V rail,
  not the oscillator.

## Common mistakes

- Wiring an LED with anode to GND and cathode to the series resistor - it will never forward-bias
  regardless of the 555's output state; always check pin 1=K/pin 2=A against the net it lands on.
- Omitting the `flags` list, leaving ERC to flag VIN or GND as undriven.
- Leaving `~{RST}` floating instead of tying it to `+5V`.
- Choosing R1 too large relative to R2 (e.g. equal values) for a "1 Hz, 50% duty" request - the
  astable formula makes t_high always longer than t_low unless R1 << R2.
- Using a 9 V-only barrel jack part or a jack footprint too small for a 12 V wall-adapter plug -
  keep the value at "12V IN" and use a standard 5.5/2.1 mm barrel footprint.
- Only decoupling one side of the LDO - both VIN and VOUT need their own bulk + bypass caps.
- Sharing one series resistor between the blink LED and the power LED - they are on different
  nets (`OUT` vs `+5V`) and need independent current-limiting resistors.
