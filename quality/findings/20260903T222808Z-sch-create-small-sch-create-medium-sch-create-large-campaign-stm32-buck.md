# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260903T222808Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [tool-contract]

- `sch-create-small`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-duplicate-part: `layout.status_led_driver` places `J1` twice; every part gets one place"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-small`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.status_led_driver` places `J2`, which is not a part of that region","layout-unknown-part: `layout.status_led_driver` places `R1`, which is not a part of that region","layout-unknown-part: `layout.status_led_driver` places `Q1`, which is not a part of that region","layout-unknown-part: `layout.status_led_driver` places `R2`, which is not a part of that region","layout-unknown-part: `layout.status_led_driver` places `J1`, which is not a part of that region","layout-unknown-part: `layout.status_led_driver` places `R3`, which is not a part of that region","layout-unknown-part: `layout.status_led_driver` places `D1`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-medium`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.power` places `J1`, which is not a part of that region","layout-unknown-part: `layout.power` places `C1`, which is not a part of that region","layout-unknown-part: `layout.power` places `C2`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `sch-create-large`: turn 1 tool `place_parts` error: no completion recorded
- `sch-create-large`: turn 1 tool `search_symbols` refusal: {"error":"`queries` must contain 1 to 10 searches"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.buck_regulator`: a layout node is exactly one of `part`, `row` or `col`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `Y1`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"part":"Device:Crystal","reason":"pin keys `3`, `4` not found on Y1 (Device:Crystal)","ref":"Y1"}],"unreliable_nets":[]}
- `campaign-stm32-buck`: turn 1 tool `place_parts` error: no completion recorded
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.power_buck` places `D1`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"did_you_mean":["Diode:SMAJ5.0A","Diode:SMAJ6.0A","Diode:SMAJ7.0A"],"part":"Power_Protection:SMBJ5.0A","reason":"D1: Power_Protection:SMBJ5.0A not found in any library; did you mean Diode:SMAJ5.0A, Diode:SMAJ6.0A, Diode:SMAJ7.0A?","ref":"D1"}],"unreliable_nets":[],"warnings":["mapped intent.rails.+5V from left to the engine-supported top band","mapped intent.rails.3V3 from right to the engine-supported bottom band"]}
- `campaign-stm32-buck`: turn 1 tool `search_symbols` refusal: {"error":"`queries` must contain 1 to 10 searches"}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"VCAP1","on_sheet":false,"pin":"1","pins_on_net":1,"ref":"C16"},{"net":"VCAP2","on_sheet":false,"pin":"1","pins_on_net":1,"ref":"C17"},{"net":"I2C_SCL","on_sheet":false,"pin":"2","pins_on_net":1,"ref":"R8"},{"net":"I2C_SDA","on_sheet":false,"pin":"2","pins_on_net":1,"ref":"R9"},{"net":"GPIO_PA0","on_sheet":false,"pin":"PA0","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA1","on_sheet":false,"pin":"PA1","pins_on_net":0,"ref":"U2"},{"net":"USB_DM_MCU","on_sheet":false,"pin":"PA11","pins_on_net":0,"ref":"U2"},{"net":"USB_DP_MCU","on_sheet":false,"pin":"PA12","pins_on_net":0,"ref":"U2"},{"net":"SWDIO","on_sheet":false,"pin":"PA13","pins_on_net":0,"ref":"U2"},{"net":"SWCLK","on_sheet":false,"pin":"PA14","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA2","on_sheet":false,"pin":"PA2","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA3","on_sheet":false,"pin":"PA3","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA4","on_sheet":false,"pin":"PA4","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA5","on_sheet":false,"pin":"PA5","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA6","on_sheet":false,"pin":"PA6","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PA7","on_sheet":false,"pin":"PA7","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB0","on_sheet":false,"pin":"PB0","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB1","on_sheet":false,"pin":"PB1","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB10","on_sheet":false,"pin":"PB10","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB11","on_sheet":false,"pin":"PB11","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB12","on_sheet":false,"pin":"PB12","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB13","on_sheet":false,"pin":"PB13","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB14","on_sheet":false,"pin":"PB14","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB15","on_sheet":false,"pin":"PB15","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PB2","on_sheet":false,"pin":"PB2","pins_on_net":0,"ref":"U2"},{"net":"SWO","on_sheet":false,"pin":"PB3","pins_on_net":0,"ref":"U2"},{"net":"I2C_SCL","on_sheet":false,"pin":"PB8","pins_on_net":1,"ref":"U2"},{"net":"I2C_SDA","on_sheet":false,"pin":"PB9","pins_on_net":1,"ref":"U2"},{"net":"GPIO_PC0","on_sheet":false,"pin":"PC0","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC1","on_sheet":false,"pin":"PC1","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC10","on_sheet":false,"pin":"PC10","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC11","on_sheet":false,"pin":"PC11","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC12","on_sheet":false,"pin":"PC12","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC2","on_sheet":false,"pin":"PC2","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC3","on_sheet":false,"pin":"PC3","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC4","on_sheet":false,"pin":"PC4","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC5","on_sheet":false,"pin":"PC5","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC6","on_sheet":false,"pin":"PC6","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC7","on_sheet":false,"pin":"PC7","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC8","on_sheet":false,"pin":"PC8","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PC9","on_sheet":false,"pin":"PC9","pins_on_net":0,"ref":"U2"},{"net":"GPIO_PD2","on_sheet":false,"pin":"PD2","pins_on_net":0,"ref":"U2"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `U2`, which is not a part of that region"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"did_you_mean":["USB_OTG_HS_ULPI_D0"],"part":"MCU_ST_STM32F4:STM32F405RGTx","reason":"pin keys `PD0`, `PD1`, `PD3`, `PD4`, `PD5`, `PD6`, `PD7`, `PE0`, `PE1`, `PE10`, `PE11`, `PE12`, `PE13`, `PE14`, `PE15`, `PE2`, `PE3`, `PE4`, `PE5`, `PE6`, `PE7`, `PE8`, `PE9`, `VCAP1`, `VCAP2` not found on U2 (MCU_ST_STM32F4:STM32F405RGTx)","ref":"U2"},{"did_you_mean":["Switch:SW_DPST","Switch:SW_DPST_Temperature","Switch:SW_DPST_x2"],"part":"Button_Switch_SMD:SW_SPST_TL3342","reason":"SW1: Button_Switch_SMD:SW_SPST_TL3342 not found in any library; did you mean Switch:SW_DPST, Switch:SW_DPST_Temperature, Switch:SW_DPST_x2?","ref":"SW1"}],"unreliable_nets":[]}

## [prompt]

- `sch-create-small`: turn 1 loop smell: tool `place_parts` called 4 times in a row
- `sch-create-small`: cost: 101.5s elapsed, 46.7s agent, 8 provider requests
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 4 times in a row
- `sch-create-large`: cost: 158.0s elapsed, 84.8s agent, 14 provider requests

## [engine]

- `sch-create-small`: failed check: human_look_schematic_score >= 8 — actual 6
- `sch-create-small`: judge: Move the supply-bypass block closer to the main circuit to eliminate the excessive empty canvas.
- `sch-create-small`: judge: Rearrange the LED, transistor, and resistor network into a tighter orthogonal signal-flow layout with shorter wires and fewer jogs.
- `sch-create-small`: judge: Improve spacing and alignment of connector labels, reference designators, values, and control-input annotations.
- `sch-create-small`: schematic critic: minor/spacing/Q1, R2, and J1 lower portion of the main driver block: The pulldown resistor and power connector are placed substantially below the active transistor/LED circuitry, leaving avoidable vertical whitespace in the driver block.
- `sch-create-small`: schematic critic: minor/off-spine-leg/R1 lower pin to Q1 base node: R1's lower connection runs left and then down instead of aligning the resistor directly with the base node.
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — actual 6
- `sch-create-medium`: judge: Re-space U1 and its nearby net labels; pin names, labels, and wires are visibly crowded around the transceiver.
- `sch-create-medium`: judge: Fix the +3V3 wire running through the J1 connector body and route it cleanly to the appropriate pin.
- `sch-create-medium`: judge: Rebalance the schematic layout: reduce the large unused gaps and align the supply, indicator, EMI, and termination components to consistent grid rows and columns.
- `sch-create-medium`: judge: Standardize section-box margins, title placement, and annotation orientation; the mixed vertical and horizontal text makes the schematic harder to scan.
- `sch-create-medium`: schematic critic: minor/spacing/R8, far right of the transceiver region: The 10k RX pull-up is an isolated satellite separated from the MCU-side header and transceiver by a large empty area, making that small signal-conditioning function less visually associated with its circuit.
- `sch-create-large`: failed check: agent_exit == 0 — actual -6
- `sch-create-large`: failed check: part_count >= 35 — actual 16
- `sch-create-large`: failed check: schematic_critic_score >= 8 — actual 7
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 6
- `sch-create-large`: judge: Complete the missing MCU, I2C sensor, SWD header, reset network, BOOT strap, 8 MHz crystal/load capacitors, and two LED indicator circuits; only the USB-C entry and buck sections were delivered.
- `sch-create-large`: judge: Connect the STM32 supply pins with one dedicated decoupling capacitor per supply pin and wire its I2C, SWD, reset, boot, and crystal pins to real functions.
- `sch-create-large`: judge: Finish and verify the schematic after the MCU placement crash; the run exited with -6 and no final completion was recorded.
- `sch-create-large`: judge: Re-layout the USB-C/protection section into a compact, aligned left-to-right power path with consistent labels, spacing, and tighter functional grouping.
- `sch-create-large`: schematic critic: major/spacing/USB-C Power Entry & Protection region: The CC pulldowns, fuse, clamp devices, and input capacitors are scattered across a large mostly empty region instead of being grouped closely around J1 and the protected-input node.
- `campaign-stm32-buck`: failed check: agent_exit == 0 — actual -6
- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: part_count >= 50 — actual 12
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — actual 6
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — actual 6
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: Add the missing STM32F405RGTx MCU and complete all supply domains, VCAP capacitors, reset, boot, clock, and GPIO connections.
- `campaign-stm32-buck`: judge: Add the USB-C interface, CC pulldowns, USBLC6-2SC6 protection, USB series resistors, VBUS sensing divider, SWD header, LEDs, I2C pull-ups, and four GPIO headers.
- `campaign-stm32-buck`: judge: Resolve the reported missing BOOT_SW pull-down and verify the buck stage against the requested component and footprint specifications.
- `campaign-stm32-buck`: judge: Expand the schematic from 12 parts to the complete requested design; current delivery contains only a partial buck power block.
- `campaign-stm32-buck`: judge: Create and route the two-layer PCB with accessible edge connectors, tight power/crystal placement, ground pour, and appropriate power widths.
- `campaign-stm32-buck`: judge: Run PCB DRC and verify zero unrouted or unconnected items; DRC was not run and no PCB was created.
- `campaign-stm32-buck`: judge: Export Gerbers, drill, pick-and-place, and BOM files into the fabrication directory.
- `campaign-stm32-buck`: judge: Improve schematic organization by tightening the oversized border, aligning the power flow, and standardizing reference/value placement.
- `campaign-stm32-buck`: schematic critic: major/spacing/U1 TPS54302, C1/C2, C5, L1, and C3/C4: The switching-power components are distributed across a large area instead of being clustered around U1, leaving large empty gaps and weakening the visual representation of a tight buck power loop.

## [harness]

- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors

## [judge]

- `sch-create-small`: schematic human-look: Reduce the excessive empty canvas between the supply-bypass block and the main circuit.
- `sch-create-small`: schematic human-look: Rework the main circuit into a tighter, more orthogonal arrangement with shorter wires and fewer jogs.
- `sch-create-small`: schematic human-look: Align reference/value labels consistently and give the connector and control-input annotations more deliberate spacing.
- `sch-create-medium`: schematic human-look: Re-space U1 and its nearby net labels; several pin names, labels, and wires are visibly crowded or overlapping.
- `sch-create-medium`: schematic human-look: Align the power, indicator, and termination elements to consistent grid columns and rows instead of leaving large uneven gaps.
- `sch-create-medium`: schematic human-look: Standardize section-box margins, title placement, and annotation orientation; the mixed vertical and horizontal text makes the page harder to scan.
- `sch-create-large`: schematic human-look: The USB-C protection block is excessively spread out, leaving large unused areas and making related parts read as disconnected islands.
- `sch-create-large`: schematic human-look: The power-entry path lacks a consistent left-to-right visual flow; fuse, clamp parts, capacitors, and pull-downs are arranged on different visual levels.
- `sch-create-large`: schematic human-look: Several short wire stubs, isolated labels, and the buck converter’s feedback wiring create a rough, mechanically assembled appearance rather than polished schematic drafting.
- `campaign-stm32-buck`: schematic human-look: Replace the scattered floating component groups with clearly aligned functional blocks and consistent spacing.
- `campaign-stm32-buck`: schematic human-look: Remove or substantially tighten the oversized dashed border and unused white space.
- `campaign-stm32-buck`: schematic human-look: Standardize reference/value label placement and alignment, especially around R1–R3, C5, and the regulator pins.

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: place_parts initially refused an otherwise valid layout because J1 was listed twice for its two units, without clearly distinguishing multi-unit placement from duplicate references.
- `sch-create-small`: struggled: turn 1: place_parts rejected the bypass addition when the payload repeated existing parts, making the append-only behavior easy to misuse.
- `sch-create-small`: struggled: turn 1: place_parts reported completeness gaps but did not provide a direct schematic-level fix or an explicit required component specification.
- `sch-create-small`: struggled: turn 1: check_schematic reported zero ERC findings but did not expose the resolved net topology or confirm the LED, resistor, transistor, and connector pin connections.
- `sch-create-small`: struggled: turn 1: render_schematic reported zero visual findings without providing image content for direct review in the transcript.
- `sch-create-small`: wished: turn 1: place_parts should support explicit multi-unit symbols, such as J1.1 and J1.2, without treating them as duplicate parts.
- `sch-create-small`: wished: turn 1: place_parts should accept a clearly scoped incremental layout containing only newly added parts and state the existing region contents.
- `sch-create-small`: wished: turn 1: Completeness diagnostics should include machine-readable suggested parts, values, and exact placement or wiring requirements.
- `sch-create-small`: wished: turn 1: check_schematic should return a concise pin-to-net connectivity table and validate the requested circuit topology, not only ERC cleanliness.
- `sch-create-small`: wished: turn 1: render_schematic should return an inline preview or structured visual inspection summary covering labels, readability, and wire crossings.
- `sch-create-medium`: struggled: turn 1: search_footprints returned blank library-hit summaries, making footprint selection and validation unclear.
- `sch-create-medium`: struggled: turn 1: place_parts refused the power update because the layout referenced existing parts outside the appended block, requiring an indirect add_parts plus arrange workaround.
- `sch-create-medium`: struggled: turn 1: The first place_parts result reported nets=0 even though the later realise phase created 13 nets, making connectivity status confusing.
- `sch-create-medium`: struggled: turn 1: The renderer flagged a wire-through-body issue at J1, but provided no actionable location or correction guidance.
- `sch-create-medium`: struggled: turn 1: check_schematic reported a power-entry-protection gap without a direct fix mechanism, requiring manual interpretation.
- `sch-create-medium`: struggled: turn 1: The tool did not expose a concise final connectivity or pin-to-net summary for verifying the rendered schematic.
- `sch-create-medium`: wished: turn 1: Provide detailed search_footprints results including selected library IDs, pad counts, and compatibility rationale.
- `sch-create-medium`: wished: turn 1: Allow place_parts to append a component into an existing named block without requiring all existing block parts in the layout payload.
- `sch-create-medium`: wished: turn 1: Add a targeted tool to insert and arrange one component into an existing region while preserving its existing layout.
- `sch-create-medium`: wished: turn 1: Make render_schematic report visual findings with component references, coordinates, and suggested fixes.
- `sch-create-medium`: wished: turn 1: Have check_schematic offer an automatic fix or explicit add_parts payload for each completeness gap.
- `sch-create-medium`: wished: turn 1: Provide a final schematic report listing every reference, pin, net, value, footprint, and connection status.
- `sch-create-large`: struggled: turn 1: place_parts initially reported zero completeness gaps despite creating only eight parts and no nets, obscuring that most requested circuitry was missing.
- `sch-create-large`: struggled: turn 1: get_symbol_info returned blank or zero-pin results for several requested symbols without clearly identifying unavailable library IDs or alternatives.
- `sch-create-large`: struggled: turn 1: search_footprints reported only hit counts rather than the actual footprint names and pin-compatible recommendations.
- `sch-create-large`: struggled: turn 1: place_parts rejected the first buck layout with a generic one-node layout error, and the crystal retry confusingly reported Y1 as both unknown and unplaced.
- `sch-create-large`: struggled: turn 1: The final MCU placement crashed the schematic-typeset thread with a stack overflow, preventing completion and verification.
- `sch-create-large`: wished: turn 1: Provide a structured build manifest showing every requested component, resolved symbol, pins, nets, and omitted items before placement is committed.
- `sch-create-large`: wished: turn 1: Return explicit unavailable-symbol diagnostics with suggested valid library replacements and complete pin maps.
- `sch-create-large`: wished: turn 1: Make footprint search return concrete footprint IDs, pad mappings, and compatibility scores for each symbol.
- `sch-create-large`: wished: turn 1: Validate part references and pin keys before layout processing, with an actionable error that identifies the exact malformed field.
- `sch-create-large`: wished: turn 1: Add a robust large-schematic typesetting path that avoids stack overflow and supports incremental rendering.
- `sch-create-large`: wished: turn 1: Provide a connectivity inspection tool or netlist preview so power, reset, clock, SWD, and I2C wiring can be verified before rendering.
- `campaign-stm32-buck`: struggled: turn 1: search_symbols refused a multi-query request with a misleading “1 to 10 searches” error, despite the request appearing to contain a valid batch.
- `campaign-stm32-buck`: struggled: turn 1: get_symbol_info and place_parts exposed a symbol mismatch: STM32F405RGTx lacked expected VCAP1, VCAP2, and many GPIO pin keys, preventing the required MCU placement.
- `campaign-stm32-buck`: struggled: turn 1: place_parts rejected a layout referencing U2 as an unknown regional part even though U2 was included in the same placement request.
- `campaign-stm32-buck`: struggled: turn 1: The requested Button_Switch_SMD:SW_SPST_TL3342 footprint or symbol was unavailable and the refusal only suggested unrelated switch symbols.
- `campaign-stm32-buck`: struggled: turn 1: The schematic tool reported dangling nets and unresolved MCU connections but did not provide a direct net-assignment or pin-mapping repair operation.
- `campaign-stm32-buck`: struggled: turn 1: The schematic typesetting phase crashed with a stack overflow before routing, DRC/ERC completion, rendering both views, or fabrication export.
- `campaign-stm32-buck`: wished: turn 1: Provide authoritative symbol-library availability and complete pin maps, including alternate pin names and power pins, before placement begins.
- `campaign-stm32-buck`: wished: turn 1: Add a transactional schematic builder that validates symbols, regions, references, and connectivity incrementally with actionable repair commands.
- `campaign-stm32-buck`: wished: turn 1: Allow explicit pin/net connection edits after placement, including attaching dangling capacitor, pull-up, GPIO, VCAP, and USB nets.
- `campaign-stm32-buck`: wished: turn 1: Return clear diagnostics explaining why a requested part or footprint is unavailable and offer verified compatible replacements.
- `campaign-stm32-buck`: wished: turn 1: Add a lightweight schematic layout mode or pagination/size controls that cannot crash on larger blocks.
- `campaign-stm32-buck`: wished: turn 1: Provide integrated autorouting, ERC/DRC diagnostics, render, and fabrication-export tools with machine-readable completion status.

## [variance]

- `sch-create-small`: provider latency: #1=4900ms, #2=3000ms, #3=9100ms, #4=7500ms, #5=3700ms, #6=3200ms, #7=4600ms, #8=5000ms
- `sch-create-medium`: cost: 130.8s elapsed, 74.3s agent, 12 provider requests
- `sch-create-medium`: provider latency: #1=7700ms, #2=2500ms, #3=3600ms, #4=3400ms, #5=18500ms, #6=1400ms, #7=6400ms, #8=2700ms, #9=5800ms, #10=2200ms, #11=1900ms, #12=6100ms
- `sch-create-large`: provider latency: #1=5400ms, #2=3400ms, #3=5400ms, #4=3600ms, #5=7700ms, #6=1700ms, #7=6200ms, #8=3000ms, #9=9100ms, #10=6000ms, #11=2400ms, #12=2900ms, #13=6600ms, #14=5100ms
- `campaign-stm32-buck`: cost: 121.8s elapsed, 76.4s agent, 9 provider requests
- `campaign-stm32-buck`: provider latency: #1=4200ms, #2=2000ms, #3=9900ms, #4=7000ms, #5=1700ms, #6=4400ms, #7=10600ms, #8=4600ms, #9=20200ms
