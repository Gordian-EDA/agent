# Quality findings: prompt-blue-pill-prompt-hbridge-prompt-sallen-key-gain-dataset-three-phase-0cdac5a0

Generated: 20260904T034910Z
Run output: /tmp/claude-1000/-home-mimi-agent/859b05e2-ce70-455d-87fa-c4a8f253c33c/scratchpad/shape-runs

Questions:
- (none provided)

## [tool-contract]

- `prompt-blue-pill`: tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.user_io`: a layout node is exactly one of `part`, `row` or `col`"}
- `prompt-blue-pill`: tool `connect` refusal: {"connected":[{"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","from":"U2.3","net_delta":{"merged":[[["Net-(U2-PD1)","OSC32_IN"],"OSC32_IN"]]},"to":"Y2.1"},{"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","from":"U2.4","net_delta":{"merged":[[["Net-(U2-PD1)","OSC32_OUT"],"OSC32_OUT"]]},"to":"Y2.2"},{"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","from":"U2.2","net_delta":{"merged":[[["Net-(U2-PD1)","PC13"],"PC13"]]},"to":"R8.1"},{"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","from":"U2.20","net_delta":{"merged":[[["Net-(U2-PD1)","PB2"],"PB2"]]},"to":"J7.1"}],"error":"all 4 connections failed — U2.3 -> Y2.1: refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written; U2.4 -> Y2.2: refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written; U2.2 -> R8.1: refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written; U2.20 -> J7.1: refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written"}
- `prompt-blue-pill`: tool `connect` refusal: {"connected":[{"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","from":"U2.2","net_delta":{"merged":[[["Net-(U2-PD1)","PC13"],"PC13"]]},"to":"R8.1"},{"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","from":"U2.20","net_delta":{"merged":[[["Net-(U2-PD1)","PB2"],"PB2"]]},"to":"J7.1"}],"error":"all 2 connections failed — U2.2 -> R8.1: refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written; U2.20 -> J7.1: refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written"}
- `prompt-blue-pill`: tool `label` refusal: {"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","net_delta":{"merged":[[["Net-(U2-PD1)","PC13"],"PC13"]]}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"refused: the edit would change connectivity the call did not name (Net-(U2-PD1)); nothing was written","net_delta":{"merged":[[["Net-(U2-PD1)","PB2"],"PB2"]]}}
- `prompt-blue-pill`: tool `arrange` refusal: {"error":"invalid arrange input at `layout`: a layout node is exactly one of `part`, `row` or `col`"}
- `prompt-blue-pill`: tool `arrange` refusal: {"error":"invalid arrange input at `intent.rails.VBUS_FUSED`: unknown variant `left`, expected `top` or `bottom`"}
- `prompt-blue-pill`: tool `move_symbols` refusal: {"error":"nothing within 25 mm of (219.71,124.46) has room for C5; nothing was moved"}
- `prompt-blue-pill`: tool `move_symbols` refusal: {"error":"refused: dragging U2, J2, J3, J4, C11, C12, C9, C10 would change nets BOOT0, NRST, OSC32_IN, OSC32_OUT, PA15, PB0, PB1, PB10; try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `prompt-blue-pill`: tool `move_symbols` refusal: {"error":"nothing within 25 mm of (209.55,105.41) has room for R2; nothing was moved"}
- `prompt-blue-pill`: tool `move_symbols` refusal: {"error":"refused: dragging J1, F1, U1, C1, C2, C3, C4, TVS1, TVS2, D1, D2 was refused (drag would leave 3 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `prompt-blue-pill`: tool `move_symbols` refusal: {"error":"refused: dragging TVS1, TVS2, D1, D2 was refused (drag would leave 1 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `prompt-blue-pill`: tool `arrange` refusal: {"error":"invalid arrange input at `layout.row[2]`: invalid type: string \"refs\", expected struct Wire"}
- `prompt-hbridge`: tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"decouple_unresolved":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["layout-unknown-part: `layout.hbridge` places `RIN_A`, which is not a part of that region — region `hbridge` holds QPA, QPB, QNA, QNB, QDRV_A, QDRV_B, RPA_G, RPB_G, RNA_G, RNB_G, RPA_PU, RPB_PU","layout-unknown-part: `layout.hbridge` places `RIN_B`, which is not a part of that region — region `hbridge` holds QPA, QPB, QNA, QNB, QDRV_A, QDRV_B, RPA_G, RPB_G, RNA_G, RNB_G, RPA_PU, RPB_PU"],"nc_overridden":[],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; occupied references are repaired in `renamed`, while `duplicate_refs` identify one ref used for incompatible declarations; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `unplaced` parts could not be resolved at all and were left out. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `prompt-hbridge`: tool `arrange` refusal: {"error":"invalid arrange input at `intent.GND`: unknown field `GND`, expected `rails` or `ports`"}
- `prompt-hbridge`: tool `move_symbols` refusal: {"error":"refused: dragging QPA, QNA, QDRV_A, RPA_G, RNA_G, RPA_PU, RNA_PD, RBA, D1, D2, QPB, QNB, QDRV_B, RPB_G, RNB_G, RPB_PU, RNB_PD, RBB, D3, D4 was refused (drag would leave 5 loose ends behind); try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `prompt-hbridge`: tool `arrange` refusal: {"error":"invalid arrange input at `layout.row[1]`: a layout node is exactly one of `part`, `row` or `col`"}
- `prompt-sallen-key-gain`: tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["duplicate-ref: `U1` assigns conflicting `props` values (`Amplifier_Operational:LM358` and `Amplifier_Operational:LM358`); give each physical part a distinct reference"],"ok":false,"unknown_pins":[],"unplaced":[],"unreliable_nets":[]}
- `prompt-sallen-key-gain`: tool `arrange` refusal: {"error":"invalid arrange input at `layout.filter_gain`: unknown field `filter_gain`, expected one of `part`, `unit`, `rot`, `mirror`, `row`, `col`, `gap`, `align`, `wrap`"}
- `prompt-sallen-key-gain`: tool `add_power` refusal: {"error":"refused: the edit would change connectivity the call did not name (AUDIO_OUT); nothing was written","net_delta":{"merged":[[["+9V","AUDIO_OUT"],"AUDIO_OUT"]],"now_connected":["#FLG1.1"]}}
- `prompt-sallen-key-gain`: tool `arrange` refusal: {"error":"invalid arrange input at `layout`: a layout node is exactly one of `part`, `row` or `col`"}
- `prompt-sallen-key-gain`: tool `move_symbols` refusal: {"error":"refused: dragging BT1, R1, R2, C1, C2, C3, J1, R3, R4, C4, C5, U1, U1, U1, R5, R6, U2, U2, U2, J2, C6, C7 would change nets +9V, AUDIO_OUT; try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}
- `prompt-sallen-key-gain`: tool `swap_symbol` refusal: {"error":"refused: the replacement pins could not be re-seated cleanly: drag would leave 2 loose ends behind; nothing was written","suggestion":{"new_symbol_unassigned_pins":[],"old_pins_without_counterpart":[],"pin_map":{}}}

## [prompt]

- `prompt-blue-pill`: loop smell: tool `get_symbol` called 5 times in a row
- `prompt-blue-pill`: loop smell: tool `search_footprints` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `connect` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `get_net` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `get_net` called 4 times in a row
- `prompt-blue-pill`: loop smell: tool `arrange` called 10 times in a row
- `prompt-blue-pill`: loop smell: tool `get_symbol` called 7 times in a row
- `prompt-blue-pill`: loop smell: tool `move_symbols` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `move_symbols` called 5 times in a row
- `prompt-blue-pill`: loop smell: tool `arrange` called 3 times in a row
- `prompt-blue-pill`: cost: 775.5s elapsed, 648.9s agent, 88 provider requests
- `prompt-hbridge`: loop smell: tool `get_symbol` called 6 times in a row
- `prompt-hbridge`: cost: 515.4s elapsed, 336.1s agent, 32 provider requests
- `prompt-sallen-key-gain`: loop smell: tool `search_footprints` called 3 times in a row
- `prompt-sallen-key-gain`: loop smell: tool `arrange` called 3 times in a row
- `prompt-sallen-key-gain`: cost: 749.7s elapsed, 577.8s agent, 99 provider requests
- `dataset-three-phase-0cdac5a0`: loop smell: tool `check_schematic` called 3 times in a row
- `dataset-three-phase-0cdac5a0`: loop smell: tool `add_power` called 4 times in a row
- `dataset-three-phase-0cdac5a0`: cost: 237.5s elapsed, 75.1s agent, 11 provider requests

## [engine]

- `prompt-blue-pill`: failed check: schematic_critic_score >= 8 — actual 5
- `prompt-blue-pill`: judge: HSE load network is miswired: C5 and C6 are both on HSE_IN, while HSE_OUT uses C11; connect one load capacitor to each crystal pin and verify capacitor values.
- `prompt-blue-pill`: judge: BOOT1/PB2 jumper J7 is only a two-pin PB2-to-+3V3 connection, so it does not provide a proper selectable BOOT1 high/low configuration.
- `prompt-blue-pill`: judge: The USB D+ series network is confusing and incorrectly partitioned: R7.2 is on an isolated J6/J8 net while U2 USB_DP is directly on USB_DP; remove the unused test-net circuitry or wire the resistor as an actual series element.
- `prompt-blue-pill`: judge: J3 pin 5 is explicitly left unconnected without a clear no-connect purpose or annotation, which is poor practice on a GPIO/header connector.
- `prompt-blue-pill`: judge: The rendered schematic remains excessively spread out, with support components separated from the circuits they serve; repack the design into compact power, MCU, clock/reset, USB, and header blocks.
- `prompt-blue-pill`: judge: Final rendering still reports visual defects, including a D1/label overlap and a wire crossing the J1 connector body; correct these before release.
- `prompt-blue-pill`: schematic critic: major/spacing/Entire sheet; MCU, power column, headers, and explanatory-note regions: The functional blocks are spread across most of the page with very large empty gaps, including a long isolated vertical power chain and headers positioned far below the MCU.
- `prompt-blue-pill`: schematic critic: major/orientation/Central vertical regulator/power-support chain: The power path and associated series parts are arranged as an unusually tall vertical sequence instead of a compact horizontal power-flow block with aligned vertical decoupling taps.
- `prompt-blue-pill`: schematic critic: minor/spacing/Decoupling, crystal, BOOT/reset, LED, and small support-component groups: Related small components are separated into several isolated stacks rather than being grouped and aligned around the MCU or their respective power/function blocks.
- `prompt-hbridge`: failed check: schematic_critic_score >= 8 — actual 6
- `prompt-hbridge`: judge: Resolve severe text collisions around QDRV_A/QDRV_B, MOSFETs, gate resistors, and net labels.
- `prompt-hbridge`: judge: Replace autogenerated net labels such as `Net-(QNA-G)` and `Net-(RPA_G-Pad2)` with short functional labels or reposition them for readability.
- `prompt-hbridge`: judge: Reorganize each half-bridge into a consistent vertical power path (+12V, high-side MOSFET, motor node, low-side MOSFET, GND) with driver circuitry adjacent.
- `prompt-hbridge`: judge: Move the bulk capacitor and motor connector closer to the H-bridge power and motor nodes, reducing the large unused page area.
- `prompt-hbridge`: judge: Keep repeated A/B channel annotations horizontal and aligned; avoid vertically stacked labels overlapping component text.
- `prompt-hbridge`: schematic critic: major/text-overlap/Left bridge gate-driver region around QPA, QNA, QDRV_A, and RGA: Multiple rotated net and component labels overlap gate wires, transistor artwork, or adjacent labels, particularly around the QPA/QNA gate connections and the RGA/pull-down network.
- `prompt-hbridge`: schematic critic: major/text-overlap/Right bridge gate-driver region around QPB, QNB, QDRV_B, and RGB: The mirrored driver section repeats the same crowded label placement, with rotated net/refdes text abutting or crossing wires and symbol artwork.
- `prompt-hbridge`: schematic critic: major/text-overlap/Lower-left and lower-right gate pull-up networks: The pull-up resistor reference/value and vertical net labels are stacked over or immediately against the resistor symbols and their vertical wires, making the networks difficult to parse.
- `prompt-hbridge`: schematic critic: major/spacing/Both gate-driver networks relative to their associated MOSFETs: The gate resistors, pull-downs, and pull-ups are distributed well below or beside the MOSFET/driver pairs, producing long control-path runs and leaving a large unused lower-center area.
- `prompt-sallen-key-gain`: failed check: schematic_critic_score >= 8 — actual 6
- `prompt-sallen-key-gain`: judge: The Sallen-Key filter topology is incorrect: R3, R4, C4, and U1 pin 3 are merged onto N1, collapsing the intended two filter nodes and effectively producing a degenerate/first-order response rather than a second-order low-pass.
- `prompt-sallen-key-gain`: judge: Rewire the filter with separate nodes: R3 from AUDIO_IN to the first RC node, R4 from that node to the op-amp output, C4 from the first node to the non-inverting input, and C5 from the non-inverting input to VGND.
- `prompt-sallen-key-gain`: judge: The schematic has excessive whitespace and scattered detached power symbols/labels; compact the power and audio sections and remove orphan-looking markers.
- `prompt-sallen-key-gain`: judge: Resolve visible label/component overlaps and improve left-to-right signal-flow alignment, especially around R4/R5, FILTER_OUT, AUDIO_OUT, and GAIN_FB.
- `prompt-sallen-key-gain`: judge: Correct the explanatory note/component references so they identify C4/C5 as the filter capacitors and accurately describe the implemented values.
- `prompt-sallen-key-gain`: schematic critic: major/spacing/Overall sheet composition between the left supply block and right signal block: The two functional blocks are placed at opposite extremes of a very wide canvas, leaving a large unused central region and making the complete signal/power arrangement difficult to read at a glance.
- `prompt-sallen-key-gain`: schematic critic: minor/spacing/Central and lower blank regions outside the dashed circuit blocks: Several standalone +9 V and GND power symbols are scattered far from the associated circuitry, reading as abandoned or misplaced symbols rather than organized power connections.
- `dataset-three-phase-0cdac5a0`: failed check: critic_vs_reference >= 8 — actual 7
- `dataset-three-phase-0cdac5a0`: judge: Remove the four added PWR_FLAG symbols; the request explicitly prohibits adding parts, and they are not in the supplied netlist.
- `dataset-three-phase-0cdac5a0`: judge: Re-run ERC after removing the PWR_FLAGs and document the resulting undriven-input findings rather than modifying the circuit to suppress them.
- `dataset-three-phase-0cdac5a0`: judge: Tighten the oversized sheet layout and relocate the detached bypass-capacitor bank closer to its associated driver channels.
- `dataset-three-phase-0cdac5a0`: judge: Add concise channel identifiers and improve consistency of reference/value placement for faster schematic reading.
- `dataset-three-phase-0cdac5a0`: schematic critic: major/spacing/C17, C19, C21, C23, C25, C26, C29, C30 and the lower-right region: The 100 nF bypass capacitors are grouped remotely at the lower right instead of being placed beside their associated FAN7371 channels, creating unnecessary visual separation and weak channel grouping.
- `dataset-three-phase-0cdac5a0`: schematic critic: minor/other/Intermediate nets N$1 through N$24 along the eight channel lanes: Numerous exposed N$ labels clutter the otherwise straight internal channel wiring and make the repeated lanes harder to scan.

## [judge]

- `prompt-blue-pill`: schematic human-look: Excessive unused canvas makes the schematic feel fragmented and forces very long visual jumps between related blocks.
- `prompt-blue-pill`: schematic human-look: Decoupling, protection, and support parts are scattered into tall vertical chains instead of compact, clearly grouped sections.
- `prompt-blue-pill`: schematic human-look: Many labels and notes are too small or distant from their symbols at the overall-sheet view, weakening scanability and hierarchy.
- `prompt-hbridge`: schematic human-look: Resolve severe text collisions around QDR driver, MOSFET, and gate-net labels.
- `prompt-hbridge`: schematic human-look: Re-align each repeated channel into consistent, clearly bounded functional blocks.
- `prompt-hbridge`: schematic human-look: Reduce excessive empty space and replace scattered vertical annotations with readable horizontal labels.
- `prompt-sallen-key-gain`: schematic human-look: Remove the detached power symbols and labels scattered through the empty center and lower canvas.
- `prompt-sallen-key-gain`: schematic human-look: Reposition and tighten the two functional blocks so the schematic uses the page efficiently instead of leaving large unused gaps.
- `prompt-sallen-key-gain`: schematic human-look: Align component rows, annotations, and explanatory notes to a consistent grid with clearer hierarchy and spacing.
- `dataset-three-phase-0cdac5a0`: schematic human-look: Large unused vertical and right-side whitespace makes the repeated channel field feel stretched rather than tightly composed.
- `dataset-three-phase-0cdac5a0`: schematic human-look: The lower/right capacitor groups are visually detached from the channel rows they document; place them nearer their associated sections or enclose them as explicit groups.
- `dataset-three-phase-0cdac5a0`: schematic human-look: The left-side vertical rail and repeated +12V/GND markings create a visually busy spine; simplify the presentation with clearer section breaks and fewer repeated labels.

## [self-diagnosis]

- `prompt-blue-pill`: struggled: The arrange tool produced an excessively spread-out schematic and introduced label overlaps and a wire crossing despite reporting connectivity unchanged.
- `prompt-blue-pill`: struggled: The arrange tool's layout schema was unclear; a malformed request was refused because the trailing `refs` field was parsed as a wire.
- `prompt-blue-pill`: struggled: The render_schematic tool reported visual findings without describing which six defects were present.
- `prompt-blue-pill`: struggled: The check_schematic tool reported zero findings even while rendering and independent review identified visible overlaps and crossings.
- `prompt-blue-pill`: struggled: The review_schematic score of 6/10 identified poor compactness but provided insufficient actionable defect locations or suggested placements.
- `prompt-blue-pill`: struggled: The read_schematic compact output did not provide enough geometry or connectivity context to repair the visual issues efficiently.
- `prompt-blue-pill`: wished: Add a structured visual-findings report with severity, component references, coordinates, and recommended fixes.
- `prompt-blue-pill`: wished: Make arrange preserve compact block placement and explicitly flag when its routing causes symbol, label, or wire-body overlaps.
- `prompt-blue-pill`: wished: Provide a documented layout schema or validation errors that identify the exact malformed field and expected structure.
- `prompt-blue-pill`: wished: Have check_schematic include graphical clearance, overlap, crossing, and readability checks alongside ERC.
- `prompt-blue-pill`: wished: Allow targeted move or compact operations for a named functional block without redrawing unrelated wiring.
- `prompt-blue-pill`: wished: Expose a rendered schematic preview or crop regions directly in tool results for faster visual diagnosis.
- `prompt-hbridge`: struggled: `review_schematic` reported visual defects and scores but did not provide actionable descriptions of the seven defects.
- `prompt-hbridge`: struggled: `arrange` rejected the layout containing gap nodes with an opaque schema error, requiring trial and error to discover valid nesting.
- `prompt-hbridge`: struggled: `move_symbols` refused the grouped move because it would leave loose ends, without identifying which connections or move strategy would resolve them.
- `prompt-hbridge`: struggled: `arrange` preserved connectivity while introducing numerous label and symbol-overlap warnings, and offered no precise collision-avoidance controls.
- `prompt-hbridge`: struggled: `check_schematic` reported zero ERC findings even though the visual review found severe annotation overlaps, making electrical and presentation quality difficult to reconcile.
- `prompt-hbridge`: wished: Return structured defect locations and descriptions from `review_schematic`, ideally with component references and suggested fixes.
- `prompt-hbridge`: wished: Provide a documented or introspectable `arrange` layout schema, including supported spacing and gap syntax.
- `prompt-hbridge`: wished: Allow constrained group moves that automatically reroute or preserve attached wires, or explain the exact loose ends blocking a move.
- `prompt-hbridge`: wished: Add tools for moving labels, pin text, and fields independently from symbols to correct visual overlaps.
- `prompt-hbridge`: wished: Include a final render preview or link in the user-facing completion response, alongside ERC and visual-review results.
- `prompt-sallen-key-gain`: struggled: The ERC checker reported zero findings after the LM358 stage was removed, but did not detect that the requested gain stage was missing.
- `prompt-sallen-key-gain`: struggled: The render tool reported four visual findings without identifying their locations or causes.
- `prompt-sallen-key-gain`: struggled: The swap_symbol tool refused to replace U1 because of loose wire ends, but provided no actionable pin or wire mapping.
- `prompt-sallen-key-gain`: struggled: Repeated check_schematic calls returned mostly identical diagnostics and consumed many requests without improving the design.
- `prompt-sallen-key-gain`: struggled: The toolset did not provide a reliable way to verify that the Sallen-Key topology and component connectivity matched the intended circuit electrically.
- `prompt-sallen-key-gain`: wished: Add completeness checks that validate required functional blocks, op-amp sections, and stated gain/filter specifications rather than only ERC connectivity.
- `prompt-sallen-key-gain`: wished: Return annotated visual-render findings with coordinates, affected references, and suggested fixes.
- `prompt-sallen-key-gain`: wished: Support transactional symbol replacement with automatic wire remapping or explicit repair instructions.
- `prompt-sallen-key-gain`: wished: Provide a netlist/topology inspection tool showing each component, pin, and net connection.
- `prompt-sallen-key-gain`: wished: Add circuit-level validation for cutoff frequency, filter response, and non-inverting gain.
- `prompt-sallen-key-gain`: wished: Expose a concise schematic summary or image inspection result so the final rendered design can be reviewed confidently.
- `dataset-three-phase-0cdac5a0`: struggled: check_schematic reported pin_not_driven ERC errors even though the netlist explicitly required those input pins and no driver parts were provided.
- `dataset-three-phase-0cdac5a0`: struggled: add_power silently added four PWR_FLAG parts, violating the instruction not to add parts.
- `dataset-three-phase-0cdac5a0`: struggled: The toolset offered no way to mark expected undriven input nets as intentional without modifying the circuit.
- `dataset-three-phase-0cdac5a0`: struggled: The initial completion claim said all 40 parts were present, but later repair actions changed the part count.
- `dataset-three-phase-0cdac5a0`: wished: Provide an ERC-exclusion or acknowledge-undriven-net tool that preserves the exact netlist and part count.
- `dataset-three-phase-0cdac5a0`: wished: Make add_power require explicit confirmation when it introduces a new schematic part.
- `dataset-three-phase-0cdac5a0`: wished: Provide a netlist-diff check reporting added, removed, or altered parts and pins before completion.
- `dataset-three-phase-0cdac5a0`: wished: Expose render inspection or an image summary so visual correctness can be verified rather than inferred.

## [variance]

- `prompt-blue-pill`: provider latency: #1=4000ms, #2=1600ms, #3=4700ms, #4=8500ms, #5=3300ms, #6=5000ms, #7=4000ms, #8=13600ms, #9=8600ms, #10=5000ms, #11=6400ms, #12=3400ms, #13=6100ms, #14=6700ms, #15=4900ms, #16=9500ms, #17=4200ms, #18=4000ms, #19=3000ms, #20=4100ms, #21=2700ms, #22=3200ms, #23=2000ms, #24=2800ms, #25=2900ms, #26=3000ms, #27=2600ms, #28=2200ms, #29=3800ms, #30=5000ms, #31=2700ms, #32=3100ms, #33=5700ms, #34=3300ms, #35=2900ms, #36=2800ms, #37=3600ms, #38=2400ms, #39=7700ms, #40=7800ms, #41=6400ms, #42=2600ms, #43=4300ms, #44=4900ms, #45=2400ms, #46=3900ms, #47=50600ms, #48=44200ms, #49=61600ms, #50=4800ms, #51=4100ms, #52=3700ms, #53=3100ms, #54=4600ms, #55=2800ms, #56=3900ms, #57=2700ms, #58=3100ms, #59=3300ms, #60=2800ms, #61=3300ms, #62=29600ms, #63=33900ms, #64=30900ms, #65=5200ms, #66=9600ms, #67=5100ms, #68=2500ms, #69=4400ms, #70=2900ms, #71=39200ms, #72=39300ms, #73=41000ms, #74=6200ms, #75=7400ms, #76=2700ms, #77=3200ms, #78=6000ms, #79=2300ms, #80=7100ms, #81=28100ms, #82=38500ms, #83=36600ms, #84=16500ms, #85=7500ms, #86=5700ms, #87=4100ms, #88=8700ms
- `prompt-hbridge`: provider latency: #1=4000ms, #2=1500ms, #3=3500ms, #4=19700ms, #5=15100ms, #6=2700ms, #7=5500ms, #8=46700ms, #9=41700ms, #10=50200ms, #11=9100ms, #12=4000ms, #13=2600ms, #14=43800ms, #15=43000ms, #16=41900ms, #17=10300ms, #18=2700ms, #19=43000ms, #20=46900ms, #21=44500ms, #22=9700ms, #23=9400ms, #24=6300ms, #25=3600ms, #26=3700ms, #27=41600ms, #28=44500ms, #29=43600ms, #30=7800ms, #31=2600ms, #32=3400ms
- `prompt-sallen-key-gain`: provider latency: #1=4200ms, #2=2400ms, #3=3200ms, #4=9200ms, #5=12100ms, #6=1600ms, #7=3300ms, #8=8200ms, #9=11900ms, #10=9400ms, #11=2400ms, #12=1900ms, #13=1700ms, #14=4100ms, #15=4000ms, #16=3000ms, #17=3200ms, #18=2600ms, #19=5700ms, #20=5300ms, #21=3000ms, #22=2900ms, #23=3700ms, #24=2500ms, #25=3800ms, #26=2100ms, #27=2800ms, #28=2700ms, #29=3500ms, #30=3800ms, #31=2000ms, #32=2100ms, #33=2300ms, #34=2100ms, #35=1900ms, #36=2300ms, #37=2100ms, #38=2700ms, #39=3900ms, #40=51400ms, #41=37500ms, #42=40300ms, #43=8600ms, #44=3600ms, #45=4000ms, #46=2200ms, #47=8100ms, #48=3800ms, #49=2000ms, #50=10800ms, #51=4400ms, #52=40000ms, #53=42800ms, #54=40800ms, #55=6800ms, #56=5400ms, #57=2100ms, #58=2400ms, #59=8600ms, #60=3000ms, #61=8900ms, #62=10400ms, #63=2200ms, #64=3200ms, #65=3200ms, #66=3500ms, #67=2300ms, #68=5900ms, #69=4700ms, #70=3500ms, #71=3500ms, #72=2000ms, #73=2400ms, #74=7400ms, #75=2800ms, #76=6700ms, #77=2900ms, #78=3200ms, #79=2400ms, #80=2300ms, #81=2100ms, #82=3100ms, #83=2800ms, #84=2800ms, #85=2200ms, #86=6700ms, #87=2000ms, #88=5900ms, #89=2800ms, #90=7700ms, #91=9600ms, #92=5900ms, #93=2500ms, #94=2200ms, #95=9600ms, #96=3000ms, #97=2100ms, #98=2400ms, #99=7000ms
- `dataset-three-phase-0cdac5a0`: provider latency: #1=3600ms, #2=1800ms, #3=14800ms, #4=3400ms, #5=7800ms, #6=3800ms, #7=8200ms, #8=2200ms, #9=5600ms, #10=2300ms, #11=3400ms
