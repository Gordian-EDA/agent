# Schematic suite baseline (2026-09-03, `--suite schematic --jobs 2 --max-turns 3`)

First run of the schematic-only benchmark on current main (`f9d15f55`), agent
`gordian` release build, `gpt-5.6-luna`. Every critic score is the calibrated,
anchored one: three reads, modal, against a human sheet rated 9 — for a
`dataset-*` case its own human original, elsewhere `quality/anchor/schematic-9.png`.

## The 14 cases

| case | parts | source |
| ---- | ----- | ------ |
| dataset-ddr-memory-411be040 | 27 | `411be040b743` DDR memory subsystem, dual DRAM, dense decoupling |
| dataset-light-accessory-266db471 | 21 | `266db4714ada` Minuet light-accessory level shifter |
| dataset-ibm-m122-261071e7 | 24 | `261071e743d8` IBM M122 keyboard interface with RF link |
| dataset-stm32-microcontroller-22e02ab9 | 31 | `22e02ab9e857` STM32 sheet, IO + decoupling |
| dataset-rp2040-mocon2040-0ea574f3 | 33 | `0ea574f3f803` RP2040 'Mocon2040' dev board |
| dataset-power-over-135a2a11 | 32 | `135a2a11a338` Power-over-RS485 board |
| dataset-three-phase-0cdac5a0 | 40 | `0cdac5a02e3a` Three-phase gate-driver stage |
| dataset-ecg-sensor-07aabb42 | 45 | `07aabb42a235` ECG analog front end |
| prompt-sallen-key-gain | ≥12 | Sallen-Key 1 kHz + gain-of-10, 9V, virtual ground |
| prompt-555-blinker-ldo | ≥12 | NE555 1 Hz blinker + 5V LDO from 12V |
| prompt-bjt-preamp | ≥12 | 2N3904 common-emitter preamp |
| prompt-hbridge | ≥14 | Discrete 12V H-bridge, 3.3V logic drive |
| prompt-blue-pill | ≥24 | STM32F103 Blue Pill board |
| prompt-arduino-uno | ≥28 | ATmega328P Arduino-style board with CH340C |

## Scoreboard

| case                                   | parts | netlist | erc e/w | critic            | human look | agent s |
| -------------------------------------- | ----- | ------- | ------- | ----------------- | ---------- | ------- |
| dataset-ddr-memory-411be040            | 28    | yes     | 0/0     | 6 [6.0, 6.0, 7.0] | 3          | 60s     |
| dataset-ecg-sensor-07aabb42            | 49    | yes     | 0/0     | 4 [1.0, 4.0, 5.0] | 2          | 257s    |
| dataset-ibm-m122-261071e7              | 34    | NO      | 0/0     | 6 [5.0, 6.0, 6.0] | 4          | 373s    |
| dataset-light-accessory-266db471       | 36    | NO      | 0/1     | 5 [4.0, 5.0, 5.0] | 3          | 232s    |
| dataset-power-over-135a2a11            | 32    | NO      | 6/9     | 5 [4.0, 5.0, 5.0] | 3          | 146s    |
| dataset-rp2040-mocon2040-0ea574f3      | 33    | NO      | 0/2     | 5 [5.0, 5.0, 6.0] | 3          | 496s    |
| dataset-stm32-microcontroller-22e02ab9 | 38    | NO      | 0/3     | 5 [4.0, 5.0, 5.0] | 3          | 534s    |
| dataset-three-phase-0cdac5a0           | 40    | yes     | 4/0     | 6                 | 6          | 82s     |
| prompt-555-blinker-ldo                 | 15    | -       | 0/0     | 6                 | 4          | 52s     |
| prompt-arduino-uno                     | 36    | -       | 0/2     | 5 [4.0, 5.0, 5.0] | 4          | 827s    |
| prompt-bjt-preamp                      | 13    | -       | 0/0     | 8 [7.0, 8.0, 8.0] | 4          | 40s     |
| prompt-blue-pill                       | 35    | -       | 0/0     | 6 [5.0, 6.0, 6.0] | 4          | 826s    |
| prompt-hbridge                         | 25    | -       | 0/0     | 6 [6.0, 6.0, 8.0] | 5          | 117s    |
| prompt-sallen-key-gain                 | 19    | -       | 0/0     | 6 [6.0, 6.0, 7.0] | 5          | 168s    |

Rubrics passed: **1/14** (`prompt-bjt-preamp`). Critic median 6, never above 8.
Part-count floors: met everywhere. ERC errors: 2 cases (`power-over` 6,
`three-phase` 4). Nothing timed out; the whole suite ran in about an hour.

## Netlist fidelity (dataset cases)

| case | ref nets | nets missing | nets extra | parts missing | parts added |
| ---- | -------- | ------------ | ---------- | ------------- | ----------- |
| ddr-memory | 92 | 0 | 0 | 0 | 0 |
| ecg-sensor | 35 | 0 | 0 | 0 | 0 |
| three-phase | 50 | 0 | 0 | 0 | 0 |
| rp2040-mocon2040 | 54 | 2 | 3 | 0 | 0 |
| power-over | 29 | 4 | 4 | 0 | 0 |
| ibm-m122 | 68 | 5 | 6 | 0 | 10 |
| stm32-microcontroller | 52 | 6 | 8 | 0 | 7 |
| light-accessory | 24 | 9 | 10 | 0 | 15 |

3/8 reproduce the human netlist exactly. No part is ever dropped; the failures
are a handful of mis-wired nets, and on three cases parts the prompt never asked
for (15 invented diodes and resistors on `light-accessory`) — "Do not add or
remove parts" is not being honoured.

## The three defects the critic names most often

1. **Sprawl / spacing (15 defects, 14 of 14 cases, 14 of them major).** Every
   single sheet is marked down for it: functional groups flung across a mostly
   empty canvas, decoupling banks stretched edge to edge while their IC sits
   alone, long net runs that follow from the spread. This one defect is what
   keeps the whole suite at 5-6.
2. **Text over circuitry (5 defects, 5 cases).** Repeated net labels merging
   into each other above a capacitor bank, and the agent's own explanatory notes
   and dashed callout boxes drawn across symbols and wires.
3. **No signal-flow grouping — congestion and long unstructured runs (6
   defects across `congestion`, `wire-crossing` and `other`, 6 cases).** Related
   circuitry (flash/QSPI, oscillator, power entry) is not placed as blocks, so
   unrelated nets take long parallel perimeter paths around the MCU.

The human-look judge says the same thing in its own words: "reduce the extreme
empty canvas", "consolidate the widely scattered symbols into compact functional
blocks", "move completion notes and callout boxes away from circuitry".

## Notes

Two harness defects were found and fixed during the run, and the affected cases
were re-measured on their delivered files (agent output untouched):
ImageMagick's SVG renderer refuses dense sheets (`vector graphics nested too
deeply`) — dense renders now fall back to KiCad PDF + poppler, which recovered
the critic scores for `rp2040-mocon2040`, `arduino-uno` and `blue-pill`; and a
human-look judge answering `null` for its lists threw instead of scoring.
The netlist comparison was also tightened mid-run to keep single-pin nets (one
sheet had matched on five nets); re-measuring every case under the strict rule
changed no verdict.

Run artifacts: `quality/runs/schematic-baseline/` (copied to the session
scratchpad `schqc-runs/`).
