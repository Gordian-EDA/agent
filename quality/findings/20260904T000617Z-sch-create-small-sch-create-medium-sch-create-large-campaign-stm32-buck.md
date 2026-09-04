# Quality findings: sch-create-small-sch-create-medium-sch-create-large-campaign-stm32-buck

Generated: 20260904T000617Z
Run output: /home/mimi/agent/.claude/worktrees/trees/quality/runs/trees

Questions:
- (none provided)

## [engine]

- `sch-create-small`: failed check: agent_exit == 0 — actual 1
- `sch-create-small`: failed check: schematic_created == true — actual false
- `sch-create-small`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `sch-create-small`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `sch-create-small`: judge: No schematic was created or rendered.
- `sch-create-small`: judge: Agent exited with status 1 before making any tool calls.
- `sch-create-small`: judge: Required NPN low-side driver, resistor network, LED, and wired connectors were not delivered.
- `sch-create-small`: judge: ERC validation was not run.
- `sch-create-medium`: failed check: agent_exit == 0 — actual 1
- `sch-create-medium`: failed check: schematic_created == true — actual false
- `sch-create-medium`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `sch-create-medium`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `sch-create-medium`: judge: No schematic was created or rendered.
- `sch-create-medium`: judge: The required CAN transceiver interface, including termination, TVS protection, connectors, decoupling, and at least 18 non-power parts, was not delivered.
- `sch-create-medium`: judge: ERC/DRC and connectivity validation were not run.
- `sch-create-large`: failed check: agent_exit == 0 — actual 1
- `sch-create-large`: failed check: schematic_created == true — actual false
- `sch-create-large`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `sch-create-large`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `sch-create-large`: judge: No schematic was created or rendered.
- `sch-create-large`: judge: The requested USB-C power, protection, regulator, MCU, sensor, clock, SWD, reset, boot, decoupling, and LED blocks were not implemented.
- `sch-create-large`: judge: Connectivity, ERC, and schematic completeness could not be verified.
- `campaign-stm32-buck`: failed check: agent_exit == 0 — actual 1
- `campaign-stm32-buck`: failed check: schematic_created == true — actual false
- `campaign-stm32-buck`: failed check: pcb_created == true — actual false
- `campaign-stm32-buck`: failed check: len(fab_files) >= 3 — actual 0
- `campaign-stm32-buck`: failed check: schematic_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: pcb_critic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_schematic_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: failed check: human_look_pcb_score >= 8 — cannot compare None with 8
- `campaign-stm32-buck`: judge: No schematic was created or delivered.
- `campaign-stm32-buck`: judge: No PCB was created, routed, or delivered.
- `campaign-stm32-buck`: judge: ERC and DRC were not run, so zero-error compliance is unverified.
- `campaign-stm32-buck`: judge: No fabrication outputs were exported; the fab file list is empty.
- `campaign-stm32-buck`: judge: No schematic or PCB renders were produced for review.

## [harness]

- `sch-create-small`: erc_check_error: not run
- `sch-create-small`: failed check: sch_errors == [] — not measured: sch_errors
- `sch-create-small`: failed check: erc_errors == 0 — not measured: erc_errors
- `sch-create-small`: failed check: partition_matches_kicad == true — not measured: partition_matches_kicad
- `sch-create-small`: failed check: part_count >= 8 — not measured: part_count
- `sch-create-small`: failed check: extractor_warnings == [] — not measured: extractor_warnings
- `sch-create-small`: failed check: text_collisions == [] — not measured: text_collisions
- `sch-create-medium`: erc_check_error: not run
- `sch-create-medium`: failed check: sch_errors == [] — not measured: sch_errors
- `sch-create-medium`: failed check: erc_errors == 0 — not measured: erc_errors
- `sch-create-medium`: failed check: partition_matches_kicad == true — not measured: partition_matches_kicad
- `sch-create-medium`: failed check: part_count >= 18 — not measured: part_count
- `sch-create-medium`: failed check: extractor_warnings == [] — not measured: extractor_warnings
- `sch-create-medium`: failed check: text_collisions == [] — not measured: text_collisions
- `sch-create-large`: erc_check_error: not run
- `sch-create-large`: failed check: sch_errors == [] — not measured: sch_errors
- `sch-create-large`: failed check: erc_errors == 0 — not measured: erc_errors
- `sch-create-large`: failed check: partition_matches_kicad == true — not measured: partition_matches_kicad
- `sch-create-large`: failed check: part_count >= 35 — not measured: part_count
- `sch-create-large`: failed check: extractor_warnings == [] — not measured: extractor_warnings
- `sch-create-large`: failed check: text_collisions == [] — not measured: text_collisions
- `campaign-stm32-buck`: erc_check_error: not run
- `campaign-stm32-buck`: failed check: erc_errors == 0 — not measured: erc_errors
- `campaign-stm32-buck`: failed check: drc_errors == 0 — not measured: drc_errors
- `campaign-stm32-buck`: failed check: partition_matches_kicad == true — not measured: partition_matches_kicad
- `campaign-stm32-buck`: failed check: part_count >= 50 — not measured: part_count
- `campaign-stm32-buck`: failed check: unconnected_pins == [] — not measured: unconnected_pins

## [self-diagnosis]

- `sch-create-small`: struggled: turn 1: Gordian configuration parsing failed because the unsupported `layout` field blocked tool startup.
- `sch-create-small`: struggled: turn 1: No schematic-generation or rendering tool result was available to verify the requested circuit.
- `sch-create-small`: struggled: turn 1: The error did not identify an automatic migration or supported replacement for the obsolete configuration field.
- `sch-create-small`: wished: turn 1: Gordian should ignore unknown configuration fields or provide a clear migration path.
- `sch-create-small`: wished: turn 1: A schematic tool should report created components, connections, and ERC results in a machine-readable summary.
- `sch-create-small`: wished: turn 1: A rendering tool should return the rendered schematic image or an explicit failure reason.
- `sch-create-medium`: struggled: turn 1: Gordian rejected the configuration because `layout = true` was unsupported, reporting only that `retryJson` or `ensemble` were expected.
- `sch-create-medium`: struggled: turn 1: The run produced no schematic-generation or rendering result after the configuration parse failure.
- `sch-create-medium`: wished: turn 1: Gordian should validate the configuration schema before execution and identify the file and supported replacement for deprecated options.
- `sch-create-medium`: wished: turn 1: Gordian should provide a recovery path or default configuration when an optional setting such as `layout` is invalid.
- `sch-create-medium`: wished: turn 1: The schematic tool should return a clear completion status plus an accessible rendered image and generated schematic file.
- `sch-create-large`: struggled: turn 1: Gordian rejected the configuration because it reported an unsupported `layout` field while expecting only `retryJson` or `ensemble`.
- `sch-create-large`: struggled: turn 1: The tool provided no actionable migration guidance for the configuration schema mismatch.
- `sch-create-large`: struggled: turn 1: No schematic-generation or symbol-placement tool executed after the configuration error.
- `sch-create-large`: struggled: turn 1: No rendering result was produced, so visual inspection of the requested schematic was impossible.
- `sch-create-large`: wished: turn 1: Gordian should validate configuration fields before execution and suggest the correct replacement for deprecated options.
- `sch-create-large`: wished: turn 1: Gordian should automatically ignore or migrate unsupported configuration keys when safe.
- `sch-create-large`: wished: turn 1: A schematic authoring tool should create the complete circuit from a structured component and connectivity specification.
- `sch-create-large`: wished: turn 1: A schematic verification tool should check power protection, regulator feedback, decoupling, MCU boot/reset, and unconnected pins.
- `sch-create-large`: wished: turn 1: A renderer should return an image or preview plus machine-readable errors for missing symbols or invalid connections.
- `campaign-stm32-buck`: struggled: turn 1: Gordian configuration parsing failed because the `layout` key was rejected as an unknown field.
- `campaign-stm32-buck`: struggled: turn 1: The tool provided no actionable migration guidance for the expected `retryJson`/`ensemble` configuration schema.
- `campaign-stm32-buck`: struggled: turn 1: The run stopped before schematic, PCB, ERC/DRC, rendering, or fabrication-file generation could begin.
- `campaign-stm32-buck`: wished: turn 1: Provide versioned configuration examples and automatic migration for deprecated or unsupported keys.
- `campaign-stm32-buck`: wished: turn 1: Report the valid configuration schema with line-specific remediation suggestions.
- `campaign-stm32-buck`: wished: turn 1: Allow the design task to proceed with safe defaults when an optional configuration key is invalid.
- `campaign-stm32-buck`: wished: turn 1: Provide an end-to-end verification report covering connectivity, placement, routing, ERC, DRC, and exported files.

## [variance]

- `sch-create-small`: cost: 5.3s elapsed, 0.1s agent, 0 provider requests
- `sch-create-medium`: cost: 5.4s elapsed, 0.1s agent, 0 provider requests
- `sch-create-large`: cost: 7.0s elapsed, 0.1s agent, 0 provider requests
- `campaign-stm32-buck`: cost: 6.1s elapsed, 0.1s agent, 0 provider requests
