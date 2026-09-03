# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260903T221848Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [tool-contract]

- `sch-create-small`: turn 1 tool `search_footprints` refusal: {"error":"discovery call batch budget exhausted","note":"reuse the catalog results already returned by this completion"}
- `sch-create-small`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.led_driver`: a layout node is exactly one of `part`, `row` or `col`"}
- `sch-create-small`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-duplicate-part: `layout.led_driver` places `Q1` twice; every part gets one place","layout-duplicate-part: `layout.led_driver` places `Q1` twice; every part gets one place"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.power_entry_regulator` places `D2`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"did_you_mean":["A1","A2"],"part":"Device:D_TVS","reason":"pin keys `A`, `K` not found on D2 (Device:D_TVS)","ref":"D2"}],"unreliable_nets":[],"warnings":["mapped intent.rails.VBUS_PROTECTED from left to the engine-supported top band","mapped intent.rails.VBUS_RAW from left to the engine-supported top band","mapped intent.rails.VOUT_3V3 from right to the engine-supported bottom band"]}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"VCAP1","on_sheet":false,"pin":"1","pins_on_net":1,"ref":"C8"},{"net":"BOOT0","on_sheet":false,"pin":"1","pins_on_net":1,"ref":"R6"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `C5`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"part":"power:GND","reason":"pin key `GND` not found on #PWR01 (power:GND)","ref":"#PWR01"},{"part":"power:+3V3","reason":"pin key `+3V3` not found on #PWR02 (power:+3V3)","ref":"#PWR02"}],"unreliable_nets":[]}
- `sch-create-large`: turn 1 tool `connect` refusal: {"error":"`@R5.2` names no existing net: R5.2 is not on a named net yet. Label it first, or connect straight to the pin."}
- `sch-create-large`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.interfaces_indicators`: unknown field `interfaces_indicators`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"CC1","on_sheet":false,"pin":"A5","pins_on_net":0,"ref":"J1"},{"net":"USB_DP_RAW","on_sheet":false,"pin":"A6","pins_on_net":0,"ref":"J1"},{"net":"USB_DM_RAW","on_sheet":false,"pin":"A7","pins_on_net":0,"ref":"J1"},{"net":"CC2","on_sheet":false,"pin":"B5","pins_on_net":0,"ref":"J1"},{"net":"USB_DP_RAW","on_sheet":false,"pin":"B6","pins_on_net":0,"ref":"J1"},{"net":"USB_DM_RAW","on_sheet":false,"pin":"B7","pins_on_net":0,"ref":"J1"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.power_entry` places `D1`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"did_you_mean":["SBU1"],"part":"Connector:USB_C_Receptacle_USB2.0_16P","reason":"pin key `S1` not found on J1 (Connector:USB_C_Receptacle_USB2.0_16P)","ref":"J1"},{"did_you_mean":["Diode:SMAJ5.0A","Diode:SMAJ6.0A","Diode:SMAJ7.0A"],"part":"Power_Protection:SMBJ5.0A","reason":"D1: Power_Protection:SMBJ5.0A not found in any library; did you mean Diode:SMAJ5.0A, Diode:SMAJ6.0A, Diode:SMAJ7.0A?","ref":"D1"}],"unreliable_nets":[],"warnings":["mapped intent.rails.3V3 from right to the engine-supported bottom band","mapped intent.rails.VBUS5 from left to the engine-supported top band"]}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"CC1","on_sheet":false,"pin":"A5","pins_on_net":1,"ref":"J1"},{"net":"CC2","on_sheet":false,"pin":"B5","pins_on_net":1,"ref":"J1"},{"net":"USB_SHIELD","on_sheet":false,"pin":"SBU1","pins_on_net":1,"ref":"J1"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["pin-conflict: J1: physical pin A8 claimed by both `A8` and `SBU1`"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[],"warnings":["mapped intent.rails.3V3 from right to the engine-supported bottom band","mapped intent.rails.VBUS5 from left to the engine-supported top band"]}
- `campaign-stm32-buck`: turn 1 tool `search_footprints` refusal: {"error":"unknown symbol `Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm_Vertical`"}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `SW1`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"did_you_mean":["Device:FerriteBead","Device:FerriteBead_Small","Device:L_Ferrite"],"part":"Device:Ferrite_Bead","reason":"FB1: Device:Ferrite_Bead not found in any library; did you mean Device:FerriteBead, Device:FerriteBead_Small, Device:L_Ferrite?","ref":"FB1"},{"did_you_mean":["Switch:SW_DPST","Switch:SW_DPST_Temperature","Switch:SW_DPST_x2"],"part":"Button_Switch_SMD:SW_SPST_TL3342","reason":"SW1: Button_Switch_SMD:SW_SPST_TL3342 not found in any library; did you mean Switch:SW_DPST, Switch:SW_DPST_Temperature, Switch:SW_DPST_x2?","ref":"SW1"}],"unreliable_nets":[]}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"bench_mismatch","error":"the bench draw did not preserve connectivity; nothing was written","ok":false,"report":{"benched":[{"ref":"C10","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C11","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C12","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C13","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C14","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C15","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C16","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C17","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C18","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C6","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C7","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C8","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"C9","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"J2","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"R8","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"R9","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"U2","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"},{"ref":"Y1","why":"no placement engine could draw it truthfully (shorted GND+HSE_OUT)"}],"committed":false,"mismatch":{"disturbed":[],"scattered":[],"shorted":[["GND","HSE_OUT"]]},"nets":["3V3","BOOT0","GND","HSE_IN","HSE_OUT","I2C1_SCL","I2C1_SDA","NRST","SWCLK","SWDIO","SWO","USB_DM","USB_DP","VBAT","VCAP1","VCAP2","VDDA"]}}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.usb_interface` places `J1`, which is not a part of that region","layout-unknown-part: `layout.usb_interface` places `U3`, which is not a part of that region","layout-unknown-part: `layout.usb_interface` places `U2`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[],"warnings":["mapped intent.rails.VBUS5 from left to the engine-supported top band"]}
- `campaign-stm32-buck`: turn 1 tool `swap_symbol` refusal: {"error":"refused: the replacement pins could not be re-seated cleanly: drag would leave 1 loose ends behind; nothing was written","suggestion":{"new_symbol_unassigned_pins":[],"old_pins_without_counterpart":[],"pin_map":{"1":"2","2":"1"}}}

## [prompt]

- `sch-create-small`: turn 1 loop smell: tool `search_footprints` called 5 times in a row
- `sch-create-small`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-small`: cost: 135.1s elapsed, 74.3s agent, 10 provider requests
- `sch-create-large`: turn 1 loop smell: tool `add_power` called 5 times in a row
- `sch-create-large`: turn 1 loop smell: tool `arrange` called 5 times in a row
- `sch-create-large`: turn 1 loop smell: tool `get_symbol` called 3 times in a row
- `sch-create-large`: cost: 411.6s elapsed, 298.1s agent, 38 provider requests
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 4 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `get_symbol` called 3 times in a row
- `campaign-stm32-buck`: cost: 324.5s elapsed, 285.9s agent, 31 provider requests

## [engine]

- `sch-create-small`: failed check: human_look_schematic_score >= 8 — actual 6
- `sch-create-small`: judge: Recompose the schematic into a compact, balanced layout; the bypass block and driver are separated by excessive horizontal whitespace.
- `sch-create-small`: judge: Move the 100 nF bypass beside J1 and the driver power entry so the supply-support relationship is immediately clear.
- `sch-create-small`: judge: Enlarge the schematic content and improve the visual hierarchy of the small explanatory note for easier reading.
- `sch-create-medium`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "D1", "with": "CANH_BUS"}]
- `sch-create-medium`: failed check: schematic_critic_score >= 8 — actual 5
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-medium`: judge: Reflow the long vertical component chain into compact left-to-right functional blocks with aligned signal flow.
- `sch-create-medium`: judge: Increase symbol, reference, value, and annotation scale so the rendered schematic is legible without excessive zoom.
- `sch-create-medium`: judge: Resolve the D1 value-text overlap with the CANH_BUS label.
- `sch-create-medium`: judge: Separate the crowded MCU-header/top-right area by spacing labels, power symbols, and wire runs consistently.
- `sch-create-medium`: schematic critic: major/spacing/Entire schematic; especially U1 through the lower CAN protection and termination network: Functional blocks are spread down a very long single column with large empty gaps, leaving the connector, protection, termination, indicators, and decoupling parts far apart.
- `sch-create-medium`: schematic critic: major/orientation/CAN connector, TVS protection, and switchable 120 ohm termination region: The CANH/CANL protection and termination network is presented as a long vertical inline stack instead of compact horizontal bus branches with short vertical shunts.
- `sch-create-medium`: schematic critic: minor/spacing/U1 supply area and C1/C2 decoupling capacitors: The decoupling capacitors are visually separated from U1 rather than forming a compact, aligned supply-decoupling bank at the IC.
- `sch-create-large`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "R6", "with": "Net-(U2-BOOT0)"}, {"field": "Value", "ref": "R9", "with": "Net-(U2-PA7)"}, {"field": "Value", "ref": "Y1", "with": "HSE_OUT"}, {"field": "Value", "ref": "Y1", "with": "Net-(U2-PH0)"}]
- `sch-create-large`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 5
- `sch-create-large`: judge: Complete the SWD header by wiring its reset pin to NRST; J2 currently exposes only 3.3 V, SWDIO, SWCLK, and GND.
- `sch-create-large`: judge: Correct the LM2596 feedback divider: the shown resistor values do not produce 3.3 V; recalculate R3/R4 using the regulator reference voltage.
- `sch-create-large`: judge: Wire all STM32 VSS pins to GND; U2.23, U2.35, U2.47, and U2.49 remain on unconnected nets.
- `sch-create-large`: judge: Finish and commit R11 and D6 to the schematic; they remain bench parts with off-grid endpoints, and the checker still reports the NRST and 3.3 V protection gaps.
- `sch-create-large`: judge: Resolve the I2C pull-up checker warnings and verify that R7/R8 are recognized as pull-ups to VOUT_3V3 on SCL/SDA.
- `sch-create-large`: judge: Recompose the schematic into compact functional blocks; excessive vertical sprawl, long MCU loops, wires through symbol bodies, and four text collisions make the rendered schematic difficult to review.
- `sch-create-large`: schematic critic: major/spacing/left-side USB protection and buck-regulator chain; C4-C7/C10 beside U3: The USB connector, protection parts, buck components, feedback network, and catch diode are spread down a tall column with large empty gaps, while the MCU decoupling capacitors are distributed well away from the MCU supply-pin region.
- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: part_count >= 50 — actual 46
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: No PCB was created, routed, rendered, DRC-checked, or exported; fabrication file list is empty.
- `campaign-stm32-buck`: judge: The USB-C D+ and D− pins are isolated on their own connector nets and are not connected to the USBLC6-2SC6 or STM32 PA11/PA12 paths.
- `campaign-stm32-buck`: judge: The required 8 MHz crystal and load capacitors are absent from the connected netlist; PH0 and PH1 remain no-connect.
- `campaign-stm32-buck`: judge: MCU supply decoupling is incomplete: the netlist does not show one 100 nF capacitor per VDD pin, and the required VDDA 1 uF and 3V3 local 4.7 uF bulk support are missing.
- `campaign-stm32-buck`: judge: The four GPIO headers do not provide the requested breakout: J6 and J7 have all signal pins unconnected, leaving only eight connected GPIO signals rather than at least 20.
- `campaign-stm32-buck`: judge: D3 and D4 are electrically flagged as reversed LEDs and remain unresolved; their footprints are also unresolved.
- `campaign-stm32-buck`: judge: Required support circuitry remains incomplete, including the VBAT 4.7 uF bulk capacitor and explicit TPS54302 BOOT/EN bias support.
- `campaign-stm32-buck`: judge: The schematic contains wires passing through MCU/header bodies, a value-field collision between R13 and U2, and three dangling wire endpoints.
- `campaign-stm32-buck`: judge: The delivered design has only 46 parts and does not meet the requested complete, fabrication-ready controller scope.

## [harness]

- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors
- `campaign-stm32-buck`: schematic critic unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.

- `campaign-stm32-buck`: schematic human-look unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.


## [judge]

- `sch-create-small`: schematic human-look: Reduce the extreme horizontal whitespace and enlarge the schematic content for a more readable sheet composition.
- `sch-create-small`: schematic human-look: Align the connector, labels, and nearby power symbols more consistently; the J1 area feels loosely arranged.
- `sch-create-small`: schematic human-look: Tighten the main signal path and standardize spacing between symbols, reference designators, and value text.
- `sch-create-medium`: schematic human-look: Reflow the long vertical component chain into compact functional blocks instead of leaving most of the page as empty whitespace.
- `sch-create-medium`: schematic human-look: Increase symbol and annotation scale so references, values, and notes remain legible at the sheet’s displayed size.
- `sch-create-medium`: schematic human-look: Resolve the crowded top-right area by separating labels, power symbols, and wire runs with consistent spacing.
- `sch-create-large`: schematic human-look: Excessive vertical sprawl and unused whitespace make the schematic difficult to scan as a single design.
- `sch-create-large`: schematic human-look: Long looping wires from the MCU to the upper-right circuitry create visual clutter and obscure functional grouping.
- `sch-create-large`: schematic human-look: Component placement and text alignment are inconsistent, especially around the power section and the densely packed MCU pins.

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: The first place_parts call was refused because the layout schema allowed only one node type, but the error did not provide a valid example.
- `sch-create-small`: struggled: turn 1: The second place_parts call was refused for duplicating Q1 after the layout structure was corrected, requiring manual restructuring.
- `sch-create-small`: struggled: turn 1: The search_footprints discovery budget was exhausted before an LED footprint could be queried.
- `sch-create-small`: struggled: turn 1: place_parts initially reported nets=0, providing insufficient visibility into how intended electrical connections would be realized.
- `sch-create-small`: struggled: turn 1: render_schematic reported zero visual findings without exposing an image preview for direct inspection.
- `sch-create-small`: wished: turn 1: Provide an inline layout-schema example or detailed validation guidance for place_parts.
- `sch-create-small`: wished: turn 1: Allow place_parts to reference a shared component once without treating repeated logical alignment references as duplicate placement.
- `sch-create-small`: wished: turn 1: Reserve discovery budget for all requested component footprints or provide catalog results in the initial search.
- `sch-create-small`: wished: turn 1: Return a connectivity summary mapping every symbol pin to its resulting net after placement.
- `sch-create-small`: wished: turn 1: Expose the rendered schematic image directly or provide OCR/layout inspection details with render_schematic.
- `sch-create-medium`: struggled: turn 1: place_parts reported 20 parts initially but later realized only 19 parts and 14 nets, making component-count results confusing.
- `sch-create-medium`: struggled: turn 1: The render_schematic visual finding did not identify the D1 text overlap location or provide an actionable correction.
- `sch-create-medium`: struggled: turn 1: The power-entry completeness gap appeared only after the main placement instead of being surfaced before layout.
- `sch-create-medium`: struggled: turn 1: check_schematic reported zero ERC issues despite requiring a separate completeness check, splitting electrical validation across tools.
- `sch-create-medium`: wished: turn 1: Add a visual-diagnostics tool that reports exact overlapping references, labels, coordinates, and suggested moves.
- `sch-create-medium`: wished: turn 1: Have place_parts enforce or clearly report the requested non-power-part count and distinguish power symbols from power components.
- `sch-create-medium`: wished: turn 1: Run completeness checks before placement and expose all missing functional blocks in the initial tool response.
- `sch-create-medium`: wished: turn 1: Provide a detailed connectivity/netlist inspection showing each required signal path, connector pin mapping, and protection topology.
- `sch-create-large`: struggled: turn 1: The arrange tool repeatedly left R11 and D6 on the bench despite reporting committed placement and unchanged connectivity.
- `sch-create-large`: struggled: turn 1: The arrange tool rejected a layout.interfaces_indicators field late in the run, blocking the planned placement workflow.
- `sch-create-large`: struggled: turn 1: The check_schematic tool reported missing I2C pull-ups even though R7 and R8 were present, making its completeness analysis confusing.
- `sch-create-large`: struggled: turn 1: The render_schematic tool reported nine visual findings without providing actionable details in the result.
- `sch-create-large`: struggled: turn 1: The verification result said unconnected pins were zero while simultaneously reporting bench parts and completeness gaps, which made the actual completion state unclear.
- `sch-create-large`: struggled: turn 1: The task consumed the full schematic budget and never reached board work because repeated arrange attempts did not close the remaining gaps.
- `sch-create-large`: wished: turn 1: A reliable arrange mode that places bench parts and preserves or explicitly rebuilds their connections.
- `sch-create-large`: wished: turn 1: A tool to query exact component coordinates, pin endpoints, net membership, and bench status before and after layout operations.
- `sch-create-large`: wished: turn 1: Detailed visual-finding output from render_schematic, including component references and coordinates for every collision or off-grid endpoint.
- `sch-create-large`: wished: turn 1: A completeness checker that recognizes pull-ups connected through labels and distinguishes real omissions from false positives.
- `sch-create-large`: wished: turn 1: A single repair or autofix command for standard gaps such as reset pull-ups and supply TVS protection.
- `sch-create-large`: wished: turn 1: A compact schematic summary/report showing required-item coverage, remaining gaps, ERC status, and render quality without requiring repeated full checks.
- `campaign-stm32-buck`: struggled: turn 1: The per-turn wall-clock limit was reached during schematic cleanup, leaving no time to start the board phase.
- `campaign-stm32-buck`: struggled: turn 1: check_schematic reported ERC clean while the overall result remained blocked by deterministic LED-polarity errors and completeness findings.
- `campaign-stm32-buck`: struggled: turn 1: swap_symbol refused to reverse D3 because pin reseating would leave a loose wire end, without providing a direct repair path.
- `campaign-stm32-buck`: struggled: turn 1: The completeness checker emitted irrelevant power-entry warnings for VBAT and VDDA despite the requested USB-powered architecture.
- `campaign-stm32-buck`: struggled: turn 1: Unconnected-wire warnings provided coordinates but no safe automated repair or context identifying the intended connections.
- `campaign-stm32-buck`: struggled: turn 1: No board, routing, DRC, or fabrication-export work could be completed after the schematic consumed the entire turn.
- `campaign-stm32-buck`: wished: turn 1: Provide a wire-preserving pin-swap or explicit wire-endpoint reconnection operation for reversing symbols such as LEDs.
- `campaign-stm32-buck`: wished: turn 1: Make check_schematic distinguish blocking errors from advisory completeness warnings and report a consistent pass/fail status.
- `campaign-stm32-buck`: wished: turn 1: Allow direct editing of symbol pin orientation or swapping electrical pin assignments without requiring geometric reseating.
- `campaign-stm32-buck`: wished: turn 1: Provide schematic inspection results with pin coordinates, connected segment IDs, and nearby net context for diagnosing loose endpoints.
- `campaign-stm32-buck`: wished: turn 1: Add a batch operation to validate and repair standard support circuitry such as BOOT/EN pulls and local capacitors.
- `campaign-stm32-buck`: wished: turn 1: Increase the per-turn time budget or expose a resumable multi-phase workflow that automatically continues from schematic completion into board layout and export.

## [variance]

- `sch-create-small`: provider latency: #1=9600ms, #2=5700ms, #3=12900ms, #4=9500ms, #5=6400ms, #6=8500ms, #7=2900ms, #8=6000ms, #9=3200ms, #10=4200ms
- `sch-create-medium`: cost: 146.7s elapsed, 82.0s agent, 9 provider requests
- `sch-create-medium`: provider latency: #1=7600ms, #2=3500ms, #3=24500ms, #4=4200ms, #5=12700ms, #6=2500ms, #7=9800ms, #8=2100ms, #9=5400ms
- `sch-create-large`: provider latency: #1=5700ms, #2=3500ms, #3=13600ms, #4=9400ms, #5=1600ms, #6=5000ms, #7=3600ms, #8=12300ms, #9=5700ms, #10=4900ms, #11=6100ms, #12=1900ms, #13=5500ms, #14=2900ms, #15=2500ms, #16=6100ms, #17=8900ms, #18=3300ms, #19=4600ms, #20=3700ms, #21=3800ms, #22=4500ms, #23=3900ms, #24=2400ms, #25=3900ms, #26=4900ms, #27=5100ms, #28=5300ms, #29=6700ms, #30=2200ms, #31=3200ms, #32=4100ms, #33=2900ms, #34=6300ms, #35=5300ms, #36=4700ms, #37=2800ms, #38=10100ms
- `campaign-stm32-buck`: provider latency: #1=13700ms, #2=2200ms, #3=12900ms, #4=10300ms, #5=8500ms, #6=4300ms, #7=5900ms, #8=2900ms, #9=4200ms, #10=3800ms, #11=15000ms, #12=3900ms, #13=17100ms, #14=5600ms, #15=16100ms, #16=11000ms, #17=6500ms, #18=16800ms, #19=5200ms, #20=10100ms, #21=6000ms, #22=7000ms, #23=7600ms, #24=2100ms, #25=4600ms, #26=2300ms, #27=14100ms, #28=2500ms, #29=2200ms, #30=3800ms, #31=4600ms
