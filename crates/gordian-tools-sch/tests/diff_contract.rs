//! Oracle-backed contract tests for `diff_schematic`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use gordian_runtime::AgentRuntime;
use sch_doc::{SchDoc, SymbolInst};
use serde_json::{Value, json};

fn passive_fixture() -> Option<AgentRuntime> {
    let ctx = AgentRuntime::detect_for_test()?;
    let input =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/cases/sch-replace-passive/input");
    std::fs::copy(input.join("design.kicad_sch"), ctx.sch_path()).unwrap();
    std::fs::copy(
        input.join("design.kicad_pro"),
        ctx.sch_path().with_extension("kicad_pro"),
    )
    .unwrap();
    Some(ctx)
}

fn tool(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    gordian_tools_sch::run(name, input, ctx)
        .unwrap_or_else(|| panic!("{name} is not registered"))
        .unwrap_or_else(|error| panic!("{name} failed: {error}"))
}

fn symbol_json(symbol: &SymbolInst) -> Value {
    json!({
        "key": format!("{}/{}", symbol.refdes(), symbol.unit),
        "uuid": symbol.uuid,
        "lib_id": symbol.lib_id,
        "x": symbol.at.x,
        "y": symbol.at.y,
        "rot": symbol.at.rot,
        "mirror": format!("{:?}", symbol.mirror),
        "fields": symbol.fields.iter().map(|(name, field)| (name.clone(), json!(field.value))).collect::<serde_json::Map<_, _>>(),
    })
}

fn facts(doc: &SchDoc) -> Value {
    json!({ "symbols": doc.symbols().map(symbol_json).collect::<Vec<_>>() })
}

fn harness_compare(before: &SchDoc, after: &SchDoc) -> Value {
    let run_py = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/run.py");
    let script = format!(
        "import importlib.util,json,sys\n\
         spec=importlib.util.spec_from_file_location('quality_run',{})\n\
         module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)\n\
         before,after=json.load(sys.stdin)\n\
         json.dump(module.compare_symbols(before,after),sys.stdout)",
        serde_json::to_string(&run_py).unwrap()
    );
    let mut child = Command::new("python3")
        .args(["-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start quality harness oracle");
    serde_json::to_writer(
        child.stdin.as_mut().expect("oracle stdin"),
        &json!([facts(before), facts(after)]),
    )
    .unwrap();
    child.stdin.take().unwrap().flush().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn diff_agrees_with_quality_harness_after_field_and_pose_edits() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    ctx.begin_turn().unwrap();
    let before = SchDoc::read(ctx.sch_path()).unwrap();
    let fields = tool(
        &ctx,
        "set_fields",
        json!({"ref": "R1", "fields": {"Value": "2.2K"}}),
    );
    assert!(fields.get("error").is_none(), "{fields}");
    let moved = tool(
        &ctx,
        "move_symbols",
        json!({"moves": [{"ref": "R1", "by": [7.62, 0.0]}]}),
    );
    assert!(moved.get("error").is_none(), "{moved}");
    let after = SchDoc::read(ctx.sch_path()).unwrap();

    let diff = tool(&ctx, "diff_schematic", json!({"detail": true}));
    let oracle = harness_compare(&before, &after);
    assert_eq!(diff["added"], oracle["symbols_added"]);
    assert_eq!(diff["removed"], oracle["symbols_removed"]);
    assert_eq!(
        json!(
            diff["moved"]
                .as_array()
                .unwrap()
                .iter()
                .map(|change| change["ref"].clone())
                .collect::<Vec<_>>()
        ),
        oracle["unchanged_symbols_moved"]
    );
    assert_eq!(
        json!(
            diff["fields_changed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|change| json!([change["ref"], change["field"], change["from"], change["to"]]))
                .collect::<Vec<_>>()
        ),
        oracle["fields_changed"]
    );
    assert_eq!(
        json!(
            diff["swapped"]
                .as_array()
                .unwrap()
                .iter()
                .map(|change| json!([change["ref"], change["from"], change["to"]]))
                .collect::<Vec<_>>()
        ),
        oracle["lib_ids_changed"]
    );

    let compact = tool(&ctx, "diff_schematic", json!({}));
    let text = compact.as_str().expect("compact diff text");
    assert!(text.contains("MOVED"), "{text}");
    assert!(text.contains("R1/1"), "{text}");
    assert!(text.contains("FIELDS CHANGED"), "{text}");
    assert!(text.contains("\"1.5K\" → \"2.2K\""), "{text}");
}
