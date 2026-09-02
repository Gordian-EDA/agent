# Quality findings: create-led-driver-pcb-replace-pcb-component-local-board-move-finish-existing-pcb-create-i2c-sensor-pcb

Generated: 20260902T140715Z
Run output: /home/mimi/agent/quality/runs/pcb-integrated

Questions:
- with pour-aware routing, refit outlines and repair tools, do the PCB cases reach DRC 0 + fab with critic 8+ and no refusals

## [tool-contract]

- `replace-pcb-component`: tool `diff_schematic` refusal: {"error":"revision 4 did not capture design.kicad_sch"}
- `replace-pcb-component`: tool `sync_board` refusal: {"code":"intent_after_creation","error":"sync_board takes `intent` only when it creates the board. On an existing board pass the layout half to place_board({intent}) and any zones as rules {\"pours\": [{\"net\": …, \"layer\": …}]}."}
- `replace-pcb-component`: tool `place_board` refusal: {"code":"board_already_placed","error":"this board is already placed; every part has a position","note":"Nothing was moved. Adjust individual parts with move_parts, name the ones to re-place with {\"refs\": [...]}, or pass {\"replace\": true} to deliberately re-place the whole board and lose the current layout.","placement_applied":false}
- `create-i2c-sensor-pcb`: tool `get_footprint_info` refusal: {"error":"unknown footprint 'Jumper:SolderJumper-2_P1.3mm_Open'; did you mean Jumper:SolderJumper-2_P1.3mm_Open_Pad1.0x1.5mm, Jumper:SolderJumper-2_P1.3mm_Open_RoundedPad1.0x1.5mm, Jumper:SolderJumper-2_P1.3mm_Open_TrianglePad1.0x1.5mm?","suggestions":["Jumper:SolderJumper-2_P1.3mm_Open_Pad1.0x1.5mm","Jumper:SolderJumper-2_P1.3mm_Open_RoundedPad1.0x1.5mm","Jumper:SolderJumper-2_P1.3mm_Open_TrianglePad1.0x1.5mm"]}

## [harness]

- `local-board-move`: runner error: sync_board failed: {
  "error": "board has no net table",
  "restored": true,
  "revision": 2
}

## [judge]

- `create-led-driver-pcb`: judge: Remove the duplicate local/global CTRL label to clear the remaining ERC warning.
- `create-led-driver-pcb`: judge: Rework PCB silkscreen placement: reference designators and polarity markings overlap heavily around R2, R3, D1, and Q1, reducing assembly readability.
- `create-led-driver-pcb`: judge: Improve component spacing and silkscreen clearance around the central transistor/LED-driver cluster while preserving the electrically valid routing.
- `create-led-driver-pcb`: schematic critic: major/off-spine-leg/CTRL/base-drive region between the horizontal base resistor and Q1: The CTRL-to-Q1 base net rises from the horizontal base resistor to the upper rail, runs a long distance right, then drops again before reaching Q1, creating an avoidable multi-bend detour and excess empty space.
- `create-led-driver-pcb`: pcb critic: critical/placement/R1, Q1, C1 / lower board edge: The lower portions of R1, Q1, and C1, along with visible red copper, extend below the cyan board outline; move all three footprints fully inside the outline or redraw and resize the outline around the complete circuit before fabrication.
- `create-led-driver-pcb`: pcb critic: major/placement/C1, J1, Q1: C1 is placed at the lower-right boundary rather than tightly inside the J1/Q1 supply-return region; move C1 upward between J1 and Q1 to keep it inside the board and shorten the supply loop.
- `create-led-driver-pcb`: pcb critic: minor/placement/R1, J2, Q1: R1 is toward the lower-left while Q1 is near the lower center, so the CTRL base resistor is not directly adjacent to the transistor; move R1 closer to Q1 along the J2-to-base path for a shorter, cleaner control route.
- `replace-pcb-component`: schematic critic: minor/orientation/R1/R2 divider: Both series resistors are drawn vertically even though the preferred convention is horizontal series elements.
- `replace-pcb-component`: pcb critic: minor/board-utilisation/right half of the board outline: The outline leaves a large unused region to the right of the compact R1/R2 stack; if the outline were permitted to change, it should be tightened around the resistor group rather than retaining the excess area.
- `finish-existing-pcb`: judge: Provide distinct, clearly labeled front- and back-side PCB renders; the delivered render view does not visibly demonstrate both sides.
- `finish-existing-pcb`: pcb critic: major/board-utilisation/Entire board; R1/R2 populated strip: The two visible components occupy only the left-central strip of the rectangular outline, leaving roughly half the board width and large surrounding margins unused; a better layout would compact the outline around the pair or, if the outline must remain, center the populated region and reduce the excessive inter-part gap.
- `finish-existing-pcb`: pcb critic: minor/placement/R1 and R2: R1 and R2 are separated by an unnecessarily large vertical gap for two aligned resistors; placing them closer together while preserving their orientation would make the placement more compact and reduce the length of their interconnection.
- `finish-existing-pcb`: pcb critic: minor/silkscreen/R1/R2 reference designators: The R1 and R2 reference texts overlap in the central gap and are not individually clear; moving R1 above or beside the upper footprint and R2 below or beside the lower footprint would restore unambiguous association.
- `create-i2c-sensor-pcb`: judge: Correct the header silkscreen order: J1 pins are VCC, GND, SDA, SCL, but the board text reads VCC, GND, SCL, SDA.
- `create-i2c-sensor-pcb`: judge: Remove or justify the unnecessary bulky SMBJ3.3CA TVS and power LED circuitry to preserve the requested tiny breakout form factor.
- `create-i2c-sensor-pcb`: judge: Clean up the schematic presentation: avoid wires passing through the J1 symbol and eliminate the duplicate local/global SDA and SCL label warnings.
- `create-i2c-sensor-pcb`: schematic critic: major/spacing/C1, D2, and the overall supply/ground routing: The 100 nF decoupler C1 and the TVS diode D2 are placed far from U1 and the header, forcing very long rail runs and leaving large unused gaps across the sheet.
- `create-i2c-sensor-pcb`: schematic critic: major/text-overlap/D1 POWER LED: The POWER value text overlaps the LED graphic and its nearby vertical wiring, making the diode symbol and label read as a single crowded mark.
- `create-i2c-sensor-pcb`: schematic critic: minor/congestion/J1 and JP1 in the lower-left region: Several parallel vertical wires, header connections, and the address-jumper route are packed closely together, making the local net tracing slower than necessary.
- `create-i2c-sensor-pcb`: pcb critic: major/placement/U1, C1-C3, R1-R2: U1 is separated from its local decoupling capacitors and the I2C pull-ups, with C1-C3 several millimetres above/right of U1 and R1-R2 near J1; cluster U1 with C1-C3 and place R1/R2 directly on the short U1-to-J1 signal path.
- `create-i2c-sensor-pcb`: pcb critic: major/board-utilisation/upper-left and central board region: The board uses a tall outline for several separated placement islands, leaving broad low-density regions between the upper D/R group and the lower-right sensor/header circuitry; consolidate the groups and shorten the board around the resulting cluster.

## [self-diagnosis]

- `create-led-driver-pcb`: struggled: search_symbols returned no visible matches for the requested capacitor and power symbols, making symbol selection ambiguous.
- `create-led-driver-pcb`: struggled: get_symbol_info used Connector:Conn_01x02_Pin despite the search request using Connector_Generic:Conn_01x02, which made library-ID matching confusing.
- `create-led-driver-pcb`: struggled: place_parts reported 10 parts while the schematic placement phases later reported 8 parts and initially 0 nets, making design completeness difficult to verify.
- `create-led-driver-pcb`: struggled: place_board and route_board emitted inconsistent phase facts such as parts=0 and nets=0 despite successfully placing and routing the board.
- `create-led-driver-pcb`: struggled: review_board returned no actionable summary, so visual quality and component-label readability could not be assessed from tool output.
- `create-led-driver-pcb`: struggled: The CTRL same-local/global-label ERC warning was detected but had no available automatic fix or edit workflow.
- `create-led-driver-pcb`: wished: Provide a consolidated schematic/netlist report listing every reference, value, footprint, pin connection, and net before PCB synchronization.
- `create-led-driver-pcb`: wished: Make symbol and footprint search results include exact library IDs, descriptions, and clearly indicate zero-result queries.
- `create-led-driver-pcb`: wished: Return consistent component and net counts across placement, synchronization, and routing phases.
- `create-led-driver-pcb`: wished: Have review_board report concrete findings with severity, locations, and recommended corrections instead of only done.
- `create-led-driver-pcb`: wished: Expose rendered-image metadata or an image inspection result covering board dimensions, silkscreen overlaps, and readable labels.
- `create-led-driver-pcb`: wished: Add an ERC-fix operation for redundant local/global labels and other nonblocking schematic warnings.
- `replace-pcb-component`: struggled: diff_schematic rejected revision 4 because it had not captured the schematic, without clearly exposing valid revision-selection guidance.
- `replace-pcb-component`: struggled: sync_board refused the placement intent on the existing board, requiring knowledge of the separate place_board workflow.
- `replace-pcb-component`: struggled: place_board then refused because the board was already placed, making the attempted preservation-oriented placement step unnecessary.
- `replace-pcb-component`: struggled: get_board returned no summarized placement, outline, or copper details, so verifying what would be preserved required indirect tool results.
- `replace-pcb-component`: struggled: render_board and render_schematic returned image paths but no structured visual observations or inspectable image content in the transcript.
- `replace-pcb-component`: wished: Provide a clear current-revision or baseline identifier directly from diff_schematic and explain invalid revision errors.
- `replace-pcb-component`: wished: Allow sync_board on an existing board to update only changed footprints while preserving placement and routing automatically.
- `replace-pcb-component`: wished: Make place_board idempotent when called without an intent on an already placed board instead of returning a refusal.
- `replace-pcb-component`: wished: Return concise structured board facts from get_board, including outline dimensions, footprint positions, and affected tracks.
- `replace-pcb-component`: wished: Add a unified final verification tool that checks schematic-to-PCB field/footprint consistency, placement preservation, routing, ERC, DRC, and renders in one call.
- `finish-existing-pcb`: struggled: render_board produced a single generic image and did not clearly identify separate front- and back-side renders.
- `finish-existing-pcb`: struggled: route_board reported contradictory counts such as parts=0, nets=0, and routed=3 versus routed 1/1.
- `finish-existing-pcb`: struggled: check_board exposed only aggregate pass/fail facts, without a detailed DRC report or rule-by-rule results.
- `finish-existing-pcb`: struggled: get_board returned no visible board data, making outline, values, placement, and routing verification difficult.
- `finish-existing-pcb`: struggled: export_fab reported only a file count and path, without a manifest, file validation, or fabrication-parameter summary.
- `finish-existing-pcb`: wished: Provide explicit front and back render outputs with layer visibility metadata.
- `finish-existing-pcb`: wished: Return normalized routing statistics including total nets, routed nets, traces, vias, and remaining unrouted items.
- `finish-existing-pcb`: wished: Expose the complete DRC report with violation types, locations, severities, and waived findings.
- `finish-existing-pcb`: wished: Provide structured board inspection data for outline geometry, references, values, footprints, and component changes.
- `finish-existing-pcb`: wished: Validate exported Gerbers and drill files and return a complete manifest with sizes or checksums.
- `finish-existing-pcb`: wished: Offer a direct schematic-versus-PCB comparison to confirm component values and detect unintended schematic changes.
- `create-i2c-sensor-pcb`: struggled: get_footprint_info refused the intuitive jumper footprint name and required guessing a suggested pad-suffixed identifier.
- `create-i2c-sensor-pcb`: struggled: move_symbols reported unexpected GND and LED-net merges/splits, making connectivity changes difficult to interpret.
- `create-i2c-sensor-pcb`: struggled: render_schematic reported one visual finding without identifying the affected object or providing actionable details.
- `create-i2c-sensor-pcb`: struggled: place_board reported parts=0 and nets=0 despite placing 12 parts, which made the board-state facts confusing.
- `create-i2c-sensor-pcb`: struggled: route_board reported 17 traces and 11 vias but summarized routing as 6/6 without explaining the discrepancy.
- `create-i2c-sensor-pcb`: struggled: review_board returned no findings or summary, so its independent visual assessment was unusable.
- `create-i2c-sensor-pcb`: wished: A footprint search or canonical-ID resolver should accept aliases and automatically select a valid footprint.
- `create-i2c-sensor-pcb`: wished: Symbol movement should preserve connectivity or clearly explain every net merge and split before committing.
- `create-i2c-sensor-pcb`: wished: Render tools should return structured finding IDs, locations, severity, and suggested fixes.
- `create-i2c-sensor-pcb`: wished: Board workflow tools should report consistent part, net, trace, and via counts across placement and routing phases.
- `create-i2c-sensor-pcb`: wished: review_board should return a concise machine-readable summary of visual, spacing, labeling, and manufacturability findings.
- `create-i2c-sensor-pcb`: wished: A final design summary should enumerate components, sensor part number, pin mapping, pull-up values, jumper behavior, and confirmed routed nets.

## [variance]

- `create-led-driver-pcb`: cost: 204.2s elapsed, 97.4s agent, 6 provider requests
- `create-led-driver-pcb`: provider latency: #1=10500ms, #2=4600ms, #3=5400ms, #4=7100ms, #5=3500ms, #6=3600ms
- `replace-pcb-component`: cost: 127.3s elapsed, 61.8s agent, 18 provider requests
- `replace-pcb-component`: provider latency: #1=4700ms, #2=2500ms, #3=3500ms, #4=3000ms, #5=3200ms, #6=2000ms, #7=1300ms, #8=3100ms, #9=3700ms, #10=1900ms, #11=3200ms, #12=1600ms, #13=2800ms, #14=4900ms, #15=2500ms, #16=4300ms, #17=4200ms, #18=5200ms
- `local-board-move`: cost: 1.8s elapsed, 0.0s agent, 0 provider requests
- `finish-existing-pcb`: cost: 119.6s elapsed, 54.3s agent, 9 provider requests
- `finish-existing-pcb`: provider latency: #1=6600ms, #2=3000ms, #3=2900ms, #4=3400ms, #5=3900ms, #6=3500ms, #7=3000ms, #8=7800ms, #9=10600ms
- `create-i2c-sensor-pcb`: cost: 205.2s elapsed, 108.6s agent, 11 provider requests
- `create-i2c-sensor-pcb`: provider latency: #1=8100ms, #2=3500ms, #3=5900ms, #4=5200ms, #5=6700ms, #6=3300ms, #7=14800ms, #8=3900ms, #9=5300ms, #10=2500ms, #11=3700ms
