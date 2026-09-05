//! A `connect` too long to draw as a wire is joined by name instead. The name it
//! invents is the one a person would have written — the MCU's own pin name, not
//! `N_J1_1_U1_42`.
//!
//! Skips when no KiCAD is installed: the mutators embed library definitions.

use gordian_runtime::AgentRuntime;
use sch_doc::SchDoc;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"9.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000cd\")\n\
\t(paper \"A2\")\n\
\t(lib_symbols)\n\
\t(sheet_instances\n\
\t\t(path \"/\"\n\
\t\t\t(page \"1\")\n\
\t\t)\n\
\t)\n\
)\n";

fn sheet() -> Option<AgentRuntime> {
    let ctx = AgentRuntime::detect_for_test()?;
    std::fs::write(ctx.sch_path(), EMPTY_SHEET).unwrap();
    Some(ctx)
}

fn call(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    gordian_tools_sch::run(name, input, ctx)
        .unwrap_or_else(|| panic!("`{name}` is not a schematic tool"))
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"))
}

#[test]
fn a_join_by_name_is_named_after_the_mcu_pin_it_joins() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let added = call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "MCU_ST_STM32F1:STM32F103C8Tx", "ref": "U1"},
            {"lib_id": "Connector_Generic:Conn_01x02", "ref": "J1"},
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    // Far enough apart that a wire between them would read as a line across the
    // page, which is what makes `connect` fall back to a pair of labels.
    let moved = call(
        &ctx,
        "move_symbols",
        json!({"moves": [{"ref": "J1", "to": [400.0, 250.0]}]}),
    );
    assert!(moved.get("error").is_none(), "{moved}");

    let joined = call(&ctx, "connect", json!({"from": "U1.PB6", "to": "J1.1"}));
    assert!(joined.get("error").is_none(), "{joined}");

    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    let labels: Vec<String> = doc.labels().map(|l| sch_doc::unescape(&l.text)).collect();
    assert!(labels.contains(&"PB6".to_string()), "labels: {labels:?}");
}
