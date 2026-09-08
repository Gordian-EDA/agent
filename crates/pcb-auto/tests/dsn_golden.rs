//! The Rust DSN writer against a DSN dumped by the Python reference (`pcbagent.route.dsn`)
//! for the same board. Exact bytes are not required — image naming order may differ — but
//! the structure the router reads has to be identical.

use std::collections::BTreeMap;

use pcb_auto::dsn::write_dsn;
use pcb_auto::model::Board;
use pcb_auto::rules::{infer_rules, router_net_widths};

const BOARD: &str = include_str!("fixtures/bluepill_outlined.kicad_pcb");
const GOLDEN: &str = include_str!("fixtures/bluepill.dsn");

fn ours() -> String {
    let board = Board::parse(BOARD).expect("fixture board parses");
    let rules = infer_rules(&board);
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
        scope(&ours, "(autoroute_settings"),
        scope(GOLDEN, "(autoroute_settings"),
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

/// Byte equality is the bar we actually reach on this board; keep it honest.
#[test]
fn byte_identical() {
    assert_eq!(ours(), GOLDEN);
}
