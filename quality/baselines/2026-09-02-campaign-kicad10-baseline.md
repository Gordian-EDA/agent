# Campaign baseline 2026-09-02 (KiCAD 10 CLI-only, main after the kicad10 + refusal-ergo3 merges)

Clean toolchain rerun (camp4). ERC from KiCAD 10. No case reached the board phase in 270 s.

| case                       | score | sch critic | pcb critic | checks | erc e/w | moved | lost | added | pcb moved | elapsed |
| -------------------------- | ----- | ---------- | ---------- | ------ | ------- | ----- | ---- | ----- | --------- | ------- |
| campaign-audio-preamp      | 2     | 5          | -          | 7/11   | 0/80    | -     | -    | -     | -         | 267s    |
| campaign-bms-10s           | 2     | 4          | -          | 4/11   | 8/114   | -     | -    | -     | -         | 366s    |
| campaign-esp32-sensor-node | 1     | 5          | -          | 4/11   | 1/126   | -     | -    | -     | -         | 354s    |
| campaign-stm32-buck        | 1     | 4          | -          | 4/11   | 0/117   | -     | -    | -     | -         | 342s    |

Time split: LLM latency 172–256 s of 270 s; place_parts compute 13–55 s. place_parts refusals 36/38 (pin keys via alternate names, unknown footprints, schema strictness, engine shorts on series parts).
