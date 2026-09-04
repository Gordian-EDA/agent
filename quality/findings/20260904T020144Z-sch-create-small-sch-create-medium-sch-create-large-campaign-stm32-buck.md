# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260904T020144Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [tool-contract]

- `sch-create-small`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.status_led_driver`: unknown field `status_led_driver`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `sch-create-small`: turn 1 tool `move_symbols` refusal: {"error":"refused: dragging J1, R3, D1, Q1, J2, R1, R2, C1 was refused (drag would leave 2 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `sch-create-large`: turn 1 tool `search_symbols` refusal: {"error":"`queries` must contain 1 to 10 searches"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"bench_mismatch","error":"the bench draw did not preserve connectivity; nothing was written","ok":false,"report":{"benched":[{"ref":"C10","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"C11","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"C12","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"C13","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"C9","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"J2","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"R5","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"R6","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"R7","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"R8","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"R9","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"U2","why":"could not be drawn truthfully (shorted PB14+PB6)"},{"ref":"Y1","why":"could not be drawn truthfully (shorted PB14+PB6)"}],"committed":false,"dangling":[{"net":"PA2","on_sheet":false,"pin":"10","pins_on_net":1,"ref":"U2"},{"net":"PA3","on_sheet":false,"pin":"11","pins_on_net":1,"ref":"U2"},{"net":"PA4","on_sheet":false,"pin":"12","pins_on_net":1,"ref":"U2"},{"net":"PA5","on_sheet":false,"pin":"13","pins_on_net":1,"ref":"U2"},{"net":"PA6","on_sheet":false,"pin":"14","pins_on_net":1,"ref":"U2"},{"net":"PA7","on_sheet":false,"pin":"15","pins_on_net":1,"ref":"U2"},{"net":"PB0","on_sheet":false,"pin":"16","pins_on_net":1,"ref":"U2"},{"net":"PB10","on_sheet":false,"pin":"18","pins_on_net":1,"ref":"U2"},{"net":"PB11","on_sheet":false,"pin":"19","pins_on_net":1,"ref":"U2"},{"net":"PC13","on_sheet":false,"pin":"2","pins_on_net":1,"ref":"U2"},{"net":"VSS_1","on_sheet":false,"pin":"20","pins_on_net":1,"ref":"U2"},{"net":"PB12","on_sheet":false,"pin":"21","pins_on_net":1,"ref":"U2"},{"net":"PB13","on_sheet":false,"pin":"22","pins_on_net":1,"ref":"U2"},{"net":"PB14","on_sheet":false,"pin":"23","pins_on_net":1,"ref":"U2"},{"net":"PB15","on_sheet":false,"pin":"24","pins_on_net":1,"ref":"U2"},{"net":"PA8","on_sheet":false,"pin":"25","pins_on_net":1,"ref":"U2"},{"net":"PA9","on_sheet":false,"pin":"26","pins_on_net":1,"ref":"U2"},{"net":"PA10","on_sheet":false,"pin":"27","pins_on_net":1,"ref":"U2"},{"net":"PA11","on_sheet":false,"pin":"28","pins_on_net":1,"ref":"U2"},{"net":"PA12","on_sheet":false,"pin":"29","pins_on_net":1,"ref":"U2"},{"net":"PC14-OSC32_IN","on_sheet":false,"pin":"3","pins_on_net":1,"ref":"U2"},{"net":"PA13-JTMS-SWDIO","on_sheet":false,"pin":"30","pins_on_net":1,"ref":"U2"},{"net":"PA14-JTCK-SWCLK","on_sheet":false,"pin":"31","pins_on_net":1,"ref":"U2"},{"net":"PB3-JTDO","on_sheet":false,"pin":"32","pins_on_net":1,"ref":"U2"},{"net":"PB4-NJTRST","on_sheet":false,"pin":"33","pins_on_net":1,"ref":"U2"},{"net":"PB5","on_sheet":false,"pin":"34","pins_on_net":1,"ref":"U2"},{"net":"PB6","on_sheet":false,"pin":"35","pins_on_net":1,"ref":"U2"},{"net":"PB7","on_sheet":false,"pin":"36","pins_on_net":1,"ref":"U2"},{"net":"PB8","on_sheet":false,"pin":"38","pins_on_net":1,"ref":"U2"},{"net":"PB9","on_sheet":false,"pin":"39","pins_on_net":1,"ref":"U2"},{"net":"PC15-OSC32_OUT","on_sheet":false,"pin":"4","pins_on_net":1,"ref":"U2"},{"net":"PA1","on_sheet":false,"pin":"9","pins_on_net":1,"ref":"U2"}],"did_you_mean":{"PB4-NJTRST":"BST","PC14-OSC32_IN":"CC2","PC15-OSC32_OUT":"CC2"},"mismatch":{"disturbed":[],"scattered":[],"shorted":[["PB14","PB6"]]},"nets":["+3V3","BOOT0","GND","LED_POWER","LED_STATUS","NRST","OSC_IN","OSC_OUT","PA0-WKUP","PA1","PA10","PA11","PA12","PA13-JTMS-SWDIO","PA14-JTCK-SWCLK","PA2","PA3","PA4","PA5","PA6","PA7","PA8","PA9","PB0","PB1","PB10","PB11","PB12","PB13","PB14","PB15","PB3-JTDO","PB4-NJTRST","PB5","PB6","PB7","PB8","PB9","PC13","PC14-OSC32_IN","PC15-OSC32_OUT","SWCLK","SWDIO","SWO","VBAT","VDDA","VDD_1","VSSA","VSS_1"]}}
- `sch-create-large`: turn 1 tool `no_connect` refusal: {"error":"D4.1 is connected to `LED_STATUS` with R13.2; a no-connect marker would sever a real net. Disconnect it first if that is what you meant."}
- `sch-create-large`: turn 1 tool `swap_symbol` refusal: {"error":"refused: the replacement pins could not be re-seated cleanly: drag would leave 1 loose ends behind; nothing was written","suggestion":{"new_symbol_unassigned_pins":[],"old_pins_without_counterpart":[],"pin_map":{"1":"2","2":"1"}}}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"PC13","on_sheet":false,"pin":"2","pins_on_net":1,"ref":"U2"},{"net":"PH0","on_sheet":false,"pin":"5","pins_on_net":1,"ref":"U2"},{"net":"PH1","on_sheet":false,"pin":"6","pins_on_net":1,"ref":"U2"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `C11`, which is not a part of that region (did you mean `C19`?)","layout-unknown-part: `layout.mcu_core` places `C12`, which is not a part of that region (did you mean `C19`?)","layout-unknown-part: `layout.mcu_core` places `C13`, which is not a part of that region (did you mean `C19`?)"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `parts[14]`: missing field `part`"}
- `campaign-stm32-buck`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.support_indicators`: unknown field `support_indicators`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}

## [prompt]

- `sch-create-small`: turn 1 loop smell: tool `get_symbol` called 8 times in a row
- `sch-create-small`: cost: 198.5s elapsed, 149.8s agent, 25 provider requests
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `connect` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `add_power` called 6 times in a row
- `sch-create-large`: cost: 295.9s elapsed, 260.5s agent, 52 provider requests
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: cost: 316.4s elapsed, 280.7s agent, 47 provider requests

## [engine]

- `sch-create-small`: failed check: text_collisions == [] — actual [{"field": "Reference", "ref": "D1", "with": "N$1"}, {"field": "Value", "ref": "D1", "with": "N$1"}]
- `sch-create-small`: failed check: schematic_critic_score >= 8 — actual 5
- `sch-create-small`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-small`: judge: Remove the oversized empty dashed annotation box and replace it with compact, nearby notes.
- `sch-create-small`: judge: Reorganize the scattered blocks into a compact left-to-right flow: J2/R1/R2 → Q1, with J1/R3/D1 above and C1 near the supply input.
- `sch-create-small`: judge: Move D1 reference/value text and the N$1 net label apart to eliminate the documented text collisions.
- `sch-create-small`: judge: Separate Q1, LED, and associated net labels to provide deliberate clearance and improve readability.
- `sch-create-small`: schematic critic: major/spacing/whole sheet; especially J1/C1 versus Q1 and the large dashed annotation region: The functional components are scattered across a small portion of a very large canvas, with C1 far from J1 and an oversized mostly empty annotation box dominating the drawing instead of a compact driver layout.
- `sch-create-small`: schematic critic: minor/text-overlap/Q1 and the Net-(Q1-B) label: The `Net-(Q1-B)` text intrudes into the Q1 symbol and competes with the transistor artwork and pin markings.
- `sch-create-medium`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-medium`: judge: Reflow the schematic into a compact left-to-right signal path; the current blocks are widely scattered with excessive empty space.
- `sch-create-medium`: judge: Reposition and orient CANH/CANL labels, C4, D3, and the protection note to improve readability and avoid visual crowding.
- `sch-create-medium`: judge: Place the CAN TVS devices immediately beside J2 and use an explicitly identified bidirectional CAN TVS part rather than relying on generic D_TVS symbols.
- `sch-create-medium`: judge: Move the termination network adjacent to the bus connector and show its 120 Ω switchable connection directly across CANH and CANL.
- `sch-create-medium`: judge: Add clear section boundaries and consistent alignment for the MCU interface, transceiver, bus protection, termination, decoupling, and indicator circuits.
- `sch-create-medium`: schematic critic: major/spacing/C3, R2, and R7 relative to U1 and the CAN interface: Related transceiver support parts are scattered across large empty areas, with R7 particularly far from the CAN connector and termination circuitry.
- `sch-create-medium`: schematic critic: major/orientation/R4 RX series path at the left MCU header: R4 is drawn vertically in the horizontal MCU signal interface, while the corresponding TX series resistor R3 is horizontal.
- `sch-create-medium`: schematic critic: minor/text-overlap/C4 / CANL_BUS lower-center region: The C4 value/net annotation is crowded against the capacitor symbol and the vertical CANL_BUS labeling, reducing legibility.
- `sch-create-large`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "D4", "with": "LED_STATUS"}, {"field": "Value", "ref": "R12", "with": "+3V3"}, {"field": "Value", "ref": "R9", "with": "I2C_SDA"}, {"field": "Value", "ref": "U3", "with": "+3V3"}]
- `sch-create-large`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `sch-create-large`: judge: Fix the four reported text collisions: D4/LED_STATUS, R12/+3V3, R9/I2C_SDA, and U3/+3V3.
- `sch-create-large`: judge: Reroute I2C_SCL, I2C_SDA, LED_POWER, and LED_STATUS wires so they do not pass through MCU or resistor bodies; the current drawing is difficult to audit visually.
- `sch-create-large`: judge: Review the buck EN bias: R12 connects VIN_PROT to GND while U1.EN is already on VIN_PROT, creating an unexplained permanent pulldown/load; verify the value and intended enable behavior.
- `sch-create-large`: judge: The final schematic render has nine visual findings and the clean artifact conversion failed; produce a clean, readable final render before release.
- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: erc_errors == 0 — actual 1
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: PCB was not created; no placement, routing, ground pour, DRC, or board render exists.
- `campaign-stm32-buck`: judge: Fabrication export was not performed; the fab directory contains no Gerbers, drill files, position file, or BOM.
- `campaign-stm32-buck`: judge: The delivered schematic still has 1 ERC error and 7 warnings, so it is not ERC-clean.
- `campaign-stm32-buck`: judge: The GPIO headers are not actually broken out: the netlist shows J5–J8 signal pins as unconnected rather than connected to at least 20 labeled MCU GPIOs.
- `campaign-stm32-buck`: judge: The schematic has unresolved visual/layout defects, including PH0 wiring through the crystal body and 14 off-grid pins on the added support circuitry.
- `campaign-stm32-buck`: judge: Required support circuitry remains incomplete, including VDDA bulk capacitance, buck bootstrap/control support, and the indicated supply protection items.

## [harness]

- `sch-create-large`: schematic critic unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/sch-create-large/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/sch-create-large/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.

- `sch-create-large`: schematic human-look unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/sch-create-large/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/sch-create-large/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.

- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors
- `campaign-stm32-buck`: schematic critic unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.

- `campaign-stm32-buck`: schematic human-look unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.


## [judge]

- `sch-create-small`: schematic human-look: Remove the oversized empty dashed annotation box and place the explanatory notes in compact, nearby text.
- `sch-create-small`: schematic human-look: Reorganize the scattered sections into a clear left-to-right signal flow with aligned component rows and consistent spacing.
- `sch-create-small`: schematic human-look: Untangle the crowded Q1/LED area by separating reference text, value text, labels, and wires with deliberate clearance.
- `sch-create-medium`: schematic human-look: Reflow the scattered subcircuits into a compact, aligned left-to-right signal path with clearly grouped interface, protection, termination, and indicator sections.
- `sch-create-medium`: schematic human-look: Eliminate label and annotation collisions, especially around CANH_CONN, CANL_CONN, C4, D3, and the protection note.
- `sch-create-medium`: schematic human-look: Standardize component spacing, wire lengths, orientations, and section boundaries so the page has a consistent drafting grid and visual hierarchy.

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: move_symbols refused the multi-symbol drag because of two loose ends without identifying the affected pins or wires.
- `sch-create-small`: struggled: turn 1: arrange repeatedly introduced label and symbol overlaps even though connectivity and ERC remained clean.
- `sch-create-small`: struggled: turn 1: render_schematic reported visual findings but did not provide their locations or descriptions in the result.
- `sch-create-small`: struggled: turn 1: get_symbol returned no visible pin, field, or geometry details, making targeted inspection difficult.
- `sch-create-small`: struggled: turn 1: read_schematic with compact detail did not expose enough layout information to diagnose the visual overlaps efficiently.
- `sch-create-small`: wished: turn 1: Add a dry-run or diagnostic mode for move_symbols that identifies the exact loose ends causing refusal.
- `sch-create-small`: wished: turn 1: Make arrange preserve text clearance and report the specific objects it moved or overlapped.
- `sch-create-small`: wished: turn 1: Include structured visual-finding details and bounding boxes in render_schematic results.
- `sch-create-small`: wished: turn 1: Expose complete symbol pin positions, fields, and bounding boxes through get_symbol.
- `sch-create-small`: wished: turn 1: Provide dedicated tools to move labels and fields independently from symbols and wires.
- `sch-create-medium`: struggled: turn 1: search_symbols returned hit counts but not symbol pin mappings or recommended part variants.
- `sch-create-medium`: struggled: turn 1: place_parts reported nets=0 initially, making its automatic connectivity behavior unclear.
- `sch-create-medium`: struggled: turn 1: The schematic tools provided no concise human-readable netlist for verifying every required connection.
- `sch-create-medium`: struggled: turn 1: render_schematic reported zero visual findings but did not expose the rendered image contents for semantic review.
- `sch-create-medium`: struggled: turn 1: check_schematic initially found a missing power-entry protection gap only after rendering.
- `sch-create-medium`: struggled: turn 1: ERC reported zero issues without confirming that the termination, TVS, and connector networks were electrically correct.
- `sch-create-medium`: wished: turn 1: Add a symbol-inspection tool showing exact pins, pin types, and orientations before placement.
- `sch-create-medium`: wished: turn 1: Provide an explicit connectivity/netlist preview after place_parts and connect operations.
- `sch-create-medium`: wished: turn 1: Allow connections to be specified by named nets and pin names rather than opaque pin references such as R3.2.
- `sch-create-medium`: wished: turn 1: Add schematic assertions for required topologies, such as 120 ohms between CANH and CANL through a switch.
- `sch-create-medium`: wished: turn 1: Provide image inspection or annotated render output so component placement and labels can be reviewed directly.
- `sch-create-medium`: wished: turn 1: Run completeness checks before rendering and report all missing required interface features in one pass.
- `sch-create-large`: struggled: turn 1: check_schematic returned contradictory summaries, reporting checks.errors=0 while also refusing with nine ERC pin_not_connected errors.
- `sch-create-large`: struggled: turn 1: Power-symbol connectivity diagnostics were confusing because duplicate GND and +3V3 symbols were flagged despite representing valid global nets.
- `sch-create-large`: struggled: turn 1: Removing nine power symbols cleared ERC but risked hiding or weakening explicit power intent rather than fixing the underlying connectivity issue.
- `sch-create-large`: struggled: turn 1: render_schematic reported nine visual findings but did not identify their locations or provide actionable fixes.
- `sch-create-large`: struggled: turn 1: The large-sheet layout produced wire-through-body and text-collision findings that could not be efficiently diagnosed or corrected with the available tools.
- `sch-create-large`: wished: turn 1: Provide consistent ERC result fields and distinguish true electrical errors from expected global-power-symbol connectivity cases.
- `sch-create-large`: wished: turn 1: Add a tool to merge, re-anchor, or validate duplicate global power symbols without deleting them.
- `sch-create-large`: wished: turn 1: Return detailed render findings with component references, coordinates, categories, and suggested move or reroute operations.
- `sch-create-large`: wished: turn 1: Provide schematic layout tools for automatic collision avoidance, wire rerouting, and symbol spacing optimization.
- `sch-create-large`: wished: turn 1: Add a compact connectivity/net report showing every required functional block and its connected pins before final rendering.
- `sch-create-large`: wished: turn 1: Support batch edits and one final validate-render cycle to reduce the many iterative tool calls needed for a large schematic.
- `campaign-stm32-buck`: struggled: turn 1: The connect tool reported success but only labelled both endpoints and did not create a clear wire path, leaving the ERC pin_not_connected error unresolved.
- `campaign-stm32-buck`: struggled: turn 1: The check_schematic tool mixed blocking ERC errors with heuristic completeness advisories, making it unclear which warnings were actually required for acceptance.
- `campaign-stm32-buck`: struggled: turn 1: The completeness checker recommended unrelated VDDA power-entry circuitry and a 3V3 TVS despite the requested USB-powered design, distracting from the specified requirements.
- `campaign-stm32-buck`: struggled: turn 1: The add_parts tool placed seven components on the bench without automatically arranging or verifying their exact connectivity and footprint assignments.
- `campaign-stm32-buck`: struggled: turn 1: There was no efficient batch operation for fixing, arranging, rendering, and rechecking the schematic, so progress consumed many tool calls.
- `campaign-stm32-buck`: struggled: turn 1: The per-turn time limit was reached before the board phase, preventing PCB placement, routing, DRC, and fabrication export.
- `campaign-stm32-buck`: wished: turn 1: Provide a targeted ERC-fix tool that connects the exact reported pins with a real wire or explicit junction rather than only applying labels.
- `campaign-stm32-buck`: wished: turn 1: Separate mandatory user-requirement validation from optional heuristic completeness advisories.
- `campaign-stm32-buck`: wished: turn 1: Allow schematic inspection to return component coordinates, pin endpoints, wires, and net connectivity in a compact machine-readable form.
- `campaign-stm32-buck`: wished: turn 1: Add a batch schematic operation that places, wires, annotates, and arranges a complete support circuit atomically.
- `campaign-stm32-buck`: wished: turn 1: Provide board creation with automatic constraint-aware placement and initial routing for standard power, USB, crystal, and SWD topologies.
- `campaign-stm32-buck`: wished: turn 1: Expose a single end-to-end verify-and-export workflow that reports remaining unrouted nets, DRC/ERC issues, and missing fabrication outputs.

## [variance]

- `sch-create-small`: provider latency: #1=10400ms, #2=3100ms, #3=3300ms, #4=8500ms, #5=2700ms, #6=3400ms, #7=3900ms, #8=3100ms, #9=7600ms, #10=3600ms, #11=2700ms, #12=9000ms, #13=2100ms, #14=1500ms, #15=2900ms, #16=11000ms, #17=3100ms, #18=2500ms, #19=2700ms, #20=11100ms, #21=9500ms, #22=2500ms, #23=6400ms, #24=2100ms, #25=8900ms
- `sch-create-medium`: cost: 140.3s elapsed, 81.7s agent, 10 provider requests
- `sch-create-medium`: provider latency: #1=6700ms, #2=4000ms, #3=22500ms, #4=10100ms, #5=6700ms, #6=2200ms, #7=4200ms, #8=7400ms, #9=1900ms, #10=8000ms
- `sch-create-large`: provider latency: #1=6100ms, #2=3700ms, #3=2900ms, #4=3800ms, #5=7700ms, #6=2800ms, #7=4500ms, #8=2900ms, #9=7100ms, #10=8600ms, #11=9300ms, #12=3500ms, #13=6300ms, #14=4900ms, #15=6800ms, #16=5400ms, #17=3600ms, #18=5000ms, #19=4200ms, #20=2600ms, #21=3800ms, #22=5000ms, #23=3300ms, #24=2300ms, #25=9700ms, #26=4400ms, #27=3500ms, #28=2300ms, #29=2700ms, #30=2400ms, #31=2100ms, #32=4000ms, #33=2700ms, #34=2300ms, #35=3600ms, #36=2200ms, #37=2900ms, #38=2300ms, #39=2200ms, #40=2300ms, #41=4400ms, #42=2600ms, #43=4500ms, #44=11400ms, #45=2000ms, #46=3000ms, #47=4600ms, #48=3400ms, #49=2400ms, #50=4700ms, #51=2600ms, #52=5800ms
- `campaign-stm32-buck`: provider latency: #1=8500ms, #2=2800ms, #3=9100ms, #4=2400ms, #5=3700ms, #6=19000ms, #7=13200ms, #8=10800ms, #9=5400ms, #10=7900ms, #11=10300ms, #12=6300ms, #13=2500ms, #14=5100ms, #15=5000ms, #16=2900ms, #17=3700ms, #18=2700ms, #19=2100ms, #20=2600ms, #21=2500ms, #22=2200ms, #23=2000ms, #24=9500ms, #25=3400ms, #26=3100ms, #27=4300ms, #28=3000ms, #29=2800ms, #30=3300ms, #31=2600ms, #32=2400ms, #33=3200ms, #34=6000ms, #35=5700ms, #36=2400ms, #37=2900ms, #38=3400ms, #39=2300ms, #40=2400ms, #41=4500ms, #42=2600ms, #43=14400ms, #44=2600ms, #45=2400ms, #46=3000ms, #47=9600ms
