# Campaign round 6 (2026-09-03, --max-turns 5, after pcb-sync-shorts + sch-refusals-3)

| case                       | score | sch critic | pcb critic | human look | checks | erc e/w | moved | lost | added | pcb moved | turns | agent s | elapsed s |
| -------------------------- | ----- | ---------- | ---------- | ---------- | ------ | ------- | ----- | ---- | ----- | --------- | ----- | ------- | --------- |
| campaign-audio-preamp      | 1     | 6          | 1          | 4/2        | 8/14   | 0/0     | -     | -    | -     | -         | 5     | 1479s   | 1760s     |
| campaign-bms-10s           | -     | 5.5        | 1          | 3/2        | 8/14   | 0/1     | -     | -    | -     | -         | 5     | 1439s   | 1695s     |
| campaign-esp32-sensor-node | -     | -          | 2          | -/2        | 7/14   | 0/3     | -     | -    | -     | -         | 5     | 1377s   | 1558s     |
| campaign-stm32-buck        | -     | -          | 1          | -/3        | 6/14   | 0/2     | -     | -    | -     | -         | 5     | 1449s   | 1667s     |

Regression: sync_board 'would short N net pairs' ×41 now at board CREATION (guard judges the seeded board: staging row / intent overlaps) and a sync↔route deadlock ('no net named X; run sync_board first'). Boards 44–111 unconnected, PCB critic 1–2. place_parts panicked once ('no entry found for key'). Schematics: ERC 0 on 4/4, sch critic 5.5–6, human-look 3–4.
