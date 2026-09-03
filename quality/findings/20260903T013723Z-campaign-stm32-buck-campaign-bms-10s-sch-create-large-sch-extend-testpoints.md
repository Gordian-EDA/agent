# Quality findings: campaign-stm32-buck-campaign-bms-10s-sch-create-large-sch-extend-testpoints

Generated: 20260903T013723Z
Run output: /home/mimi/agent/.claude/worktrees/w3/quality/runs/w3

Questions:
- does place_parts ever refuse a whole payload now, and does the bench/arrange loop produce a clean sheet

## [tool-contract]

- `campaign-stm32-buck`: turn 1 tool `read_schematic` refusal: {"error":"no schematic at /home/mimi/agent/.claude/worktrees/w3/quality/runs/w3/campaign-stm32-buck/project/design.kicad_sch yet — create one before editing it"}
- `campaign-bms-10s`: turn 1 tool `read_schematic` refusal: {"error":"no schematic at /home/mimi/agent/.claude/worktrees/w3/quality/runs/w3/campaign-bms-10s/project/design.kicad_sch yet — create one before editing it"}
- `campaign-bms-10s`: turn 1 tool `assign_footprints` refusal: {"code":"invalid_payload","error":"symbol/footprint mismatch; nothing was written","footprint_mismatch":[{"extra_pins":["4","5"],"footprint":"Package_SO:PowerPAK_SO-8L_Single","message":"footprint pad(s) 4, 5 have no symbol pin","ref":"Q1","suggestion":"Package_SO:PowerPAK_SO-8_Single","symbol":"Transistor_FET:Q_NMOS_GSD"},{"extra_pins":["4","5"],"footprint":"Package_SO:PowerPAK_SO-8L_Single","message":"footprint pad(s) 4, 5 have no symbol pin","ref":"Q2","suggestion":"Package_SO:PowerPAK_SO-8_Single","symbol":"Transistor_FET:Q_NMOS_GSD"}]}
- `campaign-bms-10s`: turn 1 tool `assign_footprints` refusal: {"code":"invalid_payload","error":"symbol/footprint mismatch; nothing was written","footprint_mismatch":[{"extra_pins":["4","5"],"footprint":"Package_SO:PowerPAK_SO-8_Single","message":"footprint pad(s) 4, 5 have no symbol pin","ref":"Q1","suggestion":"Package_SO:PowerPAK_SO-8L_Single","symbol":"Transistor_FET:Q_NMOS_GSD"},{"extra_pins":["4","5"],"footprint":"Package_SO:PowerPAK_SO-8_Single","message":"footprint pad(s) 4, 5 have no symbol pin","ref":"Q2","suggestion":"Package_SO:PowerPAK_SO-8L_Single","symbol":"Transistor_FET:Q_NMOS_GSD"}]}
- `campaign-bms-10s`: turn 1 tool `label` refusal: {"error":"R3.1 is already on net `DSG_GATE`; a label does not replace that name, it merges `DSG_GATE` and `DSG_DRIVE` into one net. Use delete_wires to take R3.1 off `DSG_GATE` first, or name a pin that is loose."}
- `campaign-bms-10s`: turn 1 tool `label` refusal: {"error":"U1.DSG is already on net `DSG_GATE`; a label does not replace that name, it merges `DSG_GATE` and `DSG_DRIVE` into one net. Use delete_wires to take U1.DSG off `DSG_GATE` first, or name a pin that is loose."}
- `campaign-bms-10s`: turn 1 tool `add_power` refusal: {"error":"no power symbol for `PACK_NEG_RAW` (tried power:PACK_NEG_RAW, power:+PACK_NEG_RAW); pass `lib_id` explicitly"}
- `campaign-bms-10s`: turn 1 tool `add_power` refusal: {"error":"no power symbol for `PACK_NEG_RAW` (tried power:PACK_NEG_RAW, power:+PACK_NEG_RAW); pass `lib_id` explicitly"}
- `sch-create-large`: turn 1 tool `read_schematic` refusal: {"error":"no schematic at /home/mimi/agent/.claude/worktrees/w3/quality/runs/w3/sch-create-large/project/design.kicad_sch yet — create one before editing it"}
- `sch-create-large`: turn 1 tool `search_symbols` refusal: {"error":"duplicate discovery call deferred","note":"reuse the coalesced results returned by the first call"}
- `sch-create-large`: turn 1 tool `place_parts` refusal: {"error":"invalid place_parts input at `intent.rails`: invalid type: string \"+3V3\", expected a map"}
- `sch-create-large`: turn 1 tool `rewire` refusal: {"code":"layout_unchanged","error":"the re-layout would have changed connectivity (Mismatch { scattered: [], shorted: [], disturbed: [\"GND\"] }); the sheet and its netlist are untouched. Arrange a smaller selection — one symbol at a time with `refs` always works.","ok":false,"report":{"committed":false,"labelled":1,"mismatch":{"disturbed":["GND"],"scattered":[],"shorted":[]},"moved":["R7","U2"],"nets":["+3V3","BOOT0","GND","I2C_SCL","I2C_SDA","LED_POWER","LED_STATUS","OSC_IN","OSC_OUT","SWCLK","SWDIO"],"redrawn":96,"warnings":[]}}
- `sch-extend-testpoints`: turn 1 tool `place_parts` refusal: {"code":"derived_net_name","nets":["refused: `Net-(P2-P1)` is the name KiCAD generates for an unnamed net (C2.1, P2.1, R3.1), not a label anything can join — naming a new node `Net-(P2-P1)` forks it and renames the original to `Net-(P2-P1)_1`. Write \"@C2.1\" to join that pin's net whatever it is called, or name the net first with `label({pin: \"C2.1\", net: \"…\"})` and use the name you gave it.","refused: `Net-(P3-P1)` is the name KiCAD generates for an unnamed net (#FLG06.1, C1.1, P3.1, U1.6), not a label anything can join — naming a new node `Net-(P3-P1)` forks it and renames the original to `Net-(P3-P1)_1`. Write \"@#FLG06.1\" to join that pin's net whatever it is called, or name the net first with `label({pin: \"#FLG06.1\", net: \"…\"})` and use the name you gave it."],"ok":false}

## [prompt]

- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-stm32-buck`: turn 1 loop smell: tool `place_parts` called 4 times in a row
- `campaign-stm32-buck`: cost: 356.8s elapsed, 281.9s agent, 19 provider requests
- `campaign-bms-10s`: turn 1 loop smell: tool `place_parts` called 3 times in a row
- `campaign-bms-10s`: turn 1 loop smell: tool `get_symbol` called 4 times in a row
- `campaign-bms-10s`: turn 1 loop smell: tool `label` called 3 times in a row
- `campaign-bms-10s`: turn 1 loop smell: tool `get_symbol` called 10 times in a row
- `campaign-bms-10s`: cost: 582.3s elapsed, 279.4s agent, 19 provider requests
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 5 times in a row
- `sch-create-large`: turn 1 loop smell: tool `place_parts` called 4 times in a row
- `sch-create-large`: cost: 355.4s elapsed, 273.4s agent, 29 provider requests

## [engine]

- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — actual 5
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — actual 3
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: PCB was not created, placed, routed, rendered, or DRC-checked.
- `campaign-stm32-buck`: judge: No Gerbers, drill files, position files, or BOM were exported.
- `campaign-stm32-buck`: judge: USB-C CC1 and CC2 are explicitly unconnected; the 5.1 kΩ pulldowns are isolated from the CC pins.
- `campaign-stm32-buck`: judge: USB_VSENSE is only connected between the 100 kΩ/33 kΩ divider resistors and is not connected to any STM32 GPIO.
- `campaign-stm32-buck`: judge: The four GPIO headers do not provide the requested breakout: most signal pins are no-connect, with only PB0 visibly connected.
- `campaign-stm32-buck`: judge: Schematic presentation is unacceptable for release: it is spread across a very large canvas with numerous text collisions and wires passing through symbol bodies.
- `campaign-stm32-buck`: judge: The final schematic was not revalidated to the requested zero-finding state after the last edits; completeness advisories remain.
- `campaign-stm32-buck`: schematic critic: major/spacing/Overall sheet; U3 and J4-J7: The MCU and GPIO-header blocks are separated by a very large empty area and connected by long perimeter rails, making the drawing substantially more sprawling than necessary.
- `campaign-stm32-buck`: schematic critic: major/spacing/Y1, C10, C11, and U3: The 8 MHz crystal and its load capacitors are far from the MCU, producing long HSE_IN/HSE_OUT routes instead of a compact clock block beside PH0 and PH1.
- `campaign-stm32-buck`: schematic critic: major/spacing/U3 VDD pins and C18-C20: The MCU VDD decouplers are not arranged immediately beside the MCU supply-pin group, weakening the visual association between each supply pin and its capacitor bank.
- `campaign-stm32-buck`: schematic critic: minor/orientation/USB data path; R10 and R11: The 22-ohm USB series resistors are vertical despite being elements in horizontal D+/D- signal paths, creating unnecessary jogs.
- `campaign-stm32-buck`: schematic critic: minor/text-overlap/U3 top supply pins and J3 SWD header: Power labels, pin names, and net labels are crowded around the MCU top edge and SWD header, reducing legibility at the rendered scale.
- `campaign-bms-10s`: failed check: pcb_created == true — actual false
- `campaign-bms-10s`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-bms-10s`: failed check: schematic_critic_score >= 8 — actual 5
- `campaign-bms-10s`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-bms-10s`: failed check: human_look_schematic_score >= 8 — actual 2
- `campaign-bms-10s`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-bms-10s`: schematic critic: major/spacing/Entire sheet; especially U1 versus the far-right TH1/TH2 and CAP1/CAP2 section and the bottom bypass section: Related circuit sections are separated by very large empty regions and connected by exceptionally long green rails, making the schematic read as a stretched collection of fragments rather than a compact set of functional blocks.
- `campaign-bms-10s`: schematic critic: major/spacing/U1 cell-monitor region; R10–R20 and C11–C20: The ten-channel RC filter bank is scattered around the monitor instead of being aligned as a compact, consistently ordered bank, forcing the reader to search across multiple rows and zig-zag routes for channel relationships.
- `sch-create-large`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "C10", "with": "+3V3"}, {"field": "Value", "ref": "C11", "with": "+3V3"}, {"field": "Value", "ref": "C6", "with": "+3V3"}, {"field": "Value", "ref": "C7", "with": "GND"}, {"field": "Value", "ref": "C9", "with": "+3V3"}, {"field": "Value", "ref": "C9", "with": "GND"}, {"field": "Value", "ref": "C9", "with": "Net-(C9-Pad1)"}, {"field": "Value", "ref": "D4", "with": "+3V3"}, {"field": "Value", "ref": "F2", "with": "+3V3"}, {"field": "Value", "ref": "J1", "with": "+3V3"}, {"field": "Value", "ref": "J2", "with": "GND"}, {"field": "Reference", "ref": "L1", "with": "+3V3"}, {"field": "Value", "ref": "L1", "with": "+3V3"}, {"field": "Value", "ref": "L1", "with": "Net-(U1-FB)"}, {"field": "Value", "ref": "R11", "with": "+3V3"}, {"field": "Value", "ref": "R6", "with": "Net-(C9-Pad1)"}, {"field": "Value", "ref": "R7", "with": "BOOT0"}, {"field": "Value", "ref": "R7", "with": "GND"}]
- `sch-create-large`: failed check: schematic_critic_score >= 8 — actual 5
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-large`: judge: Connect the reset RC network to U2.NRST (pin 7); it is currently unconnected, so reset is nonfunctional.
- `sch-create-large`: judge: Correct the LM2596 feedback divider for 3.3 V; the shown 10 kΩ/6.8 kΩ divider produces approximately 2.1 V, not 3.3 V.
- `sch-create-large`: judge: Add decoupling for every STM32 supply pin/rail; only three MCU decoupling capacitors are present for five supply pins.
- `sch-create-large`: judge: Recompose the schematic into compact functional blocks; the MCU is stranded far below the rest with excessive unused canvas.
- `sch-create-large`: judge: Replace long global power rails with local power symbols and net labels, and eliminate the numerous component/value text collisions.
- `sch-create-large`: judge: Re-space the regulator, protection, crystal, LED, and sensor circuitry so wires do not pass through symbol bodies and labels remain readable.
- `sch-create-large`: judge: Run a final schematic check after the last power-support placement; the delivered verification predates that edit and DRC was not run.
- `sch-create-large`: schematic critic: major/spacing/U2 relative to the upper power, clock, reset, and debug circuitry: U2 is positioned far below the main schematic, leaving a large empty region and requiring long vertical runs for oscillator, reset, boot, ground, and related MCU connections.
- `sch-create-large`: schematic critic: major/spacing/Overall sheet, especially U3/J3 relative to U1 and U2: The sensor and auxiliary protected-power connector are spread far to the right of the regulator and MCU, producing long full-width rails and excessive empty gaps between related functional blocks.
- `sch-create-large`: schematic critic: minor/text-overlap/X1/C10/C11 oscillator region: The vertical OSC_IN/OSC_OUT annotations and nearby crystal-load capacitor text are tightly packed around X1, making that small clock section harder to read at a glance.
- `sch-extend-testpoints`: failed check: text_collisions_added == [] — actual [{"field": "Footprint", "ref": "P2", "with": "N_P2_1"}]
- `sch-extend-testpoints`: judge: Remove the extra #PWR_GND_0/1 symbol; the request permits only three added test-point symbols.
- `sch-extend-testpoints`: judge: Preserve the original unnamed-net identities instead of renaming the P2 and P3 pin-1 nets to N_P2_1 and N_P3_1.
- `sch-extend-testpoints`: judge: Relocate TP2 and TP3 beside P2 pin 1 and P3 pin 1 respectively so the rendered schematic clearly shows their association.
- `sch-extend-testpoints`: judge: Resolve the introduced collision between P2's Footprint field and the N_P2_1 net label.
- `sch-extend-testpoints`: schematic critic: major/spacing/TP1, TP2, TP3 relative to P2, P3, and the GND network: All three added test points are detached from the circuitry they serve, with TP2 far left of P2, TP3 separated from P3, and TP1 isolated above the main ground network.

## [harness]

- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors
- `campaign-bms-10s`: failed check: drc_errors == 0 — not measured: drc_errors
- `campaign-bms-10s`: judge unavailable: The read operation timed out

## [judge]

- `campaign-stm32-buck`: schematic human-look: Reorganize the schematic into compact functional blocks instead of spreading circuitry across a very wide canvas.
- `campaign-stm32-buck`: schematic human-look: Replace long perimeter wires with net labels and local power symbols to reduce visual travel and ambiguity.
- `campaign-stm32-buck`: schematic human-look: Separate and space the crowded USB, power-protection, and debug areas so labels and component references do not overlap.
- `campaign-bms-10s`: schematic human-look: Eliminate the enormous unused canvas and regroup related circuitry into compact functional blocks.
- `campaign-bms-10s`: schematic human-look: Replace page-spanning perimeter wires with local labels and short, orthogonal connections.
- `campaign-bms-10s`: schematic human-look: Align components and standardize spacing, orientations, and text placement to remove the scattered, uneven presentation.
- `sch-create-large`: schematic human-look: Recompose the sheet into compact functional blocks; the MCU is stranded far below the rest with excessive unused canvas.
- `sch-create-large`: schematic human-look: Replace the long horizontal and vertical power rails with local power symbols and net labels to reduce visual sprawl.
- `sch-create-large`: schematic human-look: Re-space and align the central regulator, protection, LED, and crystal circuitry so labels and wires do not crowd or overlap.
- `sch-extend-testpoints`: schematic human-look: Remove or hide the extensive footprint/value metadata text overlapping symbols and wires.
- `sch-extend-testpoints`: schematic human-look: Repack the circuit into a compact, clearly grouped left-to-right layout instead of spreading blocks across the oversized sheet.
- `sch-extend-testpoints`: schematic human-look: Align component references and values consistently, and relocate test points and no-connect markers beside the relevant circuitry.

## [self-diagnosis]

- `campaign-stm32-buck`: struggled: turn 1: The per-turn 280-second limit and 26-call budget stopped the task before PCB creation, routing, DRC, and fabrication export.
- `campaign-stm32-buck`: struggled: turn 1: check_schematic reported heuristic completeness gaps for VBAT, VDDA, and 3V3 power-entry protection even though those circuits were not part of the requested design, obscuring actual issues.
- `campaign-stm32-buck`: struggled: turn 1: place_parts added 13 support components and left three gaps while providing no clear connectivity-level explanation of what was missing.
- `campaign-stm32-buck`: struggled: turn 1: move_symbols fixed the D2 polarity error but reported confusing net merge/split changes involving 3V3_LED and GND.
- `campaign-stm32-buck`: struggled: turn 1: render_schematic reported 17 visual findings without exposing sufficiently actionable collision locations or an annotated inspection view.
- `campaign-stm32-buck`: struggled: turn 1: There was no opportunity to validate whether the generated component footprints, board outline, routing constraints, or exports satisfied the requested fabrication requirements.
- `campaign-stm32-buck`: wished: turn 1: Provide a fast batch-edit tool for schematic cleanup, support-part insertion, and validation in one transaction.
- `campaign-stm32-buck`: wished: turn 1: Distinguish mandatory design-rule failures from optional heuristic completeness advisories and allow explicit suppression of irrelevant suggestions.
- `campaign-stm32-buck`: wished: turn 1: Make placement operations return a pin-by-pin connectivity diff and identify exactly which requested net or pin each unresolved gap concerns.
- `campaign-stm32-buck`: wished: turn 1: Have symbol moves preserve and clearly report net connectivity, with polarity fixes available as explicit pin-swap or orientation operations.
- `campaign-stm32-buck`: wished: turn 1: Return structured visual-diagnostic coordinates, severity, and suggested fixes for schematic and PCB render findings.
- `campaign-stm32-buck`: wished: turn 1: Support an end-to-end PCB generation, placement, autorouting, DRC, rendering, and fabrication-export pipeline within a single resumable job.
- `campaign-bms-10s`: struggled: turn 1: check_schematic reported ERC clean while simultaneously reporting six blocking errors and eight findings, making the pass/fail state confusing.
- `campaign-bms-10s`: struggled: turn 1: check_schematic offered no safe fix for the DSG_GATE and REGOUT dangling-passive errors, despite their intended gate-drive and pull-down functions being clear.
- `campaign-bms-10s`: struggled: turn 1: get_symbol returned only "done" without pin names, coordinates, orientation, or connectivity details needed to repair ambiguous passive connections.
- `campaign-bms-10s`: struggled: turn 1: The toolset consumed the entire turn on schematic inspection and placement, leaving no practical time to create the board, route it, run DRC, or export fabrication files.
- `campaign-bms-10s`: struggled: turn 1: Schematic completeness diagnostics identified missing bypass capacitors and I2C pull-ups but provided no direct component-placement or wiring operation to add them efficiently.
- `campaign-bms-10s`: struggled: turn 1: Unconnected-wire warnings at raw coordinates lacked enough contextual information to identify the offending objects or determine whether they were genuine issues.
- `campaign-bms-10s`: wished: turn 1: Provide a single authoritative ERC/diagnostic summary that distinguishes actual errors, warnings, advisories, and stale or contradictory checks.
- `campaign-bms-10s`: wished: turn 1: Add a connectivity-aware repair operation that can connect a component pin to a named net or appropriate functional endpoint without requiring ambiguous pin-coordinate inference.
- `campaign-bms-10s`: wished: turn 1: Make get_symbol return the complete symbol pin map, pin numbers, net names, footprint, and connected endpoints.
- `campaign-bms-10s`: wished: turn 1: Provide batch operations for adding standard support networks such as pull-ups, bypass capacitors, gate resistors, and RC filters from a declarative specification.
- `campaign-bms-10s`: wished: turn 1: Add a board-generation workflow that automatically syncs the schematic, places grouped blocks, routes constrained nets, and reports remaining unrouted items.
- `campaign-bms-10s`: wished: turn 1: Expose direct export, render, ERC, and DRC commands with predictable completion status so fabrication outputs can be generated and verified within one turn.
- `sch-create-large`: struggled: turn 1: render_schematic reported 22 visual findings without describing which symbols or connections were problematic.
- `sch-create-large`: struggled: turn 1: check_schematic flagged VIN_PROTECTED and +3V3 power-entry gaps even though the requested USB-C protection and regulator circuitry were already present.
- `sch-create-large`: struggled: turn 1: rewire refused the R7/U2 operation because it would disturb GND, but did not explain the specific connectivity conflict or affected geometry.
- `sch-create-large`: struggled: turn 1: The time limit prevented a final render and verification after the completeness-support block was added.
- `sch-create-large`: struggled: turn 1: place_parts reported 80 parts while the agent summary reported 41/41 parts, making the resulting component count unclear.
- `sch-create-large`: wished: turn 1: render_schematic should return structured visual findings with symbol references, locations, and suggested fixes.
- `sch-create-large`: wished: turn 1: check_schematic should distinguish genuinely missing requested circuitry from heuristic recommendations already satisfied elsewhere in the design.
- `sch-create-large`: wished: turn 1: rewire should provide the exact conflicting pins, wires, and coordinates or offer a connectivity-preserving fallback.
- `sch-create-large`: wished: turn 1: A focused arrange or move operation should support layout cleanup without recalculating or disturbing unrelated GND connectivity.
- `sch-create-large`: wished: turn 1: The workflow should reserve enough budget for an automatic final render and check after the last schematic modification.
- `sch-create-large`: wished: turn 1: project_info or a dedicated summary tool should report authoritative component, net, ERC, and render status consistently.
- `sch-extend-testpoints`: struggled: turn 1: read_schematic returned no visible component positions or pin coordinates, making precise placement difficult.
- `sch-extend-testpoints`: struggled: turn 1: place_parts refused derived net names without clearly explaining the required corrected payload format.
- `sch-extend-testpoints`: struggled: turn 1: The successful place_parts result did not state whether existing parts were preserved in their original positions.
- `sch-extend-testpoints`: struggled: turn 1: render_schematic reported one introduced visual finding without identifying the finding or showing actionable details.
- `sch-extend-testpoints`: struggled: turn 1: diff_schematic reported net renames caused by the operation, making it unclear whether the original unnamed nets were preserved correctly.
- `sch-extend-testpoints`: struggled: turn 1: The final render path contained a duplicated `quality/runs/w3` segment.
- `sch-extend-testpoints`: wished: turn 1: Provide a structured schematic summary containing every reference, position, pin coordinate, and connected net.
- `sch-extend-testpoints`: wished: turn 1: Allow test-point placement by directly specifying an existing pin endpoint instead of requiring net-name inference.
- `sch-extend-testpoints`: wished: turn 1: Have placement tools report an explicit before/after invariant for all existing references and coordinates.
- `sch-extend-testpoints`: wished: turn 1: Return the exact visual findings and affected objects from render_schematic.
- `sch-extend-testpoints`: wished: turn 1: Prevent or clearly confirm automatic renaming of unnamed nets when joining new symbols.
- `sch-extend-testpoints`: wished: turn 1: Return canonical artifact paths in final tool results and generated responses.

## [variance]

- `campaign-stm32-buck`: provider latency: #1=7500ms, #2=3800ms, #3=14700ms, #4=6700ms, #5=7700ms, #6=11400ms, #7=8200ms, #8=7400ms, #9=19200ms, #10=11100ms, #11=8200ms, #12=8900ms, #13=7800ms, #14=4900ms, #15=6800ms, #16=6100ms, #17=12800ms, #18=7800ms, #19=15800ms
- `campaign-bms-10s`: provider latency: #1=6200ms, #2=3100ms, #3=19500ms, #4=7300ms, #5=7100ms, #6=4200ms, #7=6300ms, #8=7400ms, #9=24700ms, #10=13300ms, #11=10100ms, #12=10400ms, #13=10600ms, #14=9900ms, #15=10400ms, #16=4700ms, #17=14200ms, #18=5600ms, #19=8200ms
- `sch-create-large`: provider latency: #1=7600ms, #2=3200ms, #3=5900ms, #4=5800ms, #5=5400ms, #6=4100ms, #7=4500ms, #8=12000ms, #9=4900ms, #10=4000ms, #11=7000ms, #12=11900ms, #13=9300ms, #14=7200ms, #15=9000ms, #16=6600ms, #17=4700ms, #18=4800ms, #19=9500ms, #20=6400ms, #21=4400ms, #22=6500ms, #23=4500ms, #24=6300ms, #25=4900ms, #26=5500ms, #27=14700ms, #28=8600ms, #29=8000ms
- `sch-extend-testpoints`: cost: 110.1s elapsed, 49.5s agent, 6 provider requests
- `sch-extend-testpoints`: provider latency: #1=7700ms, #2=4000ms, #3=7500ms, #4=5500ms, #5=5100ms, #6=7200ms
