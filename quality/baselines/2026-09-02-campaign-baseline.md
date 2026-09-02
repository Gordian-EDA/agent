# Campaign suite baseline — 2026-09-02

One real-model pass with `python3 quality/run.py --output quality/runs/campaign0 --suite campaign`. All four agents exhausted the 32-provider-request safety limit while still working on the schematic, so none created a PCB or fabrication bundle. The displayed elapsed time includes judging; the `elapsed_seconds <= 300` check is evaluated immediately before judging.

| case | score | checks | ERC e/w | DRC e/w | parts | elapsed | requests |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| campaign-audio-preamp | 1 | 5/11 | 9/2 | not measured | 39 | 229.1s | 32 |
| campaign-bms-10s | 2 | 5/11 | 2/13 | not measured | 61 | 353.0s | 32 |
| campaign-esp32-sensor-node | 2 | 5/11 | 4/8 | not measured | 57 | 576.4s | 32 |
| campaign-stm32-buck | 2 | 5/11 | 3/11 | not measured | 50 | 373.6s | 32 |

## Top judge issues

### campaign-audio-preamp

1. No PCB was created, leaving placement, routing, edge access, ground pour, and signal-separation requirements unmet.
2. No Gerbers, drill, position, or BOM files were exported.
3. ERC reported 9 errors and 2 warnings.

### campaign-bms-10s

1. No PCB was created, leaving high-current placement/routing, Kelvin sensing, ground pour, and connector-edge requirements unmet.
2. No Gerbers, drill, position, or BOM files were exported.
3. ERC reported 2 errors and 13 warnings.

### campaign-esp32-sensor-node

1. No PCB was created, leaving edge access, antenna keepout, routing, ground pour, and DRC requirements unmet.
2. No Gerbers, drill, position, or BOM files were exported.
3. ERC reported 4 errors and 8 warnings.

### campaign-stm32-buck

1. No PCB was created, leaving placement, routing, ground pour, edge access, switch-node containment, and decoupler proximity unverified.
2. No Gerbers, drill, position, or BOM files were exported.
3. ERC reported 3 errors and 11 warnings.

## Fact coverage

The rubrics machine-check agent exit, schematic and PCB creation, ERC and DRC errors, KiCad/extractor partition agreement, part-count floors, schematic unconnected pins, board unconnected items, fabrication-file count, and elapsed time. The harness does not expose an `unrouted` fact or list, so zero unrouted connections is prose-only. Provider request count is also not a result fact; the table above extracts the final `provider requests` value from each agent log.
