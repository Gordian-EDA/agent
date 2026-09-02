# Schematic suite — integrated main @f7fd866 (2026-09-01)

11/12 pass every deterministic check, ERC 0 throughout; `sch-create-medium` failed on a duplicate refdes from a follow-up place_parts (fixed in the payload-audit lane merged after this run). Both slow truthfulness gates green (floorplan_netlist 2/2 in 2054 s, placement_snapshot 2/2 byte-identical in 1542 s). PCB: create-led-driver-pcb 6 (judge/layout), replace-pcb-component seeding failed (fixed after this run: rails/ports exempt from the dangling audit).

| case                  | score | checks | erc e/w | moved | lost | added | elapsed |
| --------------------- | ----- | ------ | ------- | ----- | ---- | ----- | ------- |
| sch-add-block         | 9     | -      | 0/11    | 0     | 0    | 2     | 51s     |
| sch-create-large      | 5     | 7/7    | 0/6     | -     | -    | -     | 419s    |
| sch-create-medium     | 3     | 5/7    | 0/4     | -     | -    | -     | 145s    |
| sch-create-small      | 10    | 7/7    | 0/0     | -     | -    | -     | 39s     |
| sch-edit-existing     | 10    | -      | 0/9     | 0     | 0    | 1     | 23s     |
| sch-extend-led        | 5     | 10/10  | 0/33    | 0     | 0    | 3     | 57s     |
| sch-extend-protection | 8     | 10/10  | 0/38    | 0     | 0    | 5     | 60s     |
| sch-extend-testpoints | 7     | 10/10  | 0/36    | 0     | 0    | 4     | 84s     |
| sch-replace-connector | 7     | 11/11  | 0/31    | 0     | 0    | 1     | 36s     |
| sch-replace-ic        | 10    | 7/7    | 0/37    | 0     | 0    | 0     | 46s     |
| sch-replace-part      | 10    | -      | 0/7     | 0     | 0    | 0     | 14s     |
| sch-replace-passive   | 10    | 9/9    | 0/37    | 0     | 0    | 0     | 17s     |
