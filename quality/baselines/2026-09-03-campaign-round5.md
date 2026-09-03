# Campaign round 5 (2026-09-03, after PCB-2/3, sch-refusals-2, parity; --max-turns 3)

| case                       | score | sch critic | pcb critic | human look | checks | erc e/w | moved | lost | added | pcb moved | turns | agent s | elapsed s |
| -------------------------- | ----- | ---------- | ---------- | ---------- | ------ | ------- | ----- | ---- | ----- | --------- | ----- | ------- | --------- |
| campaign-audio-preamp      | 2     | 5          | 5          | 3/3        | 8/14   | 0/0     | -     | -    | -     | -         | 3     | 836s    | 1026s     |
| campaign-bms-10s           | 1     | 4          | 5          | 2/3        | 7/14   | 0/8     | -     | -    | -     | -         | 3     | 863s    | 1037s     |
| campaign-esp32-sensor-node | 1     | -          | 2          | -/2        | 7/14   | 0/1     | -     | -    | -     | -         | 3     | 829s    | 946s      |
| campaign-stm32-buck        | 1     | -          | 5          | -/2        | 7/14   | 0/1     | -     | -    | -     | -         | 3     | 870s    | 997s      |

All four: ERC 0 schematics (48/60/59/55 parts), netlist parity with KiCAD true, a board created. Audio: 0 unconnected, DRC 14, no fab. BMS/ESP32/STM32: 118–134 unconnected, DRC 5–18. Dominant refusal: sync_board 'would short N net pairs the schematic keeps apart' ×19 (existing copper vs changed nets) — must retract and report, not refuse.
