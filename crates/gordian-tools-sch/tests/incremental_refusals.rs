//! Regression coverage for edits that state their connectivity intent.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000cc\")\n\
\t(paper \"A4\")\n\
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
        .unwrap_or_else(|error| panic!("`{name}` failed: {error}"))
}

fn add_resistors(ctx: &AgentRuntime, refs: &[&str]) {
    let parts = refs
        .iter()
        .map(|reference| json!({"lib_id": "Device:R", "ref": reference}))
        .collect::<Vec<_>>();
    let result = call(ctx, "add_symbols", json!({"parts": parts}));
    assert!(result.get("error").is_none(), "fixture failed: {result}");
}

#[test]
fn connect_joins_derived_endpoints_but_not_two_authored_nets() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3", "R4", "R5"]);

    let seeded = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let named = call(&ctx, "label", json!({"pin": "R3.1", "net": "3V3"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let joined = call(&ctx, "connect", json!({"from": "R3.1", "to": "R1.2"}));
    assert!(joined.get("error").is_none(), "{joined}");
    assert_eq!(joined["joined"]["from_net"], "3V3", "{joined}");
    assert!(
        joined["joined"]["to_net"]
            .as_str()
            .is_some_and(|net| net.starts_with("Net-(")),
        "{joined}"
    );
    assert_eq!(joined["joined"]["survivor"], "3V3", "{joined}");
    let net = call(&ctx, "get_net", json!({"name": "3V3"})).to_string();
    for pin in ["R1.2", "R2.1", "R3.1"] {
        assert!(net.contains(pin), "{pin} absent from joined net: {net}");
    }

    for (pin, net) in [("R4.1", "SDA"), ("R5.1", "SCL")] {
        let result = call(&ctx, "label", json!({"pin": pin, "net": net}));
        assert!(result.get("error").is_none(), "fixture failed: {result}");
    }
    let refused = call(&ctx, "connect", json!({"from": "R4.1", "to": "R5.1"}));
    let error = refused["error"].as_str().unwrap_or_default();
    assert!(error.contains("SDA") && error.contains("SCL"), "{refused}");
    assert_eq!(
        refused["fix"],
        json!({"tool": "delete_wires", "args": {"pins": ["R5.1"]}})
    );
    let sda = call(&ctx, "get_net", json!({"name": "SDA"})).to_string();
    let scl = call(&ctx, "get_net", json!({"name": "SCL"})).to_string();
    assert!(sda.contains("R4.1") && !sda.contains("R5.1"), "{sda}");
    assert!(scl.contains("R5.1") && !scl.contains("R4.1"), "{scl}");
}

#[test]
fn connect_accepts_a_net_name_as_either_endpoint() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3"]);
    let named = call(&ctx, "label", json!({"pin": "R1.1", "net": "VREF"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let existing = call(&ctx, "connect", json!({"from": "R2.1", "to": "VREF"}));
    assert!(existing.get("error").is_none(), "{existing}");
    assert_eq!(existing["net_endpoint"]["existed"], true, "{existing}");
    let created = call(&ctx, "connect", json!({"from": "NEW_SENSE", "to": "R3.1"}));
    assert!(created.get("error").is_none(), "{created}");
    assert_eq!(created["net_endpoint"]["created"], true, "{created}");

    let vref = call(&ctx, "get_net", json!({"name": "VREF"})).to_string();
    assert!(vref.contains("R1.1") && vref.contains("R2.1"), "{vref}");
    let sense = call(&ctx, "get_net", json!({"name": "NEW_SENSE"})).to_string();
    assert!(sense.contains("R3.1"), "{sense}");
}
