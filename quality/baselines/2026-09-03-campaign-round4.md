# Campaign round 4 (2026-09-03, incremental workflow, --max-turns 3, quality-first rubric)

| case                       | score | sch critic | pcb critic | human look | checks | erc e/w | moved | lost | added | pcb moved | turns | agent s | elapsed s |
| -------------------------- | ----- | ---------- | ---------- | ---------- | ------ | ------- | ----- | ---- | ----- | --------- | ----- | ------- | --------- |
| campaign-audio-preamp      | -     | 6          | 5          | 5/3        | 8/14   | 0/0     | -     | -    | -     | -         | 3     | 857s    | 1094s     |
| campaign-bms-10s           | -     | 4          | 5          | 2/3        | 7/14   | 0/7     | -     | -    | -     | -         | 3     | 865s    | 1048s     |
| campaign-esp32-sensor-node | -     | -          | -          | -/-        | 5/14   | 1/3     | -     | -    | -     | -         | 3     | 840s    | 896s      |
| campaign-stm32-buck        | -     | -          | 2          | -/3        | 8/14   | 0/1     | -     | -    | -     | -         | 3     | 902s    | 1038s     |

Per case: audio 44 parts ERC 0 → board DRC 0, 5 unrouted, sch critic 6 / human 5, pcb critic 5 / human 3. BMS 50 parts ERC 0/7 → board DRC 0, 20 unrouted. STM32 68 parts ERC 0/1 → board DRC 0, 20 unrouted, pcb critic 2. ESP32 50 parts ERC 1 → no board (sync refused on ERC errors).

Blocking class moved to the board phase: route_board all-or-nothing on the plane net GND; route_track congestion; move_parts courtyard refusals on model-picked coordinates and invented schema (gap/reference); sync_board intent_after_creation ×3 cases; sync_board refusing on ERC errors and on missing footprints; place_board naming connectors absent from the board. Schematic tail: derived Net-(…) names used as targets (refused), connect 'every connection failed', remove_symbols not declaring its nets, library-NC pin wired refused.
