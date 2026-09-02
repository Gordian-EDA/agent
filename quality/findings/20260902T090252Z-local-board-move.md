# Quality findings: local-board-move

Generated: 20260902T090252Z
Run output: /home/mimi/agent/.claude/worktrees/algos/quality/runs/algos2

Questions:
- does the board look designed now

## [tool-contract]

- `local-board-move`: tool `move_parts` error: error: moves[0]: missing required string `reference`
- `local-board-move`: tool `move_parts` refusal: error: move_parts refused: R1 at [2.000, 4.000] would leave -0.600 mm to R2 — their courtyards need 0.200 mm between them

## [prompt]

- `local-board-move`: loop smell: tool `move_parts` called 3 times in a row
- `local-board-move`: cost: 147.2s elapsed, 63.6s agent, 10 provider requests

## [harness]

- `local-board-move`: self-diagnosis unavailable: judge returned no verdict object: '{"struggles":["move_parts initially failed with an opaque missing-reference error instead of showing the expected argument schema.","move_parts refused the first R1 position with a courtyard-clearance message that did not identify the conflicting geometry or suggest a valid position.","route_board reported routing three traces without identifying which traces were changed, making the affected-copper scope difficult to verify.","get_board did not provide a concise before-and-after diff for positions, outline, values, and connectivity."],"wishes":["Add move_parts dry-run or clearance diagnostics showing conflicting courtyards and nearest legal placements.","Have route_board accept an explicit set of nets or traces and report every modified and preserved item.","Provide a board-diff tool that verifies only the requested footprint and copper changes occurred.","Make render_board return a comparison or annotated image highlighting moved parts and rerouted copper."]}'

## [judge]

- `local-board-move`: schematic critic: minor/spacing/R1-R2 divider center: R1 and R2 are separated by a relatively long vertical wire and conspicuous empty gap despite being a simple two-resistor divider.
