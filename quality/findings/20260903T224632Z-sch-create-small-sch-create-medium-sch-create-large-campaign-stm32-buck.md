# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260903T224632Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [tool-contract]

- `sch-create-small`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.status_led_driver.row[1].col[0].value`: unknown field `value`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `sch-create-medium`: turn 1 tool `search_symbols` refusal: {"error":"`queries` must contain 1 to 10 searches"}
- `sch-create-medium`: turn 1 tool `read_schematic` refusal: {"error":"no schematic at /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/sch-create-medium/project/design.kicad_sch yet — create one before editing it"}
- `sch-create-medium`: turn 1 tool `get_net` refusal: {"error":"no net `VREF_TEST`","nets":["+3V3","CANH","CANL","GND","MCU_RX","MCU_TX","Net-(D2-A)","Net-(F1-Pad2)","Net-(SW1-A)","VDD_FILT","VIN_RAW"]}
- `sch-create-medium`: turn 1 tool `add_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.can_interface_fix` places `J2`, which is not a part of that region — region `can_interface_fix` holds R1, SW2, D3"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"renamed":{"D1":"D3","SW1":"SW2"},"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-medium`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.can_interface_fix`: unknown field `can_interface_fix`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `sch-create-medium`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.power_entry_support`: a layout node is exactly one of `part`, `row` or `col`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.buck_regulator`: a layout node is exactly one of `part`, `row` or `col`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `intent.GND`: unknown field `GND`, expected one of `flow`, `rails`, `ports`"}
- `sch-create-large`: turn 1 tool `connect` refusal: {"error":"refused: the edit would change connectivity the call did not name (SWDIO); nothing was written","net_delta":{"merged":[[["SWCLK","SWDIO"],"SWCLK"]],"now_connected":["U2.37"]}}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `intent.GND`: unknown field `GND`, expected one of `flow`, `rails`, `ports`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `parts[0]`: missing field `part`"}
- `sch-create-large`: turn 1 tool `remove_symbols` refusal: {"error":"not on the sheet: #PWR_SW_OUT","missing":["#PWR_SW_OUT"]}
- `campaign-stm32-buck`: turn 1 tool `place_parts` error: no completion recorded
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"PA0","on_sheet":false,"pin":"PA0","pins_on_net":1,"ref":"U2"},{"net":"PA1","on_sheet":false,"pin":"PA1","pins_on_net":1,"ref":"U2"},{"net":"USB_DM_MCU","on_sheet":false,"pin":"PA11","pins_on_net":1,"ref":"U2"},{"net":"USB_DP_MCU","on_sheet":false,"pin":"PA12","pins_on_net":1,"ref":"U2"},{"net":"SWDIO","on_sheet":false,"pin":"PA13","pins_on_net":1,"ref":"U2"},{"net":"SWCLK","on_sheet":false,"pin":"PA14","pins_on_net":1,"ref":"U2"},{"net":"PA2","on_sheet":false,"pin":"PA2","pins_on_net":1,"ref":"U2"},{"net":"PA3","on_sheet":false,"pin":"PA3","pins_on_net":1,"ref":"U2"},{"net":"PA4","on_sheet":false,"pin":"PA4","pins_on_net":1,"ref":"U2"},{"net":"PA5","on_sheet":false,"pin":"PA5","pins_on_net":1,"ref":"U2"},{"net":"PA6","on_sheet":false,"pin":"PA6","pins_on_net":1,"ref":"U2"},{"net":"PA7","on_sheet":false,"pin":"PA7","pins_on_net":1,"ref":"U2"},{"net":"PB12","on_sheet":false,"pin":"PB12","pins_on_net":1,"ref":"U2"},{"net":"PB13","on_sheet":false,"pin":"PB13","pins_on_net":1,"ref":"U2"},{"net":"PB14","on_sheet":false,"pin":"PB14","pins_on_net":1,"ref":"U2"},{"net":"PB15","on_sheet":false,"pin":"PB15","pins_on_net":1,"ref":"U2"},{"net":"SWO","on_sheet":false,"pin":"PB3","pins_on_net":1,"ref":"U2"},{"net":"I2C_SCL","on_sheet":false,"pin":"PB8","pins_on_net":1,"ref":"U2"},{"net":"I2C_SDA","on_sheet":false,"pin":"PB9","pins_on_net":1,"ref":"U2"},{"net":"PC13","on_sheet":false,"pin":"PC13","pins_on_net":1,"ref":"U2"},{"net":"HSE_IN","on_sheet":false,"pin":"PH0-OSC_IN","pins_on_net":1,"ref":"U2"},{"net":"HSE_OUT","on_sheet":false,"pin":"PH1-OSC_OUT","pins_on_net":1,"ref":"U2"}],"decouple_unresolved":[{"how":"add the decoupling capacitors explicitly with their supply and ground nets","ref":"C6","why":"no power_in pins; fallback looked for VDD*/VCC* and VSS*/GND* pin names; needs supply and ground candidates (found [] / []) — write the caps explicitly"}],"did_you_mean":{"SWCLK":"SW","SWDIO":"SW","SWO":"SW"},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `C17`, which is not a part of that region (did you mean `C16`?)"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}

## [prompt]

- `sch-create-small`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-small`: cost: 110.8s elapsed, 42.6s agent, 7 provider requests
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 4 times in a row
- `sch-create-large`: turn 1 loop smell: tool `get_net` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-large`: cost: 355.1s elapsed, 272.8s agent, 53 provider requests

## [engine]

- `sch-create-small`: failed check: human_look_schematic_score >= 8 — actual 5
- `sch-create-small`: judge: Tighten the oversized driver block and reduce the excessive whitespace between functional sections.
- `sch-create-small`: judge: Move R2 next to the transistor/base node; its label-based connection is electrically valid but visually disconnected from the driver.
- `sch-create-small`: judge: Reposition J1, J2, Q1, and power-symbol labels to provide consistent clearance from pins and wires.
- `sch-create-small`: judge: Align the input, base resistor, transistor, LED, and supply components on a cleaner left-to-right grid.
- `sch-create-small`: judge: Reduce the decorative border and explanatory-note area to make the schematic more compact and readable.
- `sch-create-medium`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "D2", "with": "Net-(D2-A)"}, {"field": "Reference", "ref": "D3", "with": "CANL"}, {"field": "Reference", "ref": "SW2", "with": "TERM_H"}]
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — actual 5
- `sch-create-medium`: judge: Re-draft the power-entry block to eliminate overlapping labels, wires, and component references.
- `sch-create-medium`: judge: Move the CAN bus TVS protection directly beside J2 and use short, clearly routed CANH/CANL connections.
- `sch-create-medium`: judge: Fix the three reported text collisions: D2 value, D3 reference/CANL label, and SW2 reference/TERM_H label.
- `sch-create-medium`: judge: Remove wires routed through capacitor bodies and replace the long perimeter ground wiring with shorter local connections.
- `sch-create-medium`: judge: Resolve the remaining unconnected wire endpoint warning and rerender the schematic.
- `sch-create-medium`: judge: Align the regulator, connector, protection, and termination components to consistent grid rows and spacing.
- `sch-create-medium`: schematic critic: minor/text-overlap/far-right ground-rail termination beside C4: Two GND text instances are rendered immediately adjacent at the far-right ground connection and visually read as a merged or duplicated label.
- `sch-create-large`: failed check: erc_errors == 0 — actual 1
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 7
- `sch-create-large`: judge: Repair the unresolved ERC error: #FLG1 is still associated with both PROTECTED_VIN and SW_OUT; remove the stale flag/net association and rerun ERC.
- `sch-create-large`: judge: Complete the 8 MHz crystal loop: U2.5 (OSC_IN) is unconnected, while Y1.1/C8.1 form an isolated net; connect OSC_IN to the crystal and its load capacitor.
- `sch-create-large`: judge: Add a dedicated decoupling capacitor for the fifth STM32 supply pin; U2 has five +3V3 supply pins but only C4–C7 are provided as MCU decouplers.
- `sch-create-large`: judge: Improve schematic readability by tightening the excessive horizontal spacing, enlarging small annotations, and rerouting wires that pass through MCU and LED symbol bodies.
- `campaign-stm32-buck`: failed check: agent_exit == 0 — actual -6
- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: part_count >= 50 — actual 11
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — actual 6
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — actual 4
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: Only a partial buck power block was created; the STM32F405, USB-C interface, USB protection, crystal, reset/boot circuitry, SWD header, LEDs, GPIO headers, and I2C pull-ups are missing.
- `campaign-stm32-buck`: judge: The delivered schematic contains only 11 parts and 7 nets, far below the required complete controller design.
- `campaign-stm32-buck`: judge: U1 VIN is isolated on net VIN rather than connected to the +5V rail, so the buck converter cannot receive the input supply.
- `campaign-stm32-buck`: judge: The required 100 nF VIN bypass capacitor was not implemented; the tool explicitly reported this completeness gap.
- `campaign-stm32-buck`: judge: No PCB was created, so placement, routing, ground pour, edge accessibility, track widths, and DRC cannot be assessed.
- `campaign-stm32-buck`: judge: No DRC was run and no Gerbers, drill files, pick-and-place file, or BOM were exported.
- `campaign-stm32-buck`: judge: The schematic power block is visually fragmented with long disconnected-looking paths, inconsistent alignment, and weak functional grouping; recompose it into compact readable power-input, buck, feedback, and output sections.
- `campaign-stm32-buck`: schematic critic: major/spacing/U1, C5, L1, C3/C4, and the feedback network: The switching power stage is unnecessarily spread across the block, with the bootstrap capacitor distant from U1, the inductor separated from the regulator, and the output capacitor bank distributed well away from the switch-node transition.
- `campaign-stm32-buck`: schematic critic: minor/off-spine-leg/R1 EN pull-up: R1 is placed well below the +5 V rail and reaches EN through a long down-and-around U-shaped route instead of sitting adjacent to the EN pin.

## [harness]

- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors

## [judge]

- `sch-create-small`: schematic human-look: Tighten the oversized status-driver box and bring the isolated R2 section closer to the transistor/base area.
- `sch-create-small`: schematic human-look: Reposition overlapping or ambiguously placed labels around J1, J2, Q1, and the power symbols; maintain consistent clearance from wires and pins.
- `sch-create-small`: schematic human-look: Align the bypass block, driver block, titles, and explanatory notes to a clearer page grid with more balanced whitespace.
- `sch-create-medium`: schematic human-look: Untangle the crowded upper-left power-entry area; reference designators, net labels, and wires visibly overlap.
- `sch-create-medium`: schematic human-look: Align related components to a consistent grid and equalize spacing, especially the regulator, connector, and protection rows.
- `sch-create-medium`: schematic human-look: Reduce long perimeter wiring and oversized empty regions; use shorter local connections and clearer subsection spacing.
- `sch-create-large`: schematic human-look: Compress the excessive horizontal whitespace and bring related functional blocks closer together.
- `sch-create-large`: schematic human-look: Increase symbol and annotation scale; much of the text and component detail is too small at the overall-sheet view.
- `sch-create-large`: schematic human-look: Align block contents and shorten local wire runs to create more consistent, deliberate routing.
- `campaign-stm32-buck`: schematic human-look: The buck-converter power path is visually fragmented, with the inductor and several associated elements separated far from U1.
- `campaign-stm32-buck`: schematic human-look: Component placement and wiring lack a consistent grid: long jogs, uneven spacing, and isolated-looking resistor/capacitor symbols make the flow hard to follow.
- `campaign-stm32-buck`: schematic human-look: Text and net labels are inconsistently placed and sometimes crowd symbols or wires, especially around U1, the feedback network, and the lower annotations.

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: The first place_parts call was refused because nested layout entries did not accept a value field, but the schema guidance was not available beforehand.
- `sch-create-small`: struggled: turn 1: The successful place_parts result initially reported nets=0, then later realised six nets without exposing the generated connectivity or wiring details.
- `sch-create-small`: struggled: turn 1: render_schematic reported zero visual findings but did not provide structured information about component positions, labels, or visible connections.
- `sch-create-small`: struggled: turn 1: check_schematic/ERC verified rule compliance but did not confirm that the intended low-side LED topology and resistor values were electrically correct.
- `sch-create-small`: wished: turn 1: Expose the complete place_parts schema, including how component values should be assigned, before accepting a layout request.
- `sch-create-small`: wished: turn 1: Provide a connectivity/netlist preview after placement, including pin-to-net mappings and generated wires.
- `sch-create-small`: wished: turn 1: Return an image thumbnail or machine-readable visual inspection summary from render_schematic.
- `sch-create-small`: wished: turn 1: Add an intent-level electrical validation that checks requested topology, component roles, and safe LED current before completion.
- `sch-create-medium`: struggled: turn 1: The initial place_parts call was refused because the layout schema rejected a mixed row/column structure, but the error did not clearly explain the valid nesting pattern.
- `sch-create-medium`: struggled: turn 1: check_schematic reported a pre-existing unconnected_wire_endpoint without identifying the wire, endpoint component, or a safe repair.
- `sch-create-medium`: struggled: turn 1: render_schematic reported seven visual findings without describing their locations or severity, making visual-quality review difficult.
- `sch-create-medium`: struggled: turn 1: check_schematic completeness advisories initially suggested missing power-entry protection even though the existing design already contained related power circuitry.
- `sch-create-medium`: wished: turn 1: A place_parts schema example or validation message showing accepted row/column nesting would make block placement faster.
- `sch-create-medium`: wished: turn 1: A schematic diagnostic API should return the exact wire segment, coordinates, and owning sheet for unconnected endpoints.
- `sch-create-medium`: wished: turn 1: Render findings should include readable descriptions, coordinates, and an annotated preview or severity classification.
- `sch-create-medium`: wished: turn 1: A component and net inventory report should expose the non-power-part count and confirm required interface features directly.
- `sch-create-medium`: wished: turn 1: An ERC/completeness mode should distinguish actionable design omissions from pre-existing or unrelated warnings.
- `sch-create-medium`: wished: turn 1: A final verification report should include the rendered image, part count, key net connectivity, and remaining findings in one structured result.
- `sch-create-large`: struggled: turn 1: The net-inspection and repair workflow was insufficient for diagnosing why SW_OUT and PROTECTED_VIN merged around U1.
- `sch-create-large`: struggled: turn 1: The schematic checker reported a pin-to-pin conflict but provided no safe repair, leaving the erroneous power-flag association difficult to remove.
- `sch-create-large`: struggled: turn 1: The remove_symbols refusal was confusing because the reported #PWR_SW_OUT reference was not present even though the associated power connection remained.
- `sch-create-large`: struggled: turn 1: The completeness checker incorrectly flagged missing power-entry support despite the USB-C protection circuitry being present.
- `sch-create-large`: struggled: turn 1: The renderer reported wires passing through symbol bodies without providing targeted geometry or cleanup guidance.
- `sch-create-large`: struggled: turn 1: The per-turn time limit was reached before the ERC error could be repaired and the requested schematic could be fully validated.
- `sch-create-large`: wished: turn 1: Provide get_net output with exact connected wire segments, labels, and symbol pins to make unintended net merges diagnosable.
- `sch-create-large`: wished: turn 1: Add a targeted disconnect or split-net tool that can remove one erroneous connection without rebuilding nearby circuitry.
- `sch-create-large`: wished: turn 1: Allow removal of orphaned or internally generated power symbols by connection, net, or coordinates when their references are unavailable.
- `sch-create-large`: wished: turn 1: Improve completeness checks to recognize USB-C receptacle-based power entry and existing fuse, reverse-polarity, TVS, and capacitor protection.
- `sch-create-large`: wished: turn 1: Have render_schematic identify the specific wires and symbols causing body crossings and offer automatic layout cleanup.
- `sch-create-large`: wished: turn 1: Increase the time budget or provide a fast batch repair-and-check operation for large schematics.
- `campaign-stm32-buck`: struggled: turn 1: place_parts refused the MCU block because the layout referenced nonexistent C17, while also reporting many dangling MCU pins without clearly separating fatal from nonfatal issues.
- `campaign-stm32-buck`: struggled: turn 1: The decoupling audit could not infer capacitor supply and ground nets because the capacitor symbols had no power pins, despite the intended connections being explicit in the block description.
- `campaign-stm32-buck`: struggled: turn 1: The tool reported SWCLK, SWDIO, and SWO as did-you-mean mismatches to SW, making the required debug-net naming and pin mapping unclear.
- `campaign-stm32-buck`: struggled: turn 1: The second MCU placement triggered a schematic-typeset stack overflow and aborted the run, with no recoverable intermediate result.
- `campaign-stm32-buck`: struggled: turn 1: The workflow advanced to BoardSeed after an incomplete power block and did not provide a structured completeness gate for all requested schematic blocks, routing, ERC, DRC, and fabrication exports.
- `campaign-stm32-buck`: struggled: turn 1: No board-routing, ERC/DRC, render, or fabrication-export results were produced, so it was impossible to verify the requested zero-error deliverables.
- `campaign-stm32-buck`: wished: turn 1: Validate all part references and layout entries before committing place_parts, with an automatic correction or precise field-level error.
- `campaign-stm32-buck`: wished: turn 1: Allow explicit pin-to-net connectivity for passive components and use that data directly for decoupling and power audits.
- `campaign-stm32-buck`: wished: turn 1: Provide a symbol-pin manifest showing exact pin names, aliases, and accepted net labels before placement.
- `campaign-stm32-buck`: wished: turn 1: Prevent stack overflow on large schematic typesetting and return a recoverable error with the generated project preserved.
- `campaign-stm32-buck`: wished: turn 1: Add a task checklist and completion gate that tracks required components, connections, routing, ERC/DRC, renders, and export files.
- `campaign-stm32-buck`: wished: turn 1: Provide integrated board placement/routing and one-command ERC, DRC, visualization, and Gerber/BOM/position export with summarized artifact paths.

## [variance]

- `sch-create-small`: provider latency: #1=6400ms, #2=3900ms, #3=9100ms, #4=5500ms, #5=4700ms, #6=2200ms, #7=4400ms
- `sch-create-medium`: cost: 262.2s elapsed, 197.4s agent, 37 provider requests
- `sch-create-medium`: provider latency: #1=6200ms, #2=2800ms, #3=5400ms, #4=5600ms, #5=7500ms, #6=2900ms, #7=14700ms, #8=6100ms, #9=2200ms, #10=1800ms, #11=2300ms, #12=5300ms, #13=2500ms, #14=2200ms, #15=2800ms, #16=7300ms, #17=8000ms, #18=4300ms, #19=4200ms, #20=3700ms, #21=2600ms, #22=2700ms, #23=2000ms, #24=2100ms, #25=2800ms, #26=3400ms, #27=2500ms, #28=2200ms, #29=3300ms, #30=4300ms, #31=2700ms, #32=2700ms, #33=10100ms, #34=5300ms, #35=2400ms, #36=3700ms, #37=4400ms
- `sch-create-large`: provider latency: #1=5200ms, #2=2700ms, #3=3500ms, #4=5100ms, #5=9400ms, #6=2100ms, #7=5700ms, #8=8100ms, #9=6700ms, #10=3000ms, #11=2000ms, #12=3300ms, #13=2700ms, #14=8600ms, #15=6200ms, #16=2600ms, #17=5200ms, #18=4400ms, #19=5600ms, #20=5100ms, #21=4200ms, #22=2400ms, #23=3300ms, #24=4400ms, #25=3200ms, #26=2300ms, #27=4000ms, #28=2700ms, #29=2400ms, #30=3500ms, #31=5100ms, #32=3200ms, #33=4100ms, #34=3000ms, #35=3500ms, #36=3300ms, #37=6600ms, #38=4900ms, #39=3500ms, #40=3900ms, #41=2600ms, #42=2900ms, #43=2200ms, #44=4200ms, #45=2500ms, #46=3200ms, #47=4300ms, #48=3200ms, #49=3700ms, #50=3000ms, #51=11000ms, #52=3200ms, #53=3200ms
- `campaign-stm32-buck`: cost: 117.0s elapsed, 53.0s agent, 7 provider requests
- `campaign-stm32-buck`: provider latency: #1=4700ms, #2=2000ms, #3=10700ms, #4=1400ms, #5=5800ms, #6=11900ms, #7=12600ms
