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
fn footprint_tool_schema_requires_a_symbol() {
    let definition = gordian_core::tools::tool_defs()
        .into_iter()
        .find(|tool| tool.name.to_string() == "search_footprints")
        .expect("search_footprints definition");
    let schema: Value = definition.schema.expect("search_footprints schema");
    assert_eq!(schema["oneOf"][0]["required"], json!(["symbol"]));
    assert!(
        definition
            .description
            .as_deref()
            .is_some_and(|description| description.contains("Pass the symbol"))
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
