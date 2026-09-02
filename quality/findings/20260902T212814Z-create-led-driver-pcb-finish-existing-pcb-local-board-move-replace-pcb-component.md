# Quality findings: create-led-driver-pcb-finish-existing-pcb-local-board-move-replace-pcb-component

Generated: 20260902T212814Z
Run output: /home/mimi/agent/.claude/worktrees/w1/quality/runs/w1

Questions:
- can the agent now build a board incrementally with partial states

## [tool-contract]

- `create-led-driver-pcb`: turn 1 tool `search_footprints` refusal: {"error":"missing required string field `symbol`"}
- `create-led-driver-pcb`: turn 1 tool `search_footprints` refusal: {"error":"duplicate discovery call deferred","note":"reuse the coalesced results returned by the first call"}
- `create-led-driver-pcb`: turn 1 tool `place_parts` refusal: {"code":"invalid_payload","dangling":[],"did_you_mean":{},"duplicate_refs":[],"footprint_mismatch":[],"input_errors":["decouple-ambiguous: C1: decouple no power_in pins; fallback looked for VDD*/VCC* and VSS*/GND* pin names; needs supply and ground candidates (found [] / []) — write the caps explicitly"],"note":"this lists EVERY fault in the payload — fix them all before retrying. `place_parts` appends to the sheet, so resubmit only the parts named here, not the whole payload. `input_errors` are unresolvable lib_ids and pin conflicts; `duplicate_refs` give the next free refdes; `unknown_pins` name a key the symbol does not have; `footprint_mismatch` includes the closest same-library pad-set repair. `dangling` pins are NOT fatal on their own — they are listed so you can finish them.","ok":false,"unknown_pins":[],"unreliable_nets":[]}
- `local-board-move`: turn 1 tool `move_parts` refusal: {"error":"moves[0]: missing required string `reference`"}
- `local-board-move`: turn 1 tool `move_parts` refusal: {"blocked_by":{"courtyard_mm":[0.32000000000000006,4.55,3.68,6.45],"reference":"R2"},"code":"courtyards_overlap","error":"move_parts refused: R1 at [2.000, 4.000] would leave -0.600 mm to R2 — their courtyards need 0.200 mm between them","gap_mm":-0.5999999999999996,"moved":{"courtyard_mm":[0.31999999999999984,3.05,3.68,4.95],"reference":"R1"},"required_clearance_mm":0.2}
- `replace-pcb-component`: turn 1 tool `set_fields` refusal: {"error":"set_fields does not set Footprint; use assign_footprints so symbol compatibility is validated"}
- `replace-pcb-component`: turn 1 tool `sync_board` refusal: {"code":"intent_after_creation","error":"sync_board takes `intent` only when it creates the board. On an existing board pass the layout half to place_board({intent}) and any zones as rules {\"pours\": [{\"net\": …, \"layer\": …}]}."}

## [engine]

- `create-led-driver-pcb`: failed check: schematic_critic_score >= 8 — actual 6
- `create-led-driver-pcb`: failed check: human_look_schematic_score >= 8 — actual 5
- `create-led-driver-pcb`: failed check: human_look_pcb_score >= 8 — actual 4
- `create-led-driver-pcb`: judge: Remove the redundant same-name local and global CTRL labels to eliminate the remaining ERC warning.
- `create-led-driver-pcb`: judge: Redraw the schematic as compact functional blocks using local power symbols/net labels instead of oversized perimeter buses and long wire detours.
- `create-led-driver-pcb`: judge: Rearrange the PCB so Q1, R1, R2, and R3 form a tight, clearly aligned driver group; the current placement is irregular with excessive empty space.
- `create-led-driver-pcb`: judge: Move PCB reference designators clear of pads, traces, and other labels, with consistent orientation and spacing.
- `create-led-driver-pcb`: judge: Reroute the PCB using shorter, cleaner horizontal/vertical channels rather than the current long diagonal and vertical detours.
- `create-led-driver-pcb`: schematic critic: major/spacing/overall schematic, especially the central gap and far-right R3-D1-Q1 branch: The circuit is spread across a large empty field with unnecessarily long top, bottom, and signal runs; moving the R3-D1-Q1 branch leftward and grouping it nearer R1/R2 would make the drawing substantially more compact.

## [harness]

- `finish-existing-pcb`: schematic human-look unavailable: model returned no object with 'score': '{"score":4,"worst_three":["Zoom out and frame the complete schematic instead of presenting oversized symbols with prominent coordinate axes and rulers.","Hide editor overlays and crop away the blue/red axis arrows and grid lines for a clean deliverable.","Tighten the label placement and component spacing so the resistor references and values read as an intentional grouped block."],"what_a_human_would_change":["Arrange the divider as a compact, centered functional block with consistent vertical spacing.","Use a clean power-to-ground composition and align all labels to a common text baseline.","Export at a useful schematic scale, with surrounding whitespace balanced rather than dominated by the canvas." สุ]}'
- `finish-existing-pcb`: pcb human-look unavailable: KiCad demo PCB render failed: render_board failed: {
  "error": "could not read render geometry from /tmp/gordian-quality-reference-tc4052rf/design.kicad_pcb: Edge.Cuts line segments do not form one closed outline"
}
- `local-board-move`: pcb human-look unavailable: KiCad demo PCB render failed: render_board failed: {
  "error": "could not read render geometry from /tmp/gordian-quality-reference-uttbqu4c/design.kicad_pcb: Edge.Cuts line segments do not form one closed outline"
}
- `replace-pcb-component`: pcb human-look unavailable: KiCad demo PCB render failed: render_board failed: {
  "error": "could not read render geometry from /tmp/gordian-quality-reference-5w06dy85/design.kicad_pcb: Edge.Cuts line segments do not form one closed outline"
}

## [judge]

- `create-led-driver-pcb`: schematic human-look: Replace the oversized perimeter power and ground buses with compact local power symbols and shorter connections.
- `create-led-driver-pcb`: schematic human-look: Re-layout the circuit into a tighter left-to-right flow to remove the large empty center and long wire detours.
- `create-led-driver-pcb`: schematic human-look: Move and align J2, CTRL, and nearby labels so they have consistent spacing and do not crowd adjacent rails.
- `create-led-driver-pcb`: pcb human-look: Silkscreen reference designators visibly overlap pads, traces, and other labels.
- `create-led-driver-pcb`: pcb human-look: Components are clustered irregularly, leaving a large unused central area and poor visual grouping.
- `create-led-driver-pcb`: pcb human-look: Several traces take long, awkward vertical or diagonal paths instead of forming clean, consistent routing channels.
- `finish-existing-pcb`: judge: Repair or verify the Edge.Cuts geometry so it forms one closed outline; the PCB render checker currently rejects the board as open.
- `finish-existing-pcb`: judge: Reposition and separate the R1 and R2 silkscreen references/values so they are legible and do not overlap.
- `finish-existing-pcb`: judge: Export distinct, clean front and back PCB renders without coordinate axes/grid overlays; the current render is not a useful visual deliverable.
- `finish-existing-pcb`: pcb critic: major/board-utilisation/Entire board, especially the right half: The square outline is substantially oversized for the two-resistor population, leaving most of the right side empty; the concrete better alternative is to reduce the outline around the resistor pair, or at minimum center the pair if the outline is constrained.
- `finish-existing-pcb`: pcb critic: minor/silkscreen/R1/R2 reference area: The R1 and R2 references are placed very close together in the narrow gap between the resistor rows; moving each reference farther toward its associated footprint would provide cleaner separation.
- `local-board-move`: schematic critic: minor/spacing/R1-R2 divider region: The gap between R1 and R2 leaves a relatively long interconnect and could be shortened by moving R1 one grid step toward the fixed R2 position.
- `local-board-move`: schematic human-look: Remove the prominent coordinate axes and rulers from the presentation view.
- `local-board-move`: schematic human-look: Reduce symbol and text scale, then compact the vertical spacing so the circuit reads as one cohesive block.
- `local-board-move`: schematic human-look: Reposition VIN, GND, and resistor annotations closer to the circuit with consistent alignment and margins.
- `replace-pcb-component`: judge: Improve the schematic render framing by hiding coordinate axes/ticks and cropping to the circuit; the current view is dominated by overlays and has excessive whitespace.
- `replace-pcb-component`: schematic critic: minor/orientation/R1/R2 divider: Both series resistors are drawn vertically rather than using the stated horizontal series-element convention.
- `replace-pcb-component`: schematic human-look: Visible coordinate axes, tick marks, and large arrows dominate the schematic view.
- `replace-pcb-component`: schematic human-look: Symbols and annotations are excessively oversized and leave the page composition poorly balanced.
- `replace-pcb-component`: schematic human-look: Power symbols and the central wiring are visually cramped, while the reference/value labels sit too far away.
- `replace-pcb-component`: pcb critic: major/board-utilisation/Entire board, especially the right-hand region: The component column occupies only the left portion of the rectangular outline, leaving roughly half the board width unused; tighten the right edge if permitted, or move the resistor column toward the board center if the outline must be preserved.

## [self-diagnosis]

- `create-led-driver-pcb`: struggled: turn 1: The initial place_parts decouple shorthand was refused because the tool could not infer supply and ground candidates, without clearly exposing the required explicit-net schema.
- `create-led-driver-pcb`: struggled: turn 1: The place_board and route_board logs reported parts=0 and nets=0 during guard phases despite successfully processing the board, making progress confusing.
- `create-led-driver-pcb`: struggled: turn 1: check_schematic reported a same_local_global_label warning for CTRL but provided no tool-supported fix or clear indication of which label should be renamed.
- `create-led-driver-pcb`: struggled: turn 1: check_board reported routed 6/6 while route_board also reported 10 traces and 2 vias, leaving the relationship between logical connections and physical routing unclear.
- `create-led-driver-pcb`: struggled: turn 1: The rendered-board result provided a file path but no machine-readable visual or geometry summary to independently verify placement and silkscreen quality.
- `create-led-driver-pcb`: wished: turn 1: Provide a schema example or actionable field guidance when decouple placement cannot infer power and ground nets.
- `create-led-driver-pcb`: wished: turn 1: Return consistent, phase-specific counts for parts, nets, traces, and vias across PCB workflow tools.
- `create-led-driver-pcb`: wished: turn 1: Add a tool to inspect and rename conflicting local/global labels, or automatically resolve harmless same-name label warnings.
- `create-led-driver-pcb`: wished: turn 1: Expose a connectivity report mapping each logical net to its traces, vias, and connected pads.
- `create-led-driver-pcb`: wished: turn 1: Return structured board dimensions, footprint assignments, component coordinates, and silkscreen-clearance results alongside render_board.
- `create-led-driver-pcb`: wished: turn 1: Add an export manifest listing every generated fabrication file and its purpose.
- `local-board-move`: struggled: turn 1: move_parts initially refused the valid-looking request because its required field was `reference`, not the unlisted `ref`.
- `local-board-move`: struggled: turn 1: move_parts reported courtyard coordinates and a negative gap without clearly explaining the allowable placement range or suggesting the nearest valid target.
- `local-board-move`: struggled: turn 1: get_board returned insufficiently visible placement and copper details, making R1/R2 coordinates and affected segments difficult to verify.
- `local-board-move`: struggled: turn 1: route_board reported routing 3 traces for 1 requested net, which was confusing without a per-net trace summary.
- `local-board-move`: struggled: turn 1: The workflow did not provide an explicit before/after confirmation that the board outline, values, R2 position, and schematic were unchanged.
- `local-board-move`: wished: turn 1: Document move_parts argument schemas and validate or auto-correct aliases such as `ref` versus `reference`.
- `local-board-move`: wished: turn 1: Have move_parts return the nearest legal placement or a precise clearance-based placement recommendation after a courtyard refusal.
- `local-board-move`: wished: turn 1: Make get_board provide a concise structured diff of component positions, values, outline geometry, and changed copper.
- `local-board-move`: wished: turn 1: Have route_board report affected nets, trace counts, and preserved-versus-replaced segments separately.
- `local-board-move`: wished: turn 1: Provide a single final verification tool that checks unchanged constraints, rerouted-net scope, DRC/ERC, and render freshness in one result.
- `replace-pcb-component`: struggled: turn 1: The initial set_fields call confusingly rejected the Footprint field only after accepting the same mixed payload format conceptually.
- `replace-pcb-component`: struggled: turn 1: sync_board rejected the supplied keep-near intent on the existing board, and the required place_board workflow was not explicit enough.
- `replace-pcb-component`: struggled: turn 1: place_board with an empty payload failed with “illegal layout” even though its facts said both parts were placed.
- `replace-pcb-component`: struggled: turn 1: route_board reported 3 traces routed for a single requested net, which made the actual routing change difficult to interpret.
- `replace-pcb-component`: struggled: turn 1: get_board returned no visible structured board data in the transcript, limiting independent verification of outline, placement, and preserved routing.
- `replace-pcb-component`: wished: turn 1: Provide one atomic replace-component operation that validates and updates value, footprint, schematic, PCB, and affected routing.
- `replace-pcb-component`: wished: turn 1: Document a clear existing-board sync workflow with an explicit preserve-placement option and actionable examples.
- `replace-pcb-component`: wished: turn 1: Make place_board support a validated no-op or return the exact illegal-layout reason and proposed minimal correction.
- `replace-pcb-component`: wished: turn 1: Return concise before-and-after diffs for component properties, placement, outline geometry, and copper changes.
- `replace-pcb-component`: wished: turn 1: Report routing results by net with trace counts, modified segments, and preserved-versus-rerouted status.
- `replace-pcb-component`: wished: turn 1: Include render metadata or machine-readable geometry summaries so visual and placement claims can be independently checked.

## [variance]

- `create-led-driver-pcb`: cost: 191.6s elapsed, 101.4s agent, 14 provider requests
- `create-led-driver-pcb`: provider latency: #1=9100ms, #2=4800ms, #3=4400ms, #4=10300ms, #5=6800ms, #6=2200ms, #7=2700ms, #8=2500ms, #9=2900ms, #10=2400ms, #11=5300ms, #12=3300ms, #13=1800ms, #14=3900ms
- `finish-existing-pcb`: cost: 117.0s elapsed, 44.2s agent, 9 provider requests
- `finish-existing-pcb`: provider latency: #1=6600ms, #2=2100ms, #3=2700ms, #4=3700ms, #5=5200ms, #6=4200ms, #7=3600ms, #8=2000ms, #9=5000ms
- `local-board-move`: cost: 126.7s elapsed, 60.5s agent, 11 provider requests
- `local-board-move`: provider latency: #1=6700ms, #2=3000ms, #3=5500ms, #4=2300ms, #5=2000ms, #6=4000ms, #7=2000ms, #8=2600ms, #9=4100ms, #10=3300ms, #11=5000ms
- `replace-pcb-component`: cost: 143.9s elapsed, 64.1s agent, 18 provider requests
- `replace-pcb-component`: provider latency: #1=5400ms, #2=2600ms, #3=4100ms, #4=2900ms, #5=2200ms, #6=1900ms, #7=2000ms, #8=1900ms, #9=1700ms, #10=3300ms, #11=2100ms, #12=2400ms, #13=2900ms, #14=1600ms, #15=2000ms, #16=4200ms, #17=5000ms, #18=6300ms
