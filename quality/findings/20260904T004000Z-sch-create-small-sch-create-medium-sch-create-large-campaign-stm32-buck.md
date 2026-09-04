# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260904T004000Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [tool-contract]

- `sch-create-small`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.status_led_driver.col[1].row[0].value`: unknown field `value`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `sch-create-small`: turn 1 tool `move_symbols` refusal: {"error":"refused: dragging J1, C1, R3, D1, Q1, J2, R1, R2 was refused (drag would leave 1 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `sch-create-small`: turn 1 tool `move_symbols` refusal: {"error":"refused: dragging C1, R3, D1, Q1, J2, R1, R2 was refused (drag would leave 1 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `sch-create-small`: turn 1 tool `get_net` refusal: {"error":"no net `LED_K`","nets":["+5V","BASE","CTRL","GND","Net-(D1-A)","Net-(D1-K)"]}
- `sch-create-small`: turn 1 tool `get_net` refusal: {"error":"no net `LED_A`","nets":["+5V","BASE","CTRL","GND","Net-(D1-A)","Net-(D1-K)"]}
- `sch-create-large`: turn 1 tool `search_symbols` refusal: {"error":"`queries` must contain 1 to 10 searches"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `parts[6]`: missing field `part`"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[{"net":"BOOT0","on_sheet":false,"pin":"1","pins_on_net":1,"ref":"R8"},{"net":"BOOT0","on_sheet":false,"pin":"BOOT0","pins_on_net":1,"ref":"U2"},{"net":"ALERT","on_sheet":false,"pin":"4","pins_on_net":1,"ref":"U3"}],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.mcu_core` places `C6`, which is not a part of that region — region `mcu_core` holds Y1, C10, C11, R7, C12, R8, R9, R10, J2, U3, R11, D2","layout-unknown-part: `layout.mcu_core` places `C7`, which is not a part of that region — region `mcu_core` holds Y1, C10, C11, R7, C12, R8, R9, R10, J2, U3, R11, D2","layout-unknown-part: `layout.mcu_core` places `C8`, which is not a part of that region — region `mcu_core` holds Y1, C10, C11, R7, C12, R8, R9, R10, J2, U3, R11, D2","layout-unknown-part: `layout.mcu_core` places `C9`, which is not a part of that region — region `mcu_core` holds Y1, C10, C11, R7, C12, R8, R9, R10, J2, U3, R11, D2"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[{"did_you_mean":["RCC_OSC_IN"],"part":"MCU_ST_STM32F0:STM32F072CBTx","reason":"pin keys `PH0-OSC_IN`, `PH1-OSC_OUT` not found on U2 (MCU_ST_STM32F0:STM32F072CBTx)","ref":"U2"}],"unreliable_nets":[]}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"code":"bench_mismatch","error":"the bench draw did not preserve connectivity; nothing was written","ok":false,"report":{"benched":[{"ref":"C1","why":"could not be drawn truthfully (scattered +3V3)"},{"ref":"C2","why":"could not be drawn truthfully (scattered +3V3)"},{"ref":"C3","why":"could not be drawn truthfully (scattered +3V3)"},{"ref":"C4","why":"could not be drawn truthfully (scattered +3V3)"},{"ref":"U2","why":"could not be drawn truthfully (scattered +3V3)"}],"committed":false,"dangling":[{"net":"BOOT0","on_sheet":false,"pin":"BOOT0","pins_on_net":1,"ref":"U2"},{"net":"NRST","on_sheet":false,"pin":"NRST","pins_on_net":1,"ref":"U2"},{"net":"LED_STATUS","on_sheet":false,"pin":"PA5","pins_on_net":1,"ref":"U2"},{"net":"LED_POWER","on_sheet":false,"pin":"PA6","pins_on_net":1,"ref":"U2"},{"net":"I2C_SCL","on_sheet":false,"pin":"PB6","pins_on_net":1,"ref":"U2"},{"net":"I2C_SDA","on_sheet":false,"pin":"PB7","pins_on_net":1,"ref":"U2"},{"net":"OSC_IN","on_sheet":false,"pin":"RCC_OSC_IN","pins_on_net":1,"ref":"U2"},{"net":"OSC_OUT","on_sheet":false,"pin":"RCC_OSC_OUT","pins_on_net":1,"ref":"U2"}],"mismatch":{"disturbed":["+3V3","VBUS_PROTECTED"],"scattered":[],"shorted":[]},"nets":["+3V3","BOOT0","GND","I2C_SCL","I2C_SDA","LED_POWER","LED_STATUS","NRST","OSC_IN","OSC_OUT","SWCLK","SWDIO"]}}
- `sch-create-large`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `intent.rails.+3V3`: unknown variant `right`, expected `top` or `bottom`"}
- `sch-create-large`: turn 1 tool `add_parts` refusal: {"error":"checking benched parts: loading footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`: unknown footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`"}
- `sch-create-large`: turn 1 tool `add_parts` refusal: {"error":"checking benched parts: loading footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`: unknown footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`"}
- `sch-create-large`: turn 1 tool `add_parts` refusal: {"error":"checking benched parts: loading footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`: unknown footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`"}
- `sch-create-large`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.power_buck`: unknown field `power_buck`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `sch-create-large`: turn 1 tool `arrange` refusal: {"error":"loading footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`: unknown footprint `Sensor_Temperature:Texas_XLLP-6-1EP_2x2mm_P0.65mm`"}
- `sch-create-large`: turn 1 tool `set_fields` refusal: {"error":"set_fields does not set Footprint; use assign_footprints so symbol compatibility is validated"}
- `sch-create-large`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.col[3]`: a layout node is exactly one of `part`, `row` or `col`"}
- `sch-create-large`: turn 1 tool `connect` refusal: {"error":"refused: the edit would change connectivity the call did not name (LED_STATUS); nothing was written","net_delta":{"merged":[[["LED_POWER","LED_STATUS"],"LED_POWER"]]}}
- `campaign-stm32-buck`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `intent.rails.3V3`: unknown variant `right`, expected `top` or `bottom`"}
- `campaign-stm32-buck`: turn 1 tool `arrange` refusal: {"error":"invalid arrange input at `layout.power_buck`: unknown field `power_buck`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.col[0]`: invalid type: map, expected a string"}
- `campaign-stm32-buck`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.mcu_core.col[0]`: invalid type: string \"U2\", expected struct Wire"}
- `campaign-stm32-buck`: turn 1 tool `connect` refusal: {"connected":[{"error":"refused: the edit would change connectivity the call did not name (CC1); nothing was written","from":"J4.B5","net_delta":{"merged":[[["CC1","CC2"],"CC1"]],"now_connected":["J4.B5"]},"to":"R10.1"},{"error":"refused: the edit would change connectivity the call did not name (USB_RAW_DP); nothing was written","from":"J4.B7","net_delta":{"merged":[[["USB_RAW_DM","USB_RAW_DP"],"USB_RAW_DM"]],"now_connected":["J4.B7"]},"to":"U3.6"}],"error":"all 2 connections failed — J4.B5 -> R10.1: refused: the edit would change connectivity the call did not name (CC1); nothing was written; J4.B7 -> U3.6: refused: the edit would change connectivity the call did not name (USB_RAW_DP); nothing was written"}
- `campaign-stm32-buck`: turn 1 tool `connect` refusal: {"connected":[{"error":"refused: the edit would change connectivity the call did not name (CC1); nothing was written","from":"J4.B5","net_delta":{"merged":[[["CC1","CC2"],"CC1"]]},"to":"R10.1"},{"error":"refused: the edit would change connectivity the call did not name (USB_RAW_DP); nothing was written","from":"J4.B7","net_delta":{"merged":[[["USB_RAW_DM","USB_RAW_DP"],"USB_RAW_DM"]]},"to":"U3.6"}],"error":"all 2 connections failed — J4.B5 -> R10.1: refused: the edit would change connectivity the call did not name (CC1); nothing was written; J4.B7 -> U3.6: refused: the edit would change connectivity the call did not name (USB_RAW_DP); nothing was written"}

## [prompt]

- `sch-create-small`: turn 1 loop smell: tool `move_symbols` called 3 times in a row
- `sch-create-small`: turn 1 loop smell: tool `get_net` called 4 times in a row
- `sch-create-small`: turn 1 loop smell: tool `get_symbol` called 6 times in a row
- `sch-create-small`: cost: 231.3s elapsed, 167.0s agent, 30 provider requests
- `sch-create-large`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `sch-create-large`: turn 1 loop smell: tool `add_parts` called 4 times in a row
- `sch-create-large`: turn 1 loop smell: tool `get_symbol` called 3 times in a row
- `sch-create-large`: cost: 357.9s elapsed, 272.1s agent, 51 provider requests
- `campaign-stm32-buck`: turn 1 loop smell: tool `arrange` called 3 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `get_net` called 4 times in a row
- `campaign-stm32-buck`: cost: 413.1s elapsed, 323.1s agent, 31 provider requests

## [engine]

- `sch-create-small`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-small`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-small`: judge: Re-layout the schematic into a compact, clear left-to-right signal flow: J1/+5V, R3/D1, Q1, and the CTRL/base network are currently widely scattered.
- `sch-create-small`: judge: Replace long jogged wires and isolated-looking fragments with short orthogonal connections and clearly grouped functional blocks.
- `sch-create-small`: judge: Standardize symbol orientations and align references, values, power labels, and connector annotations to improve readability.
- `sch-create-small`: judge: Visually group the power connector and bypass capacitor near the LED/transistor stage, and place the CTRL connector directly beside R1/R2.
- `sch-create-small`: schematic critic: major/off-spine-leg/J2, CTRL route, and R1: The CTRL net takes an avoidable multi-bend path rightward, downward, and then rightward again before reaching R1, while J2 branches from the lower horizontal segment.
- `sch-create-small`: schematic critic: major/spacing/J1/C1 supply block versus R3/D1/Q1 driver block: The supply connector and decoupling capacitor are separated from the actual driver circuitry by a large empty region, making the compact 5 V driver read as disconnected functional islands.
- `sch-create-medium`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "D2", "with": "CANH_BUS"}]
- `sch-create-medium`: failed check: schematic_critic_score >= 8 — actual 7
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — actual 6
- `sch-create-medium`: judge: The switchable termination is 240 Ω, not 120 Ω: R1 and R2 are each 120 Ω and are connected in series through SW1. Use one 120 Ω resistor with the switch, or change the resistor values/topology.
- `sch-create-medium`: judge: F1/F2 and common-mode choke L1 are not in the transceiver bus path: U1, J2, and the TVS devices are all directly on CANH_BUS/CANL_BUS, while CANH_PROTECTED/CANL_PROTECTED only connect fuse outputs to choke pins. Correct the intended protected-side connectivity or remove the misleading protection elements.
- `sch-create-medium`: judge: Rework the schematic layout into functional zones and reduce the large unused lower/right areas; align the transceiver, bus path, termination, and protection circuitry.
- `sch-create-medium`: judge: Rotate or reposition the vertically stacked TVS/fuse symbols so their values and net labels are horizontal and readable.
- `sch-create-medium`: judge: Resolve the D2 value-field collision with the CANH_BUS label and reduce crowded annotations around J1, U1, and the bus connections.
- `sch-create-medium`: schematic critic: major/spacing/Right-side R3/R6/R7/R8 branches and lower-left R4/D3 and R5/D4 indicator branches: Several peripheral circuits are placed far from U1 and the bus interface, leaving large unused regions and making the schematic read as spatially disconnected blocks.
- `sch-create-large`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "J2", "with": "SWCLK"}]
- `sch-create-large`: failed check: schematic_critic_score >= 8 — actual 6
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 3
- `sch-create-large`: judge: The I2C sensor is missing from the delivered schematic; add the TMP102 (or equivalent) with VCC, GND, SDA, SCL, and address/alert pins correctly handled.
- `sch-create-large`: judge: Tie MCU VBAT and VDDIO2 to the intended 3.3 V rail rather than leaving them on isolated VBAT/VDDIO2 power symbols; add local 100 nF decoupling and required bulk capacitors.
- `sch-create-large`: judge: Fix the schematic presentation: regroup the power, MCU, and interface blocks, remove the large unused canvas, and replace long perimeter wires with clear local wiring/net labels.
- `sch-create-large`: judge: Move the SWD header value field so it no longer collides with the SWCLK annotation.
- `sch-create-large`: judge: Reroute I2C wiring so it does not pass through the MCU symbol body; keep connections outside symbol graphics and use short, readable branches.
- `sch-create-large`: judge: Re-render and run the final schematic check after eliminating the six benched interface/indicator parts and confirming the sensor and pull-up connections.
- `sch-create-large`: schematic critic: major/spacing/Overall sheet; lower USB/protection/buck block relative to the upper MCU/support circuitry: The power-entry and regulator circuitry is placed far below the MCU and upper support circuitry, leaving a very large blank region and producing unnecessarily long cross-sheet runs.
- `campaign-stm32-buck`: failed check: unconnected_items == 0 — actual 127
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — actual 1
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — actual 4
- `campaign-stm32-buck`: judge: Route all 41 PCB connections; current board has 0 tracks, 0 vias, and 127 unconnected items.
- `campaign-stm32-buck`: judge: Resolve the 118 blocking DRC findings, including the extensive pad-blocked and missing-connection violations.
- `campaign-stm32-buck`: judge: Assign valid footprints and place the staged J2, J4, R13, and R14 components; the board is not fabrication-ready.
- `campaign-stm32-buck`: judge: Export the required Gerbers, drill files, position file, and BOM into the fab directory; none are present.
- `campaign-stm32-buck`: judge: Fix schematic visual quality: eliminate the reported 30 text/label overlap findings and wires through component bodies.
- `campaign-stm32-buck`: judge: Complete the remaining schematic support gaps, especially the TPS54302 EN pull network and verify the VDDA support circuitry.
- `campaign-stm32-buck`: judge: Verify STM32 supply implementation against the request, including distinct VBAT decoupling and correct per-pin VDD bypass placement.
- `campaign-stm32-buck`: judge: Recheck USB-C duplicate pin connectivity and USB protection routing after correcting the unverified connector/protection edits.
- `campaign-stm32-buck`: pcb critic: critical/routing-neatness/Entire board, especially U2, U1, and the connector/header pads: No visible pad-to-pad copper traces or completed via transitions are present; route the board completely, keeping the buck switch node compact and routing USB, MCU, power, and header nets with direct two-layer paths.
- `campaign-stm32-buck`: pcb critic: major/placement/J5 and the MCU/power regions: J5 is located well inside the board instead of on an accessible edge; move the SWD header to an edge and place the power, USB, and debug interfaces so they can be connected and used without routing through the logic area.
- `campaign-stm32-buck`: pcb critic: major/board-utilisation/Central-lower and lower-right board regions: The outline leaves substantial low-density area around the populated regions; reduce the board outline or spread the functional blocks only as needed to create a compact, routable board.
- `campaign-stm32-buck`: pcb critic: major/placement/U2, Y1, and the nearby crystal capacitors: Move Y1 and its two load capacitors immediately beside the PH0/PH1 side of U2, and place each MCU decoupler directly beside its corresponding supply pins rather than distributing the capacitors through the surrounding area.
- `campaign-stm32-buck`: pcb critic: minor/silkscreen/Central passive cluster around C8, C10, C14, C15, C18, and nearby resistors: Several reference designators are crowded into the passive cluster; move the references into clear spaces adjacent to their own footprints and rotate them consistently.
- `campaign-stm32-buck`: pcb critic: major/placement/USB interface region: No recognizable USB-C receptacle footprint is visible; verify that the specified USB-C connector is actually present and place it on a board edge with the USB protection and series resistors between it and the MCU.

## [harness]

- `campaign-stm32-buck`: schematic critic unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.

- `campaign-stm32-buck`: schematic human-look unavailable: command failed (1): magick -density 200 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.svg -trim +repage -bordercolor white -border 24 -background white -alpha remove -alpha off -resize 1600x900 /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees/campaign-stm32-buck/artifacts/phase-1-schematic-clean.png

magick: vector graphics nested too deeply `stroked-text' @ error/draw.c/RenderMVGContent/2808.


## [judge]

- `sch-create-small`: schematic human-look: Re-layout the circuit into a compact, left-to-right signal flow instead of scattering related parts across the sheet.
- `sch-create-small`: schematic human-look: Align symbols, labels, and values to a consistent grid; several annotations are awkwardly placed or visually collide with nearby wiring.
- `sch-create-small`: schematic human-look: Replace the long jogged wires and isolated-looking fragments with short orthogonal connections and clearly grouped functional blocks.
- `sch-create-medium`: schematic human-look: Rebalance the composition to eliminate the large unused lower-center and lower-right areas.
- `sch-create-medium`: schematic human-look: Rework the vertically stacked bus protection and fuse graphics so labels are horizontal, readable, and consistently spaced.
- `sch-create-medium`: schematic human-look: Resolve crowded text and label collisions around J1, U1, D1/D2, and the CAN bus connections.
- `sch-create-large`: schematic human-look: Eliminate the enormous unused canvas and regroup the MCU, interfaces, and power circuitry into compact functional blocks.
- `sch-create-large`: schematic human-look: Replace long perimeter-running wires with shorter local connections and clear net labels.
- `sch-create-large`: schematic human-look: Align symbols, rails, and annotations to a consistent grid; keep all labels and circuitry inside the page bounds.
- `campaign-stm32-buck`: pcb human-look: No visible trace geometry on the copper view, leaving the board visually unfinished.
- `campaign-stm32-buck`: pcb human-look: Components are scattered across large unused areas instead of being arranged into clear functional groups.
- `campaign-stm32-buck`: pcb human-look: Reference designators and footprint outlines are inconsistently oriented and crowded around several parts.

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: move_symbols refused the multi-symbol move with a vague loose-end error and did not identify which connection caused the refusal.
- `sch-create-small`: struggled: turn 1: render_schematic reported one visual finding without describing its location or nature.
- `sch-create-small`: struggled: turn 1: get_symbol returned no visible symbol properties or pin coordinates, making connectivity and orientation verification difficult.
- `sch-create-small`: struggled: turn 1: get_net returned generic autogenerated LED net names instead of the requested semantic LED_A and LED_K names.
- `sch-create-small`: wished: turn 1: move_symbols should provide the exact offending pin, wire, or dangling segment and support atomic connected-group moves.
- `sch-create-small`: wished: turn 1: render_schematic should include structured visual-finding descriptions with coordinates and suggested fixes.
- `sch-create-small`: wished: turn 1: get_symbol should return reference, value, orientation, pin names, pin numbers, and absolute pin locations.
- `sch-create-small`: wished: turn 1: A net-inspection tool should list every net with connected pins, labels, and aliases in one concise response.
- `sch-create-small`: wished: turn 1: The final verification should explicitly confirm component values, LED color, transistor type, and connector pin assignments.
- `sch-create-medium`: struggled: turn 1: render_schematic reported one visual finding without identifying its location or severity.
- `sch-create-medium`: struggled: turn 1: place_parts reported nets=0 during initial placement, making connectivity status unclear until the realise phase.
- `sch-create-medium`: struggled: turn 1: check_schematic provided aggregate zero findings but no detailed pin-to-net connectivity report.
- `sch-create-medium`: struggled: turn 1: search_symbols returned hit counts but not enough symbol pin and electrical-type metadata to validate wiring before placement.
- `sch-create-medium`: wished: turn 1: render_schematic should return annotated visual findings with coordinates, screenshots, and suggested fixes.
- `sch-create-medium`: wished: turn 1: check_schematic should expose a complete netlist including every component pin, net name, and connection.
- `sch-create-medium`: wished: turn 1: place_parts should report intended connectivity and net creation while placing parts rather than only after realisation.
- `sch-create-medium`: wished: turn 1: search_symbols should return pin maps, pin types, aliases, and recommended footprints in the search result.
- `sch-create-large`: struggled: turn 1: check_schematic reported 16 findings and ok:false even though ERC had zero errors, making electrical cleanliness versus completeness status confusing.
- `sch-create-large`: struggled: turn 1: arrange moved the LED and pull-up circuitry but warned that it could not preserve the netlist cleanly, forcing manual reconnection and relabeling.
- `sch-create-large`: struggled: turn 1: delete_wires removed an entire named net and unexpectedly left four pins loose, making net-level editing unnecessarily destructive.
- `sch-create-large`: struggled: turn 1: The toolset lacked a safe atomic operation for adding standard support circuits such as USB-C protection, decoupling, bulk capacitors, and I2C pull-ups.
- `sch-create-large`: struggled: turn 1: Pin/net inspection was insufficiently direct, so symbol queries were needed to determine the correct MCU pins for reconnecting the I2C and LED networks.
- `sch-create-large`: struggled: turn 1: The 270-second turn limit was consumed by many small schematic edits, leaving no time for a final render verification or board phase.
- `sch-create-large`: wished: turn 1: Provide reusable circuit templates that atomically place, wire, and label USB-C power protection, buck regulators, decoupling, reset, crystal, and I2C support circuits.
- `sch-create-large`: wished: turn 1: Make arrange preserve existing wire geometry and connectivity, or offer a preview and rollback when routing preservation fails.
- `sch-create-large`: wished: turn 1: Add non-destructive component/net editing that deletes only selected segments rather than every wire on a named net.
- `sch-create-large`: wished: turn 1: Expose a concise connectivity report showing every pin, net name, and endpoint before and after each edit.
- `sch-create-large`: wished: turn 1: Separate ERC status from completeness status and provide actionable one-click fixes for supported completeness findings.
- `sch-create-large`: wished: turn 1: Support larger batched edit requests and automatic final schematic rendering and verification before the turn timeout.
- `campaign-stm32-buck`: struggled: turn 1: The board placement and routing tools left 118 unconnected copper items even though the schematic ERC passed with zero errors, making it difficult to distinguish connectivity, routing, and pad-assignment problems.
- `campaign-stm32-buck`: struggled: turn 1: The staged-footprint warnings for J2, J4, R13, and R14 referenced an unavailable Gordian footprint library without identifying the intended compatible replacement.
- `campaign-stm32-buck`: struggled: turn 1: The DRC output was extremely large and truncated, so the complete set of blockers and their relationships could not be inspected efficiently.
- `campaign-stm32-buck`: struggled: turn 1: The routing tools did not provide a reliable high-level way to route all ordinary nets while preserving a compact switch-node loop and sensitive crystal/USB layout constraints.
- `campaign-stm32-buck`: struggled: turn 1: The board-check report showed routed 0/41 but did not clearly explain whether tracks were absent, invalid, or merely disconnected from pads.
- `campaign-stm32-buck`: struggled: turn 1: The large number of generated placement and connectivity islands made manual recovery impractical within the per-turn time limit.
- `campaign-stm32-buck`: wished: turn 1: Provide a batch operation that assigns standard compatible footprints to all staged components and synchronizes the board automatically.
- `campaign-stm32-buck`: wished: turn 1: Add a deterministic net connectivity repair or ratsnest-to-track routing operation that connects every same-net pad and reports failures per net.
- `campaign-stm32-buck`: wished: turn 1: Return structured, non-truncated DRC results with grouping by root cause, net, and component, plus direct repair suggestions.
- `campaign-stm32-buck`: wished: turn 1: Add placement constraints or regions for power, USB, crystal, and logic areas so automated placement and routing respect electrical layout requirements.
- `campaign-stm32-buck`: wished: turn 1: Expose a board-level connectivity graph showing which pads, tracks, vias, and zones are electrically joined before running DRC.
- `campaign-stm32-buck`: wished: turn 1: Support a fast finalization pipeline that routes, pours zones, runs ERC/DRC, renders both views, and exports Gerbers, drill, pick-and-place, and BOM files with a single validated command.

## [variance]

- `sch-create-small`: provider latency: #1=8800ms, #2=4900ms, #3=5300ms, #4=8600ms, #5=5600ms, #6=2100ms, #7=4800ms, #8=2600ms, #9=7300ms, #10=2500ms, #11=5300ms, #12=10900ms, #13=2400ms, #14=3400ms, #15=6500ms, #16=7000ms, #17=2600ms, #18=2700ms, #19=9700ms, #20=2700ms, #21=7600ms, #22=1900ms, #23=4700ms, #24=5100ms, #25=3500ms, #26=2500ms, #27=8300ms, #28=2200ms, #29=2300ms, #30=3700ms
- `sch-create-medium`: cost: 103.3s elapsed, 47.4s agent, 8 provider requests
- `sch-create-medium`: provider latency: #1=7100ms, #2=3600ms, #3=5500ms, #4=10700ms, #5=2900ms, #6=8200ms, #7=2200ms, #8=2900ms
- `sch-create-large`: provider latency: #1=6800ms, #2=4300ms, #3=3500ms, #4=3400ms, #5=2100ms, #6=3400ms, #7=2400ms, #8=9700ms, #9=3000ms, #10=2300ms, #11=7400ms, #12=7000ms, #13=4000ms, #14=3000ms, #15=3200ms, #16=4200ms, #17=5700ms, #18=7800ms, #19=7100ms, #20=4500ms, #21=5800ms, #22=5700ms, #23=3900ms, #24=3200ms, #25=2500ms, #26=4700ms, #27=2100ms, #28=4100ms, #29=7000ms, #30=4300ms, #31=7700ms, #32=4200ms, #33=3300ms, #34=2600ms, #35=2900ms, #36=3000ms, #37=2500ms, #38=3700ms, #39=3900ms, #40=5700ms, #41=3700ms, #42=9300ms, #43=3300ms, #44=3300ms, #45=8300ms, #46=7800ms, #47=4400ms, #48=5900ms, #49=3600ms, #50=2900ms, #51=2500ms
- `campaign-stm32-buck`: provider latency: #1=16300ms, #2=13700ms, #3=2900ms, #4=8200ms, #5=3600ms, #6=2800ms, #7=3100ms, #8=3300ms, #9=3100ms, #10=4200ms, #11=24600ms, #12=15900ms, #13=22200ms, #14=17100ms, #15=3100ms, #16=6200ms, #17=4400ms, #18=4100ms, #19=4500ms, #20=2500ms, #21=2100ms, #22=3000ms, #23=2200ms, #24=2000ms, #25=6300ms, #26=3300ms, #27=3900ms, #28=8900ms, #29=6500ms, #30=4500ms, #31=4200ms
