# Quality findings: sch-create-large-sch-extend-testpoints

Generated: 20260903T015218Z
Run output: /home/mimi/agent/.claude/worktrees/w3/quality/runs/w3b

Questions:
- are the intent-shape and rail-erase refusals gone

## [tool-contract]

- `sch-create-large`: turn 1 tool `read_schematic` refusal: {"error":"no schematic at /home/mimi/agent/.claude/worktrees/w3/quality/runs/w3b/sch-create-large/project/design.kicad_sch yet — create one before editing it"}
- `sch-create-large`: turn 1 tool `search_footprints` refusal: {"error":"missing required string field `symbol`"}
- `sch-create-large`: turn 1 tool `search_footprints` refusal: {"error":"duplicate discovery call deferred","note":"reuse the coalesced results returned by the first call"}
- `sch-create-large`: turn 1 tool `search_footprints` refusal: {"error":"duplicate discovery call deferred","note":"reuse the coalesced results returned by the first call"}
- `sch-create-large`: turn 1 tool `search_footprints` refusal: {"error":"duplicate discovery call deferred","note":"reuse the coalesced results returned by the first call"}
- `sch-create-large`: turn 1 tool `get_symbol_info` refusal: {"error":"`lib_ids` must contain 1 to 12 symbols"}
- `sch-create-large`: turn 1 tool `get_symbol_info` refusal: {"error":"unknown symbol `Connector_USB:USB_C_Receptacle_GCT_USB4125-xx-x-0190_6P_TopMnt_Horizontal`","suggestions":[]}
- `sch-create-large`: turn 1 tool `get_symbol_info` refusal: {"error":"unknown symbol `Regulator_Switching:TPS62160`","suggestions":["Regulator_Switching:TPS62160DGK","Regulator_Switching:TPS62160DSG"]}
- `sch-create-large`: turn 1 tool `get_symbol` refusal: {"error":"no symbol `U2` on the sheet"}
- `sch-create-large`: turn 1 tool `get_symbol` refusal: {"error":"no symbol `P4` on the sheet"}
- `sch-extend-testpoints`: turn 1 tool `place_parts` refusal: {"code":"derived_net_name","nets":["refused: `Net-(P2-P1)` is the name KiCAD generates for an unnamed net (C2.1, P2.1, R3.1), not a label anything can join — naming a new node `Net-(P2-P1)` forks it and renames the original to `Net-(P2-P1)_1`. Write \"@C2.1\" to join that pin's net whatever it is called, or name the net first with `label({pin: \"C2.1\", net: \"…\"})` and use the name you gave it.","refused: `Net-(P3-P1)` is the name KiCAD generates for an unnamed net (#FLG06.1, C1.1, P3.1, U1.6), not a label anything can join — naming a new node `Net-(P3-P1)` forks it and renames the original to `Net-(P3-P1)_1`. Write \"@#FLG06.1\" to join that pin's net whatever it is called, or name the net first with `label({pin: \"#FLG06.1\", net: \"…\"})` and use the name you gave it."],"ok":false}

## [prompt]

- `sch-create-large`: turn 1 loop smell: tool `search_footprints` called 4 times in a row
- `sch-create-large`: turn 1 loop smell: tool `get_symbol_info` called 19 times in a row
- `sch-create-large`: turn 1 loop smell: tool `get_symbol` called 5 times in a row
- `sch-create-large`: cost: 347.7s elapsed, 256.0s agent, 24 provider requests
- `sch-extend-testpoints`: turn 1 loop smell: tool `get_symbol` called 3 times in a row
- `sch-extend-testpoints`: cost: 171.5s elapsed, 104.4s agent, 11 provider requests

## [engine]

- `sch-create-large`: failed check: text_collisions == [] — actual [{"field": "Value", "ref": "R11", "with": "Net-(D4-A)"}, {"field": "Value", "ref": "R12", "with": "+3V3"}, {"field": "Value", "ref": "R4", "with": "+3V3"}, {"field": "Value", "ref": "R8", "with": "+3V3"}, {"field": "Value", "ref": "U3", "with": "GND"}]
- `sch-create-large`: failed check: schematic_critic_score >= 8 — actual 3
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — actual 4
- `sch-create-large`: judge: Add a dedicated decoupling capacitor for the fourth STM32 supply pin; U2 has four supply pins but only three MCU decouplers C10–C12.
- `sch-create-large`: judge: Rework the indicator circuit: D3/R11 and D4/R12 are chained in series from +3V3 to GND, and LED_STATUS has no STM32 endpoint; give each LED an independent, correctly driven path.
- `sch-create-large`: judge: Rearrange the schematic into compact functional blocks and replace page-spanning bus wires with local net labels.
- `sch-create-large`: judge: Resolve the five reported text collisions around R4, R8, R11, R12, and U3.
- `sch-create-large`: judge: Reroute wires passing through the bodies of D1, J1, and R4; the final render reports these visual violations.
- `sch-create-large`: schematic critic: critical/wire-through-body/R4 / central horizontal rail: The long horizontal green rail passes through the R4 resistor body even though R4’s two pins are vertically oriented above and below the body.
- `sch-create-large`: schematic critic: critical/dangling-pin/L1 buck inductor output: The right pin of L1 ends in empty space and is not visibly connected to the +3V3 output net.
- `sch-create-large`: schematic critic: major/spacing/whole sheet, especially MCU-to-peripheral routing: The functional blocks are spread over a very large canvas with long perimeter-like vertical runs and a large unused lower-right region.
- `sch-create-large`: schematic critic: minor/congestion/J1 CC1/CC2 pin area: The CC1/CC2 labels, connector pin annotations, and nearby wiring are tightly packed and visually collide as a label cluster.
- `sch-extend-testpoints`: failed check: text_collisions_added == [] — actual [{"field": "Footprint", "ref": "P2", "with": "N_P2_1"}, {"field": "Value", "ref": "TP3", "with": "N_P3_1"}]
- `sch-extend-testpoints`: judge: Resolve the two introduced text collisions: P2’s footprint field overlaps N_P2_1, and TP3’s value overlaps N_P3_1.
- `sch-extend-testpoints`: judge: Reposition the new test points close to their associated connector pins and GND circuitry without moving existing parts; the current floating placement makes the intended attachment difficult to read.
- `sch-extend-testpoints`: judge: Avoid renaming the pre-existing unnamed P2-pin-1 and P3-pin-1 nets to N_P2_1 and N_P3_1; connect the test points by pin-relative net reference instead.
- `sch-extend-testpoints`: schematic critic: major/spacing/TP1, TP2, and TP3 relative to P2, P3, and nearby ground symbols: All three test points are isolated in large blank areas rather than being placed adjacent to the nets they monitor, forcing the reader to rely on distant net labels and making the sheet substantially more sprawling than necessary.

## [judge]

- `sch-create-large`: schematic human-look: Replace the extremely long bus-like wires with short, locally placed net labels and grouped functional blocks.
- `sch-create-large`: schematic human-look: Reposition overlapping labels and symbols around J1, C1/C2, D2, and the MCU so references and values are clearly readable.
- `sch-create-large`: schematic human-look: Repack the schematic into balanced sections with consistent alignment and substantially less unused canvas.
- `sch-extend-testpoints`: schematic human-look: Compact the design into a coherent left-to-right signal flow instead of spreading related parts across a very large empty canvas.
- `sch-extend-testpoints`: schematic human-look: Hide footprint and library-field text, which currently overlaps symbols and wires and overwhelms the readable references and values.
- `sch-extend-testpoints`: schematic human-look: Align components, connectors, test points, and ground symbols to a consistent grid with shorter, cleaner wire runs.

## [self-diagnosis]

- `sch-create-large`: struggled: turn 1: render_schematic reported eight visual findings without identifying their exact locations or affected references, making cleanup difficult.
- `sch-create-large`: struggled: turn 1: check_schematic was electrically clean while visual wires-through-symbols and text collisions remained undetected by ERC.
- `sch-create-large`: struggled: turn 1: place_parts introduced an unexplained +3V3 protection gap that required adding an extra D5 TVS late in the task.
- `sch-create-large`: struggled: turn 1: get_symbol refused reference P4 because it did not exist, without explaining which actual symbol or reference was intended.
- `sch-create-large`: struggled: turn 1: The remaining time budget ended before the recommended arrange-based presentation cleanup could be performed.
- `sch-create-large`: wished: turn 1: render_schematic should return structured visual findings with coordinates, references, collision types, and suggested fixes.
- `sch-create-large`: wished: turn 1: A schematic-specific layout/arrange tool should support deterministic movement of individual symbols and fields while preserving connectivity.
- `sch-create-large`: wished: turn 1: check_schematic should include visual quality checks in its actionable diagnostics, not only electrical ERC results.
- `sch-create-large`: wished: turn 1: place_parts should expose the generated symbol/reference/net mapping and explain completeness-gap detection decisions.
- `sch-create-large`: wished: turn 1: get_symbol should provide fuzzy reference lookup or list nearby valid references when a requested reference is absent.
- `sch-extend-testpoints`: struggled: turn 1: read_schematic and get_symbol returned no usable geometry or pin-coordinate details, forcing placement and connectivity to be inferred.
- `sch-extend-testpoints`: struggled: turn 1: place_parts initially refused the existing unnamed connector nets and did not provide a direct pin-attachment workflow for new symbols.
- `sch-extend-testpoints`: struggled: turn 1: The placement relation syntax was ambiguous and allowed the GND test point to be grouped with P2 rather than explicitly targeting its intended net.
- `sch-extend-testpoints`: struggled: turn 1: connect renamed the existing unnamed P2 and P3 nets to N_P2_1 and N_P3_1, changing net naming while adding the test points.
- `sch-extend-testpoints`: struggled: turn 1: remove_symbols was used to recover the GND connection and deleted the existing #PWR_GND_0 symbol, conflicting with the requirement to leave existing parts unchanged.
- `sch-extend-testpoints`: struggled: turn 1: render_schematic reported two introduced visual findings without describing them, making visual verification insufficient.
- `sch-extend-testpoints`: wished: turn 1: Expose complete symbol positions, orientations, pin coordinates, and current net memberships in read_schematic and get_symbol results.
- `sch-extend-testpoints`: wished: turn 1: Support placing a symbol directly onto an existing pin or net with an explicit pin-to-pin attachment field.
- `sch-extend-testpoints`: wished: turn 1: Allow joining an unnamed existing net by pin reference without renaming that net or requiring a label.
- `sch-extend-testpoints`: wished: turn 1: Add a transaction or guard preventing removal or modification of pre-existing symbols when the task requires preservation.
- `sch-extend-testpoints`: wished: turn 1: Make placement relations specify exact anchor pins rather than only symbol-level groups and sides.
- `sch-extend-testpoints`: wished: turn 1: Return the actual visual finding descriptions and locations from render_schematic.

## [variance]

- `sch-create-large`: provider latency: #1=11200ms, #2=5800ms, #3=7000ms, #4=6200ms, #5=6400ms, #6=6800ms, #7=5200ms, #8=4500ms, #9=4500ms, #10=12500ms, #11=5100ms, #12=4200ms, #13=7300ms, #14=10600ms, #15=9100ms, #16=10400ms, #17=8000ms, #18=5500ms, #19=8500ms, #20=7700ms, #21=5700ms, #22=6000ms, #23=5300ms, #24=11000ms
- `sch-extend-testpoints`: provider latency: #1=7600ms, #2=4200ms, #3=9100ms, #4=5700ms, #5=5100ms, #6=6600ms, #7=6600ms, #8=4500ms, #9=7700ms, #10=7700ms, #11=12600ms
