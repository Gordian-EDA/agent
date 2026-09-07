//! Demo-backed contract tests for the schematic query text formats.

use std::path::Path;

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

fn demo() -> Option<AgentRuntime> {
    let ctx = AgentRuntime::detect_for_test()?;
    let input =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/cases/sch-replace-ic/input");
    std::fs::copy(input.join("design.kicad_sch"), ctx.sch_path()).unwrap();
    std::fs::copy(
        input.join("design.kicad_pro"),
        ctx.sch_path().with_extension("kicad_pro"),
    )
    .unwrap();
    Some(ctx)
}

fn text(ctx: &AgentRuntime, tool: &str, input: Value) -> String {
    match gordian_tools_sch::run(tool, input, ctx)
        .unwrap_or_else(|| panic!("`{tool}` is not a schematic tool"))
        .unwrap_or_else(|error| panic!("`{tool}` failed: {error}"))
    {
        Value::String(text) => text,
        other => panic!("`{tool}` returned {other}"),
    }
}

fn parts(text: &str) -> &str {
    text.split_once("\nPOWER SYMBOLS")
        .unwrap()
        .0
        .split_once("\nPARTS")
        .unwrap()
        .1
}

#[test]
fn read_schematic_groups_and_sorts_the_demo() {
    let Some(ctx) = demo() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let compact = text(&ctx, "read_schematic", json!({}));
    let references: Vec<&str> = parts(&compact)
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty() && !line.starts_with(' '))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        references,
        [
            "C1", "C2", "P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "R1", "R2", "R3", "R4",
            "U1",
        ]
    );
    assert_eq!(
        compact
            .lines()
            .filter(|line| line.starts_with("U1 "))
            .count(),
        1
    );
    assert!(compact.contains("U1  ECC83"));
    assert!(compact.contains("3 units\n  unit 1"));
    assert_eq!(
        compact
            .lines()
            .filter(|line| line.starts_with("  unit "))
            .count(),
        3
    );
    assert!(compact.contains("POWER SYMBOLS  GND ×7  PWR_FLAG ×2"));
    assert!(
        compact
            .lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .all(|line| line.contains("uuid=")),
        "{compact}"
    );
    assert!(compact.contains("\nLABELS  (scope  name  @x,y  uuid)\n"));
    let gnd = compact
        .lines()
        .find(|line| line.starts_with("GND "))
        .unwrap();
    assert!(
        gnd.trim_start_matches("GND")
            .trim_start()
            .starts_with("(8 power symbols)")
    );

    let full = text(&ctx, "read_schematic", json!({"detail": "full"}));
    let power = full.lines().find(|line| line.contains("#PWR01")).unwrap();
    assert!(power.contains('@'), "{power}");

    let region = text(
        &ctx,
        "read_schematic",
        json!({"region": [85.0, 56.0, 87.0, 58.0]}),
    );
    let regional_parts = parts(&region);
    assert!(regional_parts.lines().any(|line| line.starts_with("C1 ")));
    assert!(!regional_parts.lines().any(|line| line.starts_with("C2 ")));
    assert!(!regional_parts.lines().any(|line| line.starts_with("U1 ")));
    assert!(region.contains("NETS  (name: pins; power symbols counted, not listed)"));
}

#[test]
fn focused_lookups_are_aligned_plain_text() {
    let Some(ctx) = demo() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let symbol = text(&ctx, "read_schematic", json!({"ref": "U1"}));
    assert!(symbol.starts_with("U1  ECC83  ecc83-pp:ECC83  3 units"));
    assert!(symbol.contains("pin  name  type     side"));
    assert_eq!(
        symbol
            .lines()
            .filter(|line| line.starts_with("unit "))
            .count(),
        3
    );
    let pin_rows = symbol
        .lines()
        .filter(|line| {
            line.strip_prefix("  ")
                .and_then(|line| line.split_whitespace().next())
                .is_some_and(|pin| pin.chars().all(|character| character.is_ascii_digit()))
        })
        .count();
    assert_eq!(pin_rows, 9, "{symbol}");

    let net = text(&ctx, "read_schematic", json!({"net": "GND"}));
    assert!(net.starts_with("NET GND  7 pins  named by: power symbol\n"));
    let pins: Vec<&str> = net
        .lines()
        .skip(1)
        .take_while(|line| *line != "LABELS")
        .collect();
    assert_eq!(pins.len(), 7, "{net}");
    assert!(pins.iter().all(|line| !line.starts_with('#')));
}
