# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260904T002110Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [tool-contract]

- `sch-create-small`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.status_led_driver` places `#PWR01`, which is not a part of that region — region `status_led_driver` holds C1","layout-unknown-part: `layout.status_led_driver` places `#PWR02`, which is not a part of that region — region `status_led_driver` holds C1"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-small`: turn 1 tool `move_symbols` refusal: {"error":"refused: dragging J1, R3, D1, Q1, J2, R1, R2, C1 was refused (drag would leave 2 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `sch-create-medium`: turn 1 tool `search_footprints` refusal: {"error":"unknown symbol `Interface_CAN_LIN:MCP2562`"}
- `sch-create-medium`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"TERM_A","on_sheet":false,"pin":"1","pins_on_net":1,"ref":"R2"},{"net":"TERM_B","on_sheet":false,"pin":"2","pins_on_net":1,"ref":"R2"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.can_interface` places `J1`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R4`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `D3`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `U1`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `C1`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `C2`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `C3`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R3`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R6`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `SW1`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R1`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `J2`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `D1`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `D2`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R7`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R8`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `R5`, which is not a part of that region — region `can_interface` holds R2","layout-unknown-part: `layout.can_interface` places `D4`, which is not a part of that region — region `can_interface` holds R2"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `blocks.buck_regulator.note2`: unknown field `note2`, expected `title` or `note`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.buck_regulator`: a layout node is exactly one of `part`, `row` or `col`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["pin-conflict: U2: physical pin 45 claimed by both `I2C1_SCL` and `PB8`","pin-conflict: U2: physical pin 46 claimed by both `I2C1_SDA` and `PB9`"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `parts[4]`: missing field `part`"}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"bench_mismatch","error":"the bench draw did not preserve connectivity; nothing was written","ok":false,"report":{"benched":[{"ref":"C_X1","why":"could not be drawn truthfully (shorted GND+PH1; disturbed existing GND, PH1)"},{"ref":"C_X2","why":"could not be drawn truthfully (shorted GND+PH1; disturbed existing GND, PH1)"},{"ref":"J2","why":"could not be drawn truthfully (shorted GND+PH1; disturbed existing GND, PH1)"},{"ref":"X1","why":"could not be drawn truthfully (shorted GND+PH1; disturbed existing GND, PH1)"}],"committed":false,"mismatch":{"disturbed":["GND","PH1"],"scattered":[],"shorted":[["GND","PH1"]]},"nets":["3V3","GND","NRST","PA13_SWDIO","PA14_SWCLK","PB3_SWO","PH0","PH1"]}}
- `campaign-stm32-buck`: turn 1 tool `label` refusal: {"error":"U2.13 is already on net `VDDA`; a label does not replace that name, it merges `VDDA` and `VDDA_FILT` into one net. Use delete_wires to take U2.13 off `VDDA` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.13"]},"tool":"delete_wires"}}
- `campaign-stm32-buck`: turn 1 tool `no_connect` refusal: {"error":"U2.13 is connected to `VDDA` with #FLG_VDDA.1; a no-connect marker would sever a real net. Disconnect it first if that is what you meant."}
- `campaign-stm32-buck`: turn 1 tool `add_symbols` refusal: {"error":"`LED_PWR` is not a valid reference; use letters followed by digits, such as R12, U3, or #PWR01","placed":[]}

## [prompt]

- `sch-create-small`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-small`: turn 1 loop smell: tool `get_symbol` called 8 times in a row
- `sch-create-small`: turn 1 loop smell: tool `get_net` called 3 times in a row
- `sch-create-small`: cost: 154.7s elapsed, 104.8s agent, 20 provider requests
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 4 times in a row
- `sch-create-large`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `place_parts` called 6 times in a row
- `sch-create-large`: cost: 268.5s elapsed, 145.7s agent, 26 provider requests
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: cost: 301.7s elapsed, 273.6s agent, 42 provider requests

## [engine]

- `sch-create-small`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-small`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-small`: judge: Remove or resize the oversized empty dashed hierarchical-sheet box; it dominates the page and makes the schematic look unfinished.
- `sch-create-small`: judge: Recompose the circuit into a compact, aligned left-to-right signal flow with J1, R3, D1, Q1, and the CTRL/base network grouped together.
- `sch-create-small`: judge: Move C1 next to the power connector and switching stage instead of isolating it far to the right.
- `sch-create-small`: judge: Group the power and control connectors with their associated circuitry and use consistent spacing to improve readability.
- `sch-create-small`: schematic critic: major/spacing/Whole sheet, especially the large blue annotation box and the separated J1/+5 V/C1 region: The circuit occupies only a small upper-left portion of the canvas while a large empty blue rectangle dominates the sheet, and the power connector and decoupling section are farther from the driver than necessary.
- `sch-create-medium`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — actual 5
- `sch-create-medium`: judge: Move D1 and D2 adjacent to J2 so the CANH/CANL TVS protection is physically located at the bus connector.
- `sch-create-medium`: judge: Reduce the oversized sheet and consolidate the widely separated vertical component chains into compact functional sections.
- `sch-create-medium`: judge: Align J1, U1, J2, termination, and protection circuitry for a clear left-to-right signal flow.
- `sch-create-medium`: judge: Correct the termination annotation to identify the actual implemented network as SW1 in series with R1; the claimed R1/R2 pair is inaccurate because R2 was removed.
- `sch-create-medium`: schematic critic: major/spacing/J2, D1/D2, R7/R8, SW1/R1, and U1 CAN-bus region: The CAN connector, TVS/bias bank, and switchable termination network are distributed around large empty regions, making the bus protection and termination architecture read as disconnected functional blocks.
- `sch-create-large`: failed check: part_count >= 35 — actual 34
- `sch-create-large`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 6
- `sch-create-large`: judge: Correct the TPS62160 pin wiring: VIN must connect to VBUS_PROT; the delivered netlist places U1.1 (VIN) on GND while U1.2/U1.3 are on VBUS_PROT.
- `sch-create-large`: judge: Reverse D1 orientation so the Schottky diode conducts from VBUS_USB to VBUS_PROT while blocking reverse current.
- `sch-create-large`: judge: Reverse or rewire both indicator LEDs; their net connections place the LED junctions reverse-biased, so the MCU outputs cannot drive them as indicators.
- `sch-create-large`: judge: Provide a dedicated local decoupling capacitor for every STM32 supply rail/pin; the netlist has five MCU supply pins but only four shared +3V3 capacitors, with C4 serving as buck output bulk.
- `sch-create-large`: judge: Compact and rebalance the sheet: reduce the extreme horizontal whitespace, enlarge component annotations, and place MCU support circuitry closer to the MCU.
- `sch-create-large`: schematic critic: major/spacing/Whole sheet, especially the separated MCU support, power, USB-C, buck, and interface regions: The functional blocks occupy a very wide, sparse canvas, leaving large empty areas and placing related MCU support circuitry far from the MCU and power chain.
- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: erc_errors == 0 — actual 1
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: PCB was not created or routed; no DRC was run and no board render exists.
- `campaign-stm32-buck`: judge: No fabrication outputs were exported: Gerbers, drill, pick-and-place, BOM, and fab directory are absent.
- `campaign-stm32-buck`: judge: ERC is not clean: one unresolved 3V3 power-symbol pin-not-connected error remains.
- `campaign-stm32-buck`: judge: VDDA is incorrectly isolated: U2.13 is on VDDA alone while the ferrite bead and VDDA decoupling capacitors are on disconnected VDDA_FILT.
- `campaign-stm32-buck`: judge: Required VDDA decoupling is incomplete; the required local 100 nF and 4.7 uF support capacitors are reported missing.
- `campaign-stm32-buck`: judge: TPS54302 BOOT and EN control pull support is missing despite the requested EN pull-up.
- `campaign-stm32-buck`: judge: The required red power LED is absent from the final netlist; only the activity LED circuitry remains.
- `campaign-stm32-buck`: judge: Schematic readability still has a value/text collision and wires running through U2 and the crystal footprint.
- `campaign-stm32-buck`: judge: The attempted crystal/debug placement exposed a GND-to-PH1 connectivity short and was refused; verify the delivered crystal and SWD topology before proceeding.

## [harness]

- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors
- `campaign-stm32-buck`: schematic critic unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.

- `campaign-stm32-buck`: schematic human-look unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.


## [judge]

- `sch-create-small`: schematic human-look: The large empty dashed hierarchical-sheet box dominates the page without containing any circuitry.
- `sch-create-small`: schematic human-look: The actual circuit is compressed into a small upper-left strip, leaving excessive unused canvas and poor visual balance.
- `sch-create-small`: schematic human-look: Power, connector, and net-label elements feel scattered rather than organized into clear functional groups with consistent spacing.
- `sch-create-medium`: schematic human-look: Reduce the oversized canvas and excessive empty space, especially around the left protection block and lower vertical chains.
- `sch-create-medium`: schematic human-look: Rearrange related components into compact, consistently aligned functional clusters instead of widely separated vertical runs.
- `sch-create-medium`: schematic human-look: Standardize symbol spacing, label placement, and connector orientation to create clearer visual hierarchy and faster reading.
- `sch-create-large`: schematic human-look: Tighten the extreme horizontal spacing and reduce unused canvas between functional sections.
- `sch-create-large`: schematic human-look: Increase annotation and reference-text size; much of the sheet is difficult to read at normal viewing scale.
- `sch-create-large`: schematic human-look: Rebalance the composition by aligning the major blocks and integrating the oversized MCU area with its surrounding support circuitry.

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: The get_symbol tool returned no symbol details, making pin orientations and exact placements difficult to inspect.
- `sch-create-small`: struggled: turn 1: The move_symbols refusal cited two loose ends without identifying which pins or wires caused the refusal.
- `sch-create-small`: struggled: turn 1: The arrange tool sometimes reported parts=7 despite eight requested references, with no explanation of the discrepancy.
- `sch-create-small`: struggled: turn 1: The arrange tool initially created a Q1/BASE label overlap and provided only a warning rather than an automatic fix.
- `sch-create-small`: struggled: turn 1: The render_schematic tool reported zero findings but did not provide an inline image or readable visual summary for review.
- `sch-create-small`: struggled: turn 1: The net and connectivity results were terse, so verifying the complete LED path and connector pin mapping required inference.
- `sch-create-small`: wished: turn 1: Return full symbol metadata from get_symbol, including pin names, numbers, orientations, and coordinates.
- `sch-create-small`: wished: turn 1: Explain move_symbols refusals by identifying the specific loose ends and suggesting valid target coordinates.
- `sch-create-small`: wished: turn 1: Have arrange preserve or clearly report all requested parts and explain omitted or duplicated references.
- `sch-create-small`: wished: turn 1: Add a schematic preview or visual-diff tool that exposes label overlaps, spacing, and readability issues directly.
- `sch-create-small`: wished: turn 1: Provide a structured connectivity report listing every component pin and its resolved net.
- `sch-create-small`: wished: turn 1: Allow targeted layout edits that move labels or symbols without requiring preservation of every connected wire endpoint.
- `sch-create-medium`: struggled: turn 1: place_parts rejected the full layout because the can_interface region appeared to contain only R2, producing repeated layout-unknown-part errors for valid parts.
- `sch-create-medium`: struggled: turn 1: The place_parts refusal did not clearly explain how to declare or populate regions correctly, requiring trial and error.
- `sch-create-medium`: struggled: turn 1: read_schematic with compact detail did not provide enough context to diagnose the region-membership problem quickly.
- `sch-create-medium`: struggled: turn 1: check_schematic reported a power-entry TVS completeness warning even though the requested protection was specifically at the CAN bus connector.
- `sch-create-medium`: struggled: turn 1: The final render was not automatically rechecked after adding D5, so visual and electrical verification required separate tool calls.
- `sch-create-medium`: wished: turn 1: Provide explicit region membership requirements and an example valid layout payload for place_parts.
- `sch-create-medium`: wished: turn 1: Return a concise structured diagnosis identifying the malformed field that caused the layout rejection.
- `sch-create-medium`: wished: turn 1: Offer a schematic summary showing parts, regions, and net connectivity in one readable result.
- `sch-create-medium`: wished: turn 1: Distinguish required user-request checks from optional completeness heuristics such as power-entry protection.
- `sch-create-medium`: wished: turn 1: Have place_parts automatically connect simple two-pin support parts to the named rails when the block specifies them.
- `sch-create-medium`: wished: turn 1: Have render_schematic and check_schematic run together after the final schematic modification.
- `sch-create-large`: struggled: turn 1: place_parts refused the first interfaces payload with a missing `part` field but did not identify which row entry was malformed.
- `sch-create-large`: struggled: turn 1: The completeness checker continued reporting the U2 bypass gap even after adding bypass capacitors, making the required reference unclear.
- `sch-create-large`: struggled: turn 1: render_schematic initially reported one visual finding without describing the problem or its location.
- `sch-create-large`: struggled: turn 1: search_footprints returned only a count and generic hits, not the actual footprint names or suitability details.
- `sch-create-large`: struggled: turn 1: Placement results reported net counts but did not expose the resulting pin-to-net connectivity for review.
- `sch-create-large`: wished: turn 1: Provide schema validation errors with the exact JSON path and offending value.
- `sch-create-large`: wished: turn 1: Expose completeness rules and explain why a component does or does not satisfy each gap.
- `sch-create-large`: wished: turn 1: Return detailed visual findings with coordinates, severity, and suggested fixes.
- `sch-create-large`: wished: turn 1: List full footprint search results with names, libraries, pin compatibility, and previews.
- `sch-create-large`: wished: turn 1: Add a connectivity report showing every component pin and its resolved net.
- `sch-create-large`: wished: turn 1: Allow inspecting or annotating the rendered schematic directly before final verification.
- `campaign-stm32-buck`: struggled: turn 1: The connect tool only labelled the two 3V3 endpoints and reported unchanged connectivity, so it could not resolve the remaining ERC pin-not-connected error.
- `campaign-stm32-buck`: struggled: turn 1: check_schematic returned checks.errors=0 and warnings=0 while simultaneously reporting one ERC error and six completeness warnings, making pass/fail status confusing.
- `campaign-stm32-buck`: struggled: turn 1: The completeness checker identified missing VDDA capacitors, buck control pulls, and protection circuitry but provided null fixes and no targeted mutators.
- `campaign-stm32-buck`: struggled: turn 1: The workflow spent nearly the entire turn repeatedly rendering and checking the schematic, leaving no time or capability to advance to PCB placement and routing.
- `campaign-stm32-buck`: struggled: turn 1: No board was created, so routing, ground pours, accessibility, DRC, renders, and fabrication exports could not be performed or verified.
- `campaign-stm32-buck`: struggled: turn 1: The tool results did not provide a concise component-to-pin/net audit to verify all STM32 supply pins, USB duplicate pins, and intentionally unused GPIOs before layout.
- `campaign-stm32-buck`: wished: turn 1: Provide a schematic batch-edit tool that can add, place, wire, and label a complete support circuit in one operation.
- `campaign-stm32-buck`: wished: turn 1: Make connect distinguish label association from actual wire connectivity and offer an explicit power-symbol or wire repair operation.
- `campaign-stm32-buck`: wished: turn 1: Return a unified check result with separate ERC, completeness, and hard-blocking status instead of contradictory error counts.
- `campaign-stm32-buck`: wished: turn 1: Give every completeness finding a concrete automated fix or a precise component/net edit recipe.
- `campaign-stm32-buck`: wished: turn 1: Support an end-to-end transition from a validated schematic to automatic footprint assignment, board placement, routing, and verification.
- `campaign-stm32-buck`: wished: turn 1: Add a netlist and pin-coverage report showing every required MCU pin, decoupler, connector duplicate, no-connect, and unrouted net.

## [variance]

- `sch-create-small`: provider latency: #1=5400ms, #2=2500ms, #3=7900ms, #4=6800ms, #5=3500ms, #6=2700ms, #7=2100ms, #8=4000ms, #9=4600ms, #10=8700ms, #11=2100ms, #12=2900ms, #13=4700ms, #14=4700ms, #15=4600ms, #16=2400ms, #17=6500ms, #18=2800ms, #19=7400ms, #20=4900ms
- `sch-create-medium`: cost: 144.9s elapsed, 85.8s agent, 17 provider requests
- `sch-create-medium`: provider latency: #1=7700ms, #2=3700ms, #3=3800ms, #4=3200ms, #5=18200ms, #6=4900ms, #7=2800ms, #8=1600ms, #9=2800ms, #10=1600ms, #11=4400ms, #12=5700ms, #13=2400ms, #14=2500ms, #15=4400ms, #16=2900ms, #17=3400ms
- `sch-create-large`: provider latency: #1=7600ms, #2=4100ms, #3=4400ms, #4=6200ms, #5=1600ms, #6=5600ms, #7=4300ms, #8=7100ms, #9=5800ms, #10=6700ms, #11=2900ms, #12=3200ms, #13=2600ms, #14=3300ms, #15=11200ms, #16=6500ms, #17=5000ms, #18=4900ms, #19=5100ms, #20=3700ms, #21=2200ms, #22=3500ms, #23=4100ms, #24=4100ms, #25=3000ms, #26=4500ms
- `campaign-stm32-buck`: provider latency: #1=5000ms, #2=3000ms, #3=12100ms, #4=1400ms, #5=5500ms, #6=21900ms, #7=9000ms, #8=5800ms, #9=5100ms, #10=9100ms, #11=6700ms, #12=10500ms, #13=10300ms, #14=3600ms, #15=5000ms, #16=5500ms, #17=4600ms, #18=10000ms, #19=11700ms, #20=8200ms, #21=2600ms, #22=2700ms, #23=2900ms, #24=2900ms, #25=3500ms, #26=2300ms, #27=6100ms, #28=3400ms, #29=3100ms, #30=2400ms, #31=2000ms, #32=4000ms, #33=3900ms, #34=3200ms, #35=2500ms, #36=3200ms, #37=4800ms, #38=2800ms, #39=3900ms, #40=3200ms, #41=2800ms, #42=3500ms
