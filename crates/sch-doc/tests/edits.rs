//! Gate 3 — an edit changes what it says it changes and nothing else.
//!
//! Every case starts from the same schematic, applies one mutator, and checks
//! both halves of the contract: the net delta is exactly the intended one, and
//! every top-level block the edit did not touch comes back byte for byte.

mod corpus;

use geom::Point2;
use sch_doc::{LabelKind, Mirror, Pose, SchDoc, SymbolSource, connect};

const FIXTURE: &str = "crates/sch-floorplan/tests/snapshots/divider-filter.kicad_sch";

fn fixture() -> (std::path::PathBuf, String) {
    let path = corpus::repo_roots()
        .into_iter()
        .map(|root| root.join(FIXTURE))
        .find(|p| p.exists())
        .expect("fixture");
    let text = std::fs::read_to_string(&path).expect("read");
    (path, text)
}

/// Top-level blocks of a tab-formatted schematic, verbatim.
fn blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match &mut current {
            None if line.starts_with("\t(") => current = Some(line.to_string()),
            None => {}
            Some(block) => {
                block.push('\n');
                block.push_str(line);
                if line == "\t)" {
                    out.push(current.take().expect("open block"));
                }
            }
        }
    }
    out
}

/// Blocks of `before` that survive into `after` untouched, and those that do not.
fn surviving(before: &str, after: &str) -> (usize, Vec<String>) {
    let mut lost = Vec::new();
    let mut kept = 0;
    for block in blocks(before) {
        if after.contains(&block) {
            kept += 1;
        } else {
            lost.push(block.lines().take(3).collect::<Vec<_>>().join(" | "));
        }
    }
    (kept, lost)
}

fn edited(mutate: impl FnOnce(&mut SchDoc)) -> (String, String, connect::NetDelta) {
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    let before = connect::extract(&doc);
    mutate(&mut doc);
    let text = doc.to_text();
    let reparsed = SchDoc::parse(&text).expect("reparse");
    let delta = connect::Netlist::diff(&before, &connect::extract(&reparsed));
    (source, text, delta)
}

fn symbol_source() -> Option<SymbolSource> {
    corpus::kicad10().map(|k| SymbolSource::new(k.symbol_dir()))
}

#[test]
fn setting_a_field_touches_one_block_and_no_net() {
    let (source, text, delta) = edited(|doc| {
        doc.set_field("R7", "Value", "22k").expect("set_field");
        doc.set_field("R7", "MPN", "RC0603FR-0722KL").expect("set_field");
    });
    assert!(delta.is_empty(), "{delta:?}");
    let (kept, lost) = surviving(&source, &text);
    assert_eq!(lost.len(), 1, "expected only R7 to change, lost {lost:?}");
    assert!(lost[0].contains("symbol"), "{lost:?}");
    assert!(kept > 10, "only {kept} blocks survived");

    let doc = SchDoc::parse(&text).expect("reparse");
    let r7 = doc.symbol_by_ref("R7").expect("R7");
    assert_eq!(r7.value(), "22k");
    assert_eq!(r7.fields["MPN"].value, "RC0603FR-0722KL");
}

#[test]
fn moving_a_symbol_carries_its_fields_and_drops_only_its_own_pins() {
    let (source, text, delta) = edited(|doc| {
        let r7 = doc.symbol_by_ref("R7").expect("R7");
        let (x, y) = (r7.at.x + 25.4, r7.at.y + 25.4);
        doc.move_symbol("R7", x, y).expect("move");
    });
    assert!(
        delta
            .pins_now_unconnected
            .iter()
            .all(|p| p.refdes == "R7"),
        "{:?}",
        delta.pins_now_unconnected
    );
    assert!(!delta.pins_now_unconnected.is_empty(), "R7 stayed wired");
    let (_, lost) = surviving(&source, &text);
    assert_eq!(lost.len(), 1, "expected only R7 to change, lost {lost:?}");

    let doc = SchDoc::parse(&text).expect("reparse");
    let before = SchDoc::parse(&source).expect("parse");
    let (was, now) = (
        before.symbol_by_ref("R7").expect("R7"),
        doc.symbol_by_ref("R7").expect("R7"),
    );
    assert!((now.at.x - was.at.x - 25.4).abs() < 1e-9);
    for (name, field) in &now.fields {
        let (Some(old), Some(new)) = (was.fields[name].at, field.at) else {
            continue;
        };
        assert!(
            (new.x - old.x - 25.4).abs() < 1e-9 && (new.y - old.y - 25.4).abs() < 1e-9,
            "{name} lagged"
        );
    }
}

#[test]
fn orientation_moves_pins_without_moving_the_symbol() {
    let (source, text, _) = edited(|doc| {
        doc.set_symbol_orientation("R7", 90.0, Mirror::Y)
            .expect("orient");
    });
    let (_, lost) = surviving(&source, &text);
    assert_eq!(lost.len(), 1, "{lost:?}");
    let doc = SchDoc::parse(&text).expect("reparse");
    let r7 = doc.symbol_by_ref("R7").expect("R7");
    assert_eq!(r7.at.rot, 90.0);
    assert_eq!(r7.mirror, Mirror::Y);
}

#[test]
fn adding_a_symbol_embeds_its_definition_and_leaves_nets_alone() {
    let Some(source_lib) = symbol_source() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let (source, text, delta) = edited(|doc| {
        assert!(
            doc.lib_symbols().is_some_and(|l| !l.contains("Device:L")),
            "fixture already embeds Device:L"
        );
        doc.add_symbol(
            "Device:L",
            "L9",
            "10uH",
            Pose::new(20.0, 20.0, 0.0),
            &source_lib,
        )
        .expect("add_symbol");
    });
    assert!(delta.is_empty(), "{delta:?}");
    let (_, lost) = surviving(&source, &text);
    assert_eq!(lost.len(), 1, "only lib_symbols should change, lost {lost:?}");
    assert!(lost[0].contains("lib_symbols"), "{lost:?}");

    let doc = SchDoc::parse(&text).expect("reparse");
    assert!(doc.lib_symbols().expect("libs").contains("Device:L"));
    let l9 = doc.symbol_by_ref("L9").expect("L9");
    assert_eq!(l9.lib_id, "Device:L");
    assert_eq!(l9.value(), "10uH");
    assert_eq!(l9.pin_uuids.len(), 2, "pin uuids: {:?}", l9.pin_uuids);
    assert_eq!(sch_doc::placed_pins(&doc).iter().filter(|p| p.refdes == "L9").count(), 2);
}

#[test]
fn adding_a_symbol_is_deterministic() {
    let Some(source_lib) = symbol_source() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let once = edited(|doc| {
        doc.add_symbol("Device:L", "L9", "10uH", Pose::new(20.0, 20.0, 0.0), &source_lib)
            .expect("add");
    });
    let twice = edited(|doc| {
        doc.add_symbol("Device:L", "L9", "10uH", Pose::new(20.0, 20.0, 0.0), &source_lib)
            .expect("add");
    });
    assert_eq!(once.1, twice.1, "UUIDs are not content-derived");
}

/// Two added wires fuse two nets; the label riding the corner does not get to
/// name the result, because a power rail outranks a local label.
#[test]
fn added_wires_merge_two_nets_and_the_stronger_name_wins() {
    let (source, text, delta) = edited(|doc| {
        let pins = sch_doc::placed_pins(doc);
        let a = pins.iter().find(|p| p.refdes == "R7" && p.number == "1").expect("R7.1");
        let b = pins.iter().find(|p| p.refdes == "C3" && p.number == "1").expect("C3.1");
        let corner = Point2::new(a.at.x, b.at.y);
        doc.add_wire(a.at, corner);
        doc.add_wire(corner, b.at);
        doc.add_label(LabelKind::Local, "STITCH", Pose::new(corner.x, corner.y, 0.0));
    });
    let (_, lost) = surviving(&source, &text);
    assert!(lost.is_empty(), "additions rewrote existing blocks: {lost:?}");
    assert_eq!(
        delta.merged,
        vec![(vec!["OUT".to_string(), "VCC".to_string()], "VCC".to_string())],
        "{delta:?}"
    );
    assert!(delta.pins_now_unconnected.is_empty(), "{delta:?}");
}

#[test]
fn removing_a_symbol_takes_its_pins_and_nothing_else() {
    let (source, text, delta) = edited(|doc| doc.remove_symbol("C3").expect("remove"));
    assert!(
        delta.pins_now_unconnected.iter().all(|p| p.refdes == "C3"),
        "{:?}",
        delta.pins_now_unconnected
    );
    let (_, lost) = surviving(&source, &text);
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(SchDoc::parse(&text).expect("reparse").symbol_by_ref("C3").is_none());
}

#[test]
fn write_collects_definitions_the_edit_orphaned() {
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    assert!(doc.lib_symbols().expect("libs").contains("Device:C"));
    doc.remove_symbol("C3").expect("remove");
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.kicad_sch");
    doc.write(&out).expect("write");
    let written = SchDoc::read(&out).expect("reparse");
    assert!(!written.lib_symbols().expect("libs").contains("Device:C"));
    assert!(written.lib_symbols().expect("libs").contains("Device:R"));
}

#[test]
fn snapshots_restore_the_document_exactly() {
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    let start = doc.snapshot();
    doc.move_symbol("R7", 10.0, 10.0).expect("move");
    doc.set_field("R8", "Value", "0R").expect("set_field");
    let moved = doc.snapshot();
    doc.restore(start).expect("restore");
    assert_eq!(doc.to_text(), source);
    doc.restore(moved).expect("restore");
    assert_eq!(doc.symbol_by_ref("R7").expect("R7").at.x, 10.0);
    assert_eq!(doc.symbol_by_ref("R8").expect("R8").value(), "0R");
}

#[test]
fn kicad_agrees_with_the_edited_file() {
    let Some(kicad) = corpus::kicad10() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    doc.set_field("R7", "Value", "22k").expect("set_field");
    let dir = tempfile::tempdir().expect("tempdir");
    let original = dir.path().join("before.kicad_sch");
    let after = dir.path().join("after.kicad_sch");
    std::fs::write(&original, &source).expect("write");
    doc.write(&after).expect("write");

    let mut before_nets = kicad.netlist(&original).expect("netlist").nets;
    let mut after_nets = kicad.netlist(&after).expect("netlist").nets;
    for nets in [&mut before_nets, &mut after_nets] {
        for net in nets.iter_mut() {
            net.nodes.sort();
        }
        nets.sort_by(|a, b| a.nodes.cmp(&b.nodes));
    }
    assert_eq!(
        before_nets.iter().map(|n| &n.nodes).collect::<Vec<_>>(),
        after_nets.iter().map(|n| &n.nodes).collect::<Vec<_>>()
    );
}
