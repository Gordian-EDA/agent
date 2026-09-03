use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use serde_json::{Value, json};

#[test]
fn footprint_search_returns_compatible_pads_before_exact_incompatible_text() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let result = run_tool(
        "search_footprints",
        json!({
            "symbol": "Connector:Barrel_Jack",
            "query": "Connector_BarrelJack:BarrelJack_Horizontal",
            "limit": 25
        }),
        &ctx,
    )
    .unwrap();
    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits[0]["compatible"], true);
    assert!(
        hits[0]["pads"]
            .as_array()
            .is_some_and(|pads| !pads.is_empty())
    );
    let exact = hits
        .iter()
        .position(|hit| hit["lib_id"] == "Connector_BarrelJack:BarrelJack_Horizontal")
        .expect("exact incompatible text match remains visible");
    assert!(hits[..exact].iter().all(|hit| hit["compatible"] == true));
    assert_eq!(hits[exact]["compatible"], false);
    assert_eq!(hits[exact]["pads"], json!(["1", "2", "3"]));
}

#[test]
fn top_symbol_hit_has_a_validated_compatible_footprint() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let result = run_tool(
        "search_symbols",
        json!({"query": "Connector:Barrel_Jack", "limit": 3}),
        &ctx,
    )
    .unwrap();
    let top = &result["hits"][0];
    let symbol = top["lib_id"].as_str().unwrap();
    let footprint = top["footprint"]
        .as_str()
        .expect("top discovery hit has no footprint");
    let verdict =
        gordian_runtime::footprint_compat::footprint_compatibility(&ctx, symbol, footprint)
            .unwrap();
    assert!(verdict.compatible, "discovery returned {top:#}");
    assert!(top["pins"].as_array().is_some_and(|pins| !pins.is_empty()));
}

#[test]
fn footprint_tool_schema_accepts_a_query_without_a_symbol() {
    let definition = gordian_core::tools::tool_defs()
        .into_iter()
        .find(|tool| tool.name.to_string() == "search_footprints")
        .expect("search_footprints definition");
    let schema: Value = definition.schema.expect("search_footprints schema");
    assert_eq!(schema["anyOf"][1]["required"], json!(["query"]));
    assert!(
        definition
            .description
            .as_deref()
            .is_some_and(|description| description.contains("without it"))
    );
}

#[test]
fn footprint_search_rejects_an_unknown_symbol() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let error = run_tool(
        "search_footprints",
        json!({"symbol": "Missing:Definitely_Not_A_Symbol"}),
        &ctx,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown symbol"));
}

#[test]
fn symbol_info_exposes_only_a_validated_footprint() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let result = run_tool(
        "get_symbol_info",
        json!({"lib_id": "Connector:Barrel_Jack"}),
        &ctx,
    )
    .unwrap();
    assert!(result.get("default_footprint").is_none());
    let footprint = result["footprint"].as_str().unwrap();
    let verdict = gordian_runtime::footprint_compat::footprint_compatibility(
        &ctx,
        "Connector:Barrel_Jack",
        footprint,
    )
    .unwrap();
    assert!(verdict.compatible);
}

#[test]
fn symbol_info_recognizes_a_pin_header_footprint_name() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };

    let result = run_tool(
        "get_symbol_info",
        json!({
            "lib_id": "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical"
        }),
        &ctx,
    )
    .unwrap();

    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| { error.contains("is a FOOTPRINT name, not a symbol") })
    );
    assert_eq!(
        result["suggestions"],
        json!(["Connector_Generic:Conn_01x06"]),
        "{result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("Did you mean `Connector_Generic:Conn_01x06`?"),
        "{result}"
    );
}
