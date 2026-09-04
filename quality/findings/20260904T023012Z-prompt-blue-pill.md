# Quality findings: prompt-blue-pill

Generated: 20260904T023012Z
Run output: /tmp/claude-1000/-home-mimi-agent/859b05e2-ce70-455d-87fa-c4a8f253c33c/scratchpad/bp/runs

Questions:
- how does the BluePill sheet compare with the sch-agent reference now?

## [tool-contract]

- `prompt-blue-pill`: tool `arrange` refusal: {"error":"invalid arrange input at `intent.rails.+5V`: unknown variant `left`, expected `top` or `bottom`"}
- `prompt-blue-pill`: tool `connect` refusal: {"connected":[{"did_you_mean":{"U2.24":["U2.24","U2.2","U2.ADC2_IN4","U2.TIM2_CH4","U2.4"],"U2.VDD":["U2.VDD","U2.VDDA"]},"error":"`U2` has 3 pins named `VDD` (24, 36, 48); use the pin number","from":"U2.24","to":"U2.VDD"},{"did_you_mean":{"U2.36":["U2.36","U2.3","U2.6"],"U2.VDD":["U2.VDD","U2.VDDA"]},"error":"`U2` has 3 pins named `VDD` (24, 36, 48); use the pin number","from":"U2.36","to":"U2.VDD"},{"did_you_mean":{"U2.48":["U2.48","U2.4","U2.8"],"U2.VDD":["U2.VDD","U2.VDDA"]},"error":"`U2` has 3 pins named `VDD` (24, 36, 48); use the pin number","from":"U2.48","to":"U2.VDD"}],"error":"all 3 connections failed — U2.24 -> U2.VDD: `U2` has 3 pins named `VDD` (24, 36, 48); use the pin number; U2.36 -> U2.VDD: `U2` has 3 pins named `VDD` (24, 36, 48); use the pin number; U2.48 -> U2.VDD: `U2` has 3 pins named `VDD` (24, 36, 48); use the pin number"}
- `prompt-blue-pill`: tool `connect` refusal: {"connected":[{"error":"refused: the edit would change connectivity the call did not name (N_U2_43_J2_18, Net-(J2-Pin_15), Net-(J2-Pin_16)); nothing was written","from":"U2.43","net_delta":{"merged":[[["Net-(J2-Pin_15)","Net-(J2-Pin_16)"],"N_U2_43_J2_18"]],"now_connected":["J2.18","U2.43"]},"to":"J2.18"}],"error":"all 1 connections failed — U2.43 -> J2.18: refused: the edit would change connectivity the call did not name (N_U2_43_J2_18, Net-(J2-Pin_15), Net-(J2-Pin_16)); nothing was written"}
- `prompt-blue-pill`: tool `connect` refusal: {"error":"refused: the edit would change connectivity the call did not name (Net-(J2-Pin_15), Net-(J2-Pin_16)); nothing was written","net_delta":{"merged":[[["Net-(J2-Pin_15)","Net-(J2-Pin_16)"],"PB7"]],"now_connected":["J2.18","U2.43"]}}
- `prompt-blue-pill`: tool `connect` refusal: {"error":"refused: the edit would change connectivity the call did not name (Net-(J2-Pin_17), Net-(J3-Pin_1)); nothing was written","net_delta":{"merged":[[["Net-(J2-Pin_17)","Net-(J3-Pin_1)"],"PB5"]],"now_connected":["J2.16"]}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"U2.25 is already on net `N_U2_25_J3_3`; a label does not replace that name, it merges `N_U2_25_J3_3` and `PB12` into one net. Use delete_wires to take U2.25 off `N_U2_25_J3_3` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.25"]},"tool":"delete_wires"}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"U2.26 is already on net `N_U2_26_J3_4`; a label does not replace that name, it merges `N_U2_26_J3_4` and `PB13` into one net. Use delete_wires to take U2.26 off `N_U2_26_J3_4` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.26"]},"tool":"delete_wires"}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"U2.27 is already on net `N_U2_27_J3_5`; a label does not replace that name, it merges `N_U2_27_J3_5` and `PB14` into one net. Use delete_wires to take U2.27 off `N_U2_27_J3_5` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.27"]},"tool":"delete_wires"}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"U2.28 is already on net `N_U2_28_J3_6`; a label does not replace that name, it merges `N_U2_28_J3_6` and `PB15` into one net. Use delete_wires to take U2.28 off `N_U2_28_J3_6` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.28"]},"tool":"delete_wires"}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"U2.3 is already on net `LSE_IN`; a label does not replace that name, it merges `LSE_IN` and `PC14` into one net. Use delete_wires to take U2.3 off `LSE_IN` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.3"]},"tool":"delete_wires"}}
- `prompt-blue-pill`: tool `label` refusal: {"error":"U2.4 is already on net `LSE_OUT`; a label does not replace that name, it merges `LSE_OUT` and `PC15` into one net. Use delete_wires to take U2.4 off `LSE_OUT` first, or name a pin that is loose.","fix":{"args":{"pins":["U2.4"]},"tool":"delete_wires"}}
- `prompt-blue-pill`: tool `place_parts` refusal: {"error":"invalid place_parts input at `layout.interfaces`: a layout node is exactly one of `part`, `row` or `col`"}

## [prompt]

- `prompt-blue-pill`: loop smell: tool `arrange` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `rewire` called 6 times in a row
- `prompt-blue-pill`: loop smell: tool `connect` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `get_symbol` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `connect` called 5 times in a row
- `prompt-blue-pill`: loop smell: tool `get_symbol` called 3 times in a row
- `prompt-blue-pill`: loop smell: tool `connect` called 19 times in a row
- `prompt-blue-pill`: loop smell: tool `label` called 21 times in a row
- `prompt-blue-pill`: loop smell: tool `label` called 7 times in a row
- `prompt-blue-pill`: loop smell: tool `place_parts` called 5 times in a row
- `prompt-blue-pill`: cost: 1310.5s elapsed, 1135.2s agent, 145 provider requests

## [judge]

- `prompt-blue-pill`: judge: Reconnect the GPIO breakout headers: the delivered KiCad netlist leaves most J2/J3 pins and many STM32 GPIO pads on unconnected nets, so the requested GPIO breakout is not functional.
- `prompt-blue-pill`: judge: Reconnect USB connector pins J1.2 (D−) and J1.3 (D+); both are unconnected in the delivered netlist, so USB data cannot work.
- `prompt-blue-pill`: judge: Correct and verify the USB resistor topology: the 1.5 kOhm pull-up and series resistors are split across unnamed nets and do not form a valid connector-to-PA11/PA12 path.
- `prompt-blue-pill`: judge: Do not rely on the clean ERC result: the final schematic still contains extensive isolated GPIO and connector pins despite ERC reporting zero errors.
- `prompt-blue-pill`: judge: Tighten and reorganize the schematic; the final drawing has excessive unused whitespace, scattered small components/labels, and weak alignment and section hierarchy.
- `prompt-blue-pill`: judge: Assign footprints for JP1 and JP2 before considering the design fabrication-ready.
- `prompt-blue-pill`: schematic human-look: Excessive unused whitespace makes the schematic feel stretched rather than deliberately composed.
- `prompt-blue-pill`: schematic human-look: Several small symbols, labels, and notes are scattered with inconsistent spacing and alignment around the MCU and interface blocks.
- `prompt-blue-pill`: schematic human-look: Section boxes and annotations vary in scale and placement, weakening the page’s visual hierarchy.

## [self-diagnosis]

- `prompt-blue-pill`: struggled: check_schematic reported checks.errors=0 while simultaneously reporting ERC errors=2 and ok=false, which was confusing.
- `prompt-blue-pill`: struggled: The tool exposed fixes for stale power-symbol endpoints but did not provide a direct apply-fixes operation, requiring manual diagnosis and symbol removal.
- `prompt-blue-pill`: struggled: Repeated provider/tool calls and schematic checks made the task unnecessarily slow.
- `prompt-blue-pill`: struggled: render_schematic reported no visual findings but did not provide structured verification that every requested component and connection was visible.
- `prompt-blue-pill`: struggled: The final cleanup removed duplicate power symbols rather than clearly identifying why they were disconnected or preserving them through explicit net connections.
- `prompt-blue-pill`: struggled: No concise netlist or component inventory was available to independently verify GPIO breakout completeness and USB pull-up connectivity.
- `prompt-blue-pill`: wished: Add an apply_erc_fixes tool that accepts the fixes returned by check_schematic.
- `prompt-blue-pill`: wished: Make check_schematic use consistent error fields and clearly distinguish ERC findings from general check counts.
- `prompt-blue-pill`: wished: Provide a compact schematic summary listing components, pins, nets, and unconnected endpoints.
- `prompt-blue-pill`: wished: Add targeted queries such as get_component, get_pin, and get_connection for validating requested circuitry.
- `prompt-blue-pill`: wished: Automatically rerender after schematic edits and return the final render path alongside ERC status.
- `prompt-blue-pill`: wished: Provide a visual or structured completeness check mapped directly to each user requirement.

## [variance]

- `prompt-blue-pill`: provider latency: #1=5200ms, #2=2800ms, #3=3400ms, #4=4500ms, #5=8900ms, #6=3300ms, #7=3200ms, #8=6800ms, #9=2700ms, #10=3800ms, #11=13000ms, #12=3600ms, #13=7100ms, #14=4100ms, #15=6700ms, #16=1800ms, #17=6000ms, #18=7600ms, #19=9000ms, #20=5500ms, #21=4400ms, #22=3200ms, #23=3000ms, #24=5100ms, #25=4600ms, #26=12600ms, #27=6500ms, #28=4000ms, #29=33100ms, #30=48100ms, #31=38100ms, #32=5100ms, #33=5700ms, #34=3500ms, #35=3900ms, #36=3500ms, #37=7600ms, #38=50600ms, #39=38800ms, #40=43300ms, #41=6000ms, #42=3200ms, #43=6600ms, #44=48400ms, #45=42500ms, #46=52600ms, #47=7200ms, #48=3500ms, #49=2200ms, #50=7700ms, #51=4400ms, #52=2500ms, #53=4300ms, #54=4200ms, #55=1900ms, #56=5000ms, #57=3900ms, #58=4000ms, #59=6400ms, #60=5200ms, #61=3100ms, #62=2600ms, #63=3100ms, #64=5300ms, #65=3100ms, #66=2600ms, #67=2200ms, #68=4900ms, #69=49600ms, #70=52100ms, #71=46200ms, #72=4900ms, #73=6800ms, #74=9100ms, #75=6300ms, #76=2700ms, #77=8000ms, #78=6200ms, #79=7500ms, #80=4600ms, #81=5800ms, #82=5500ms, #83=2800ms, #84=7600ms, #85=3300ms, #86=3800ms, #87=6000ms, #88=6100ms, #89=8200ms, #90=7800ms, #91=6400ms, #92=7000ms, #93=3200ms, #94=5700ms, #95=2400ms, #96=3600ms, #97=7900ms, #98=6900ms, #99=4600ms, #100=2800ms, #101=4600ms, #102=2600ms, #103=3400ms, #104=5000ms, #105=7500ms, #106=7000ms, #107=3000ms, #108=3700ms, #109=8100ms, #110=9600ms, #111=4000ms, #112=3300ms, #113=3400ms, #114=3300ms, #115=2600ms, #116=5700ms, #117=7800ms, #118=10300ms, #119=5300ms, #120=3700ms, #121=3100ms, #122=8200ms, #123=10800ms, #124=9900ms, #125=11300ms, #126=7700ms, #127=5200ms, #128=2700ms, #129=5900ms, #130=3000ms, #131=8100ms, #132=42200ms, #133=48200ms, #134=42600ms, #135=10800ms, #136=4300ms, #137=4200ms, #138=4400ms, #139=3300ms, #140=9300ms, #141=4700ms, #142=3800ms, #143=5300ms, #144=3800ms, #145=3400ms
