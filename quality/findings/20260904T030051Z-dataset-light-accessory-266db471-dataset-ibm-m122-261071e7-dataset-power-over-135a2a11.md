# Quality findings: dataset-light-accessory-266db471-dataset-ibm-m122-261071e7-dataset-power-over-135a2a11

Generated: 20260904T030051Z
Run output: /home/mimi/agent/.claude/worktrees/dsd/quality/runs

Questions:
- (none provided)

## [tool-contract]

- `dataset-light-accessory-266db471`: tool `arrange` refusal: {"error":"invalid arrange input at `intent.rails.+12V`: unknown variant `left`, expected `top` or `bottom`"}
- `dataset-light-accessory-266db471`: tool `move_symbols` refusal: {"error":"refused: dragging J4, R1 would change nets GPIO0, GPIO14, I2C_SDA, RESET; try a small 1.27 mm nudge away from other pins or wires; nothing was moved"}

## [prompt]

- `dataset-light-accessory-266db471`: loop smell: tool `get_symbol` called 4 times in a row
- `dataset-light-accessory-266db471`: cost: 442.8s elapsed, 339.9s agent, 39 provider requests
- `dataset-ibm-m122-261071e7`: loop smell: tool `check_schematic` called 3 times in a row
- `dataset-ibm-m122-261071e7`: cost: 215.7s elapsed, 71.2s agent, 9 provider requests

## [engine]

- `dataset-light-accessory-266db471`: failed check: critic_vs_reference >= 8 — actual 6
- `dataset-light-accessory-266db471`: judge: Compact the schematic and crop the sheet to eliminate the excessive blank area.
- `dataset-light-accessory-266db471`: judge: Group the power regulation, load-switch, signal-conditioning, and connector sections into a coherent left-to-right flow.
- `dataset-light-accessory-266db471`: judge: Reposition and space J1/J3/J4 so connector pin labels and references are readable without crowding.
- `dataset-light-accessory-266db471`: judge: Resolve the R1/DATA label collision and improve spacing around the R1/N$2 connection.
- `dataset-light-accessory-266db471`: judge: Add clear section headings or visual grouping for power and interface circuitry.
- `dataset-light-accessory-266db471`: schematic critic: major/spacing/Overall sheet; especially JP2, the left-side power groups, and the right-side connector bank: The schematic occupies only a shallow band near the top of an otherwise nearly empty sheet, while related sections are spread widely apart instead of being arranged as compact functional blocks.
- `dataset-light-accessory-266db471`: schematic critic: minor/congestion/J1, J3, and J4 at the far right: The three connector symbols and their adjacent pin/net text are packed into a narrow vertical region, making this area harder to scan than the otherwise open sheet.
- `dataset-ibm-m122-261071e7`: failed check: erc_errors == 0 — actual 1
- `dataset-ibm-m122-261071e7`: failed check: critic_vs_reference >= 8 — actual 7
- `dataset-ibm-m122-261071e7`: judge: ERC is not clean: one actual ERC error remains on U1.AA24/SWDCLK, with numerous singleton-net lint findings; resolve or explicitly document the unavoidable external-interface exceptions.
- `dataset-ibm-m122-261071e7`: judge: Correct the XH1 wiring through the Y2 crystal body; reroute it around the symbol for unambiguous schematic readability.
- `dataset-ibm-m122-261071e7`: judge: Recompose the sheet to reduce the extensive unused whitespace and group the MCU, clocks, RF matching network, and decoupling capacitors into compact functional blocks.
- `dataset-ibm-m122-261071e7`: judge: Reduce cramped net-label placement around U1 and standardize alignment/orientation of the repeated capacitors and inductors.
- `dataset-ibm-m122-261071e7`: schematic critic: major/orientation/C1-C16 and the clock/RF capacitor groups: The repeated shunt and decoupling capacitors are predominantly drawn horizontally as inline elements, causing them to read like series chains instead of consistent vertical taps to the common return rail.
- `dataset-ibm-m122-261071e7`: schematic critic: minor/spacing/decoupling capacitor region at upper right versus C9, C15, and C16 near the center: The decoupling bank is split into separate rows and regions rather than presented as one coherent aligned bank.
- `dataset-power-over-135a2a11`: failed check: netlist_matches_reference == true — actual false
- `dataset-power-over-135a2a11`: failed check: erc_errors == 0 — actual 6
- `dataset-power-over-135a2a11`: failed check: critic_vs_reference >= 8 — actual 6
- `dataset-power-over-135a2a11`: judge: Delivered connectivity does not match the authoritative reference netlist: R1 is on RS485_02_A/RS485_02_B, while the reference expects the corresponding termination across the 00/01 nets; net partition is 23 nets instead of 29.
- `dataset-power-over-135a2a11`: judge: KiCad ERC is not clean: six single-pin and six undriven-input errors remain on the RS485_*_RE and RS485_*_DE nets.
- `dataset-power-over-135a2a11`: judge: The rendered schematic is excessively wide and zoomed out, making references, values, and net labels difficult to read.
- `dataset-power-over-135a2a11`: judge: Repeated RS485 channels are not compactly aligned, and the long resistor/protection runs lack clear functional grouping.
- `dataset-power-over-135a2a11`: judge: The visible sheet heading is “RS485 interfaces and protection” rather than the requested sheet title “135a2a11a338”.
- `dataset-power-over-135a2a11`: schematic critic: major/spacing/entire sheet; U3/U4/U5 channel groups and J4-J6: Related parts are distributed across a very wide canvas with large empty gaps and long runs instead of being arranged as three compact RS-485 channel blocks.
- `dataset-power-over-135a2a11`: schematic critic: major/congestion/central horizontal resistor/network row around R16, R23-R26, R31-R32, and R3: Multiple long net names, resistor references, and values are packed into one extended row, so the separate channel nets are difficult to follow at normal sheet scale.
- `dataset-power-over-135a2a11`: schematic critic: minor/off-spine-leg/J4, J5, and J6 connector routes: The connector connections use large rectangular detours and are positioned farther from their associated protection networks than necessary.

## [judge]

- `dataset-light-accessory-266db471`: schematic human-look: Repack the circuit into a compact, coherent flow instead of scattering small blocks across a mostly empty sheet.
- `dataset-light-accessory-266db471`: schematic human-look: Align symbols, labels, and power sections to a consistent grid with uniform spacing and clear visual grouping.
- `dataset-light-accessory-266db471`: schematic human-look: Clean up the crowded right-side connector area by separating references, pin labels, and nearby symbols.
- `dataset-ibm-m122-261071e7`: schematic human-look: Excessive unused whitespace makes the schematic feel scattered and poorly balanced.
- `dataset-ibm-m122-261071e7`: schematic human-look: RF passives are distributed as isolated fragments instead of being arranged into compact, clearly separated functional blocks.
- `dataset-ibm-m122-261071e7`: schematic human-look: Dense pin labeling around the large IC is visually cramped while nearby sections have inconsistent spacing and alignment.
- `dataset-power-over-135a2a11`: schematic human-look: The schematic is excessively wide and zoomed out, making references, values, and net labels difficult to read.
- `dataset-power-over-135a2a11`: schematic human-look: Repeated interface sections are separated by large empty gaps rather than aligned into compact, clearly comparable blocks.
- `dataset-power-over-135a2a11`: schematic human-look: The long mid-page resistor run and scattered protection parts lack strong visual grouping or a clear drafting hierarchy.

## [self-diagnosis]

- `dataset-light-accessory-266db471`: struggled: review_schematic returned a 6/10 score and four defects but did not identify the defects or their locations.
- `dataset-light-accessory-266db471`: struggled: render_schematic reported visual findings without describing which objects or overlaps caused them.
- `dataset-light-accessory-266db471`: struggled: move_symbols refused moving J4 and R1 because nets would change, but did not explain the specific geometric conflict or safe coordinates.
- `dataset-light-accessory-266db471`: struggled: rewire added same-named pin labels and warned that routes could not be preserved cleanly, making it difficult to verify visual connectivity.
- `dataset-light-accessory-266db471`: struggled: check_schematic inconsistently reported I2C pull-up findings while later ERC output showed zero warnings.
- `dataset-light-accessory-266db471`: struggled: arrange reported several power symbols becoming disconnected even though connectivity was said to be unchanged.
- `dataset-light-accessory-266db471`: wished: review_schematic should return an actionable defect list with coordinates, affected references, and recommended fixes.
- `dataset-light-accessory-266db471`: wished: render_schematic should describe every visual finding with the overlapping or clipped items and coordinates.
- `dataset-light-accessory-266db471`: wished: move_symbols should provide the obstructing pin or wire and a suggested safe displacement when refusing an operation.
- `dataset-light-accessory-266db471`: wished: rewire should expose a before-and-after connectivity table for every moved reference and label it creates.
- `dataset-light-accessory-266db471`: wished: check_schematic should distinguish advisory diagnostics from ERC findings consistently in its summary.
- `dataset-light-accessory-266db471`: wished: arrange should guarantee that moving grouped connectors preserves power-symbol connectivity or report exactly which connections changed.
- `dataset-ibm-m122-261071e7`: struggled: check_schematic treated legitimate single-pin external interface nets as blocking errors even though the exact netlist required preserving them.
- `dataset-ibm-m122-261071e7`: struggled: The tool reported contradictory counts and summaries, including 39, 209, and 34 errors/findings, making validation difficult.
- `dataset-ibm-m122-261071e7`: struggled: ERC flagged SWDCLK as undriven without a way to declare an intentionally external or passive interface pin.
- `dataset-ibm-m122-261071e7`: struggled: The schematic rendered successfully, but the final tool status still reported an ERC error and provided no actionable repair path.
- `dataset-ibm-m122-261071e7`: wished: Add an explicit external-interface or intentionally-single-pin net annotation that preserves exact netlist fidelity while suppressing inappropriate lint errors.
- `dataset-ibm-m122-261071e7`: wished: Provide a structured, consistent diagnostic summary with separate counts for blocking errors, advisory warnings, and accepted external nets.
- `dataset-ibm-m122-261071e7`: wished: Allow pin electrical-type overrides or ERC exclusions for intentionally undriven external connections without changing parts or nets.
- `dataset-ibm-m122-261071e7`: wished: Return a schematic preview or render-quality diagnostics so layout and labeling can be verified directly.
- `dataset-power-over-135a2a11`: struggled: The schematic checker treated intentionally single-pin RE/DE nets as errors even though the netlist explicitly assigned them to only one pin.
- `dataset-power-over-135a2a11`: struggled: ERC reported undriven-input errors for pins whose connectivity could not be changed without violating the exact netlist.
- `dataset-power-over-135a2a11`: struggled: The library-symbol mismatch warnings for MAX3485 were unclear because the requested lib_id was correct but the embedded symbol copy differed.
- `dataset-power-over-135a2a11`: struggled: The checker exposed no actionable fixes for the reported ERC errors, requiring manual interpretation of whether they were acceptable.
- `dataset-power-over-135a2a11`: wished: Add a way to mark intentional single-pin nets as accepted without changing connectivity.
- `dataset-power-over-135a2a11`: wished: Provide a netlist-fidelity-aware ERC mode that suppresses undriven-input errors when exact connectivity is required.
- `dataset-power-over-135a2a11`: wished: Clarify whether lib_symbol_mismatch affects schematic correctness or is only a library cache/version warning.
- `dataset-power-over-135a2a11`: wished: Provide a concise rendered preview or validation report confirming every symbol value, lib_id, pin mapping, and sheet title.

## [variance]

- `dataset-light-accessory-266db471`: provider latency: #1=3900ms, #2=1800ms, #3=12100ms, #4=4500ms, #5=5300ms, #6=2600ms, #7=2600ms, #8=3700ms, #9=4300ms, #10=5900ms, #11=2800ms, #12=5500ms, #13=40200ms, #14=40300ms, #15=48100ms, #16=7400ms, #17=5000ms, #18=3400ms, #19=3000ms, #20=35100ms, #21=40100ms, #22=41100ms, #23=5000ms, #24=8400ms, #25=2500ms, #26=8700ms, #27=2600ms, #28=4100ms, #29=3400ms, #30=6800ms, #31=5900ms, #32=2700ms, #33=3000ms, #34=11400ms, #35=3400ms, #36=43500ms, #37=40900ms, #38=46300ms, #39=4300ms
- `dataset-ibm-m122-261071e7`: provider latency: #1=3000ms, #2=1600ms, #3=20000ms, #4=3600ms, #5=9500ms, #6=3000ms, #7=9800ms, #8=4400ms, #9=5400ms
- `dataset-power-over-135a2a11`: cost: 165.9s elapsed, 56.2s agent, 10 provider requests
- `dataset-power-over-135a2a11`: provider latency: #1=4500ms, #2=2800ms, #3=12300ms, #4=3400ms, #5=6300ms, #6=5300ms, #7=1900ms, #8=6000ms, #9=2600ms, #10=4300ms
