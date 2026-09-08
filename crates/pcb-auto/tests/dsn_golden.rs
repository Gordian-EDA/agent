//! The Rust DSN writer against a DSN dumped by the Python reference (`pcbagent.route.dsn`)
//! for the same board. Exact bytes are not required — image naming order may differ — but
//! the structure the router reads has to be identical.

use std::collections::BTreeMap;

use pcb_auto::dsn::write_dsn;
use pcb_auto::model::{Board, Rules};
use pcb_auto::rules::router_net_widths;

const BOARD: &str = include_str!("fixtures/bluepill_outlined.kicad_pcb");
const GOLDEN: &str = include_str!("fixtures/bluepill.dsn");

/// The rules the golden file was dumped at — KiCad's built-in constraints, which is what the
/// Python reference defaults to. The crate now defaults to a standard fab process instead, so the
/// comparison states the rules explicitly: this test is about the WRITER, not the rule policy.
fn python_rules() -> Rules {
    Rules {
        track_width: 0.2,
        clearance: 0.2,
        via_size: 0.6,
        via_drill: 0.3,
        ..Rules::default()
    }
}

fn ours() -> String {
    let board = Board::parse(BOARD).expect("fixture board parses");
    let rules = python_rules();
    let widths: BTreeMap<String, f64> = router_net_widths(&board, &rules);
    let kicad = kicad::KicadInstallation::detect();
    let doc = write_dsn(&board, &widths, &rules, kicad.as_ref()).expect("write_dsn");
    if let Ok(p) = std::env::var("PCB_AUTO_DUMP_DSN") {
        std::fs::write(p, &doc.text).ok();
    }
    doc.text
}

/// `(head ...)` scopes at a given indent, keyed by their first line.
fn scope(text: &str, opening: &str) -> String {
    let start = text.find(opening).unwrap_or_else(|| panic!("no {opening:?}"));
    let indent = text[..start].rsplit('\n').next().unwrap_or("").len();
    let mut depth = 0i32;
    let mut out = String::new();
    for line in text[start - indent..].lines() {
        out.push_str(line);
        out.push('\n');
        depth += line.matches('(').count() as i32 - line.matches(')').count() as i32;
        if depth <= 0 {
            break;
        }
    }
    out
}

/// Every `(net NAME (pins ...))` as name -> sorted pin tokens.
fn nets(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        let Some(rest) = t.strip_prefix("(net ") else { continue };
        if !rest.ends_with(')') {
            let name = rest.trim().trim_matches('"').to_string();
            let pins = lines[i + 1].trim();
            let pins = pins
                .trim_start_matches("(pins ")
                .trim_end_matches(')')
                .split_whitespace()
                .map(|s| s.trim_matches('"').to_string())
                .collect::<Vec<_>>();
            let mut pins = pins;
            pins.sort();
            out.insert(name, pins);
        }
    }
    out
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

#[test]
fn matches_python_reference() {
    let ours = ours();

    assert_eq!(
        normalise_via_costs(&scope(&ours, "(autoroute_settings")),
        normalise_via_costs(&scope(GOLDEN, "(autoroute_settings")),
        "autoroute_settings block differs"
    );
    assert_eq!(
        scope(&ours, "(boundary"),
        scope(GOLDEN, "(boundary"),
        "boundary differs"
    );
    let rule = |t: &str| {
        t.lines()
            .skip_while(|l| !l.trim_start().starts_with("(rule"))
            .take(4)
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(rule(&ours), rule(GOLDEN), "structure (rule ...) differs");

    let (a, b) = (nets(&ours), nets(GOLDEN));
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "net names differ"
    );
    for (name, pins) in &b {
        assert_eq!(a.get(name), Some(pins), "pins differ for net {name}");
    }

    assert_eq!(
        count(&ours, "(padstack "),
        count(GOLDEN, "(padstack "),
        "padstack count differs"
    );
    assert_eq!(
        count(&ours, "(image "),
        count(GOLDEN, "(image "),
        "image count differs"
    );
    assert_eq!(
        count(&ours, "(place "),
        count(GOLDEN, "(place "),
        "placement count differs"
    );
    assert_eq!(
        count(&ours, "(plane "),
        count(GOLDEN, "(plane "),
        "plane count differs"
    );
    assert_eq!(
        count(&ours, "(pin "),
        count(GOLDEN, "(pin "),
        "pin count differs"
    );
    for class in ["(class kicad_default", "(class fine"] {
        assert_eq!(
            scope(&ours, class),
            scope(GOLDEN, class),
            "{class} differs"
        );
    }
}

/// The one line where this writer deliberately parts company with the Python reference.
///
/// `via_costs` is a router cost, not board geometry: 100 is what pcbagent tuned for four-layer
/// boards, and an A/B on a placed Blue Pill said 30 routes the same netlist in 34 s with 2 items
/// left against 37 s with 8. The golden file still carries 100, so the comparison normalises it.
fn normalise_via_costs(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_start().starts_with("(via_costs ") {
                "      (via_costs N)".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Byte equality is the bar we actually reach on this board; keep it honest.
#[test]
fn byte_identical() {
    assert_eq!(normalise_via_costs(&ours()), normalise_via_costs(GOLDEN));
}
