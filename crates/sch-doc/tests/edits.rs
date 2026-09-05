//! Gate 3 — an edit changes what it says it changes and nothing else.
//!
//! Every case starts from the same schematic, applies one mutator, and checks
//! both halves of the contract: the net delta is exactly the intended one, and
//! every top-level block the edit did not touch comes back byte for byte.

mod corpus;

use geom::Point2;
use sch_doc::{LabelKind, Mirror, Pose, SchDoc, SymbolSource, connect};

/// The document crate's own copy of a drawn sheet. It used to point at the layout
/// crate's snapshot of the same circuit, and that snapshot is re-baselined whenever the
/// typesetter changes how it draws — one such change turned the `OUT` label from global
/// to plain, which is a correct drawing decision and silently changed what this crate's
/// naming tests were asserting about. A test about KiCAD's label ranking needs a sheet
/// whose labels do not move under it.
const FIXTURE: &str = "crates/sch-doc/tests/fixtures/divider-filter.kicad_sch";

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
fn clipping_wires_keeps_only_the_outside_fragments() {
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    let from = Point2::new(0.0, 250.0);
    let to = Point2::new(30.0, 250.0);
    doc.add_wire(from, to);

    let clipped = doc.clip_wires_outside(geom::Rect::new(10.0, 240.0, 20.0, 260.0));

    assert_eq!(clipped.wires, 1);
    assert_eq!(
        clipped.cut_points,
        [Point2::new(10.0, 250.0), Point2::new(20.0, 250.0)]
    );
    assert!(
        doc.wires()
            .any(|wire| wire.points == [from, Point2::new(10.0, 250.0)])
    );
    assert!(
        doc.wires()
            .any(|wire| wire.points == [Point2::new(20.0, 250.0), to])
    );
}

#[test]
fn setting_a_field_touches_one_block_and_no_net() {
    let (source, text, delta) = edited(|doc| {
        doc.set_field("R7", "Value", "22k").expect("set_field");
        doc.set_field("R7", "MPN", "RC0603FR-0722KL")
            .expect("set_field");
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
fn renaming_to_an_existing_reference_is_refused_without_editing() {
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    let before = doc.to_text();

    let refused = doc.set_field("R7", "Reference", "C3");

    assert!(
        matches!(refused, Err(sch_doc::Error::ReferenceInUse(ref name)) if name == "C3"),
        "{refused:?}"
    );
    assert!(!doc.is_edited());
    assert_eq!(doc.to_text(), before);
}

#[test]
fn moving_a_symbol_carries_its_fields_and_drops_only_its_own_pins() {
    let (source, text, delta) = edited(|doc| {
        let r7 = doc.symbol_by_ref("R7").expect("R7");
        let (x, y) = (r7.at.x + 25.4, r7.at.y + 25.4);
        doc.move_symbol("R7", x, y).expect("move");
    });
    assert!(
        delta.pins_now_unconnected.iter().all(|p| p.refdes == "R7"),
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
    assert_eq!(
        lost.len(),
        1,
        "only lib_symbols should change, lost {lost:?}"
    );
    assert!(lost[0].contains("lib_symbols"), "{lost:?}");

    let doc = SchDoc::parse(&text).expect("reparse");
    assert!(doc.lib_symbols().expect("libs").contains("Device:L"));
    let l9 = doc.symbol_by_ref("L9").expect("L9");
    assert_eq!(l9.lib_id, "Device:L");
    assert_eq!(l9.value(), "10uH");
    assert_eq!(l9.pin_uuids.len(), 2, "pin uuids: {:?}", l9.pin_uuids);
    assert_eq!(
        sch_doc::placed_pins(&doc)
            .iter()
            .filter(|p| p.refdes == "L9")
            .count(),
        2
    );
}

#[test]
fn adding_a_symbol_is_deterministic() {
    let Some(source_lib) = symbol_source() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let once = edited(|doc| {
        doc.add_symbol(
            "Device:L",
            "L9",
            "10uH",
            Pose::new(20.0, 20.0, 0.0),
            &source_lib,
        )
        .expect("add");
    });
    let twice = edited(|doc| {
        doc.add_symbol(
            "Device:L",
            "L9",
            "10uH",
            Pose::new(20.0, 20.0, 0.0),
            &source_lib,
        )
        .expect("add");
    });
    assert_eq!(once.1, twice.1, "UUIDs are not content-derived");
}

/// Two added wires fuse a global-labelled net with a power rail. The local
/// label riding the corner does not get to name the result, and neither does
/// the rail: KiCAD ranks a global label above a power symbol above a local one.
#[test]
fn added_wires_merge_two_nets_and_the_stronger_name_wins() {
    let (source, text, delta) = edited(|doc| {
        let pins = sch_doc::placed_pins(doc);
        let a = pins
            .iter()
            .find(|p| p.refdes == "R7" && p.number == "1")
            .expect("R7.1");
        let b = pins
            .iter()
            .find(|p| p.refdes == "C3" && p.number == "1")
            .expect("C3.1");
        let corner = Point2::new(a.at.x, b.at.y);
        doc.add_wire(a.at, corner);
        doc.add_wire(corner, b.at);
        doc.add_label(
            LabelKind::Local,
            "STITCH",
            Pose::new(corner.x, corner.y, 0.0),
        );
    });
    let (_, lost) = surviving(&source, &text);
    assert!(
        lost.is_empty(),
        "additions rewrote existing blocks: {lost:?}"
    );
    assert_eq!(
        delta.merged,
        vec![(
            vec!["OUT".to_string(), "VCC".to_string()],
            "OUT".to_string()
        )],
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
    assert!(
        SchDoc::parse(&text)
            .expect("reparse")
            .symbol_by_ref("C3")
            .is_none()
    );
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

/// A sheet the hierarchy places several times carries one `(instances)` path
/// per placement, each with its own reference. Moving a symbol must not touch
/// that table, and renaming one has to be refused rather than flattening it.
#[test]
fn a_re_instantiated_sheet_keeps_its_per_placement_references() {
    let Some(path) = corpus::files()
        .into_iter()
        .find(|p| p.ends_with("multichannel/channel_strip.kicad_sch"))
    else {
        eprintln!("SKIP: corpus not found");
        return;
    };
    let source = std::fs::read_to_string(&path).expect("read");
    let mut doc = SchDoc::parse(&source).expect("parse");
    let refdes = doc.symbols().next().expect("a symbol").refdes().to_string();
    assert!(
        references(&source).len() > 4,
        "fixture has no instance table"
    );

    doc.move_symbol(&refdes, 10.0, 10.0).expect("move");
    let moved = doc.to_text();
    assert_eq!(
        references(&moved),
        references(&source),
        "moving a symbol rewrote the instance table"
    );

    let refused = doc.set_field(&refdes, "Reference", "R999");
    assert!(
        matches!(refused, Err(sch_doc::Error::ForeignInstances(_))),
        "renaming was allowed: {refused:?}"
    );
    assert_eq!(references(&doc.to_text()), references(&source));
}

/// Every `(reference "…")` in the file, in order.
fn references(text: &str) -> Vec<&str> {
    text.match_indices("(reference \"")
        .filter_map(|(at, tag)| {
            let rest = &text[at + tag.len()..];
            rest.find('"').map(|end| &rest[..end])
        })
        .collect()
}

/// The units of one part share a reference, so a reference does not name a
/// symbol there. Editing one unit and leaving its siblings behind would make a
/// part whose halves disagree; the mutators take a UUID for that case.
#[test]
fn a_reference_shared_by_several_units_is_refused() {
    let Some(path) = corpus::files()
        .into_iter()
        .find(|p| p.ends_with("ecc83/ecc83-pp.kicad_sch"))
    else {
        eprintln!("SKIP: corpus not found");
        return;
    };
    let mut doc = SchDoc::parse(&std::fs::read_to_string(&path).expect("read")).expect("parse");
    let shared = doc
        .symbols()
        .find(|s| doc.symbols().filter(|o| o.refdes() == s.refdes()).count() > 1)
        .map(|s| (s.refdes().to_string(), s.uuid.clone()))
        .expect("a multi-unit part");
    let units = doc.symbols().filter(|s| s.refdes() == shared.0).count();

    assert!(matches!(
        doc.move_symbol(&shared.0, 10.0, 10.0),
        Err(sch_doc::Error::AmbiguousReference(_))
    ));
    assert!(matches!(
        doc.remove_symbol(&shared.0),
        Err(sch_doc::Error::AmbiguousReference(_))
    ));

    // The same edit by UUID names one unit and goes through.
    doc.move_symbol(&shared.1, 10.0, 10.0)
        .expect("move by uuid");
    let moved = doc.symbol(&shared.1).expect("unit");
    assert_eq!((moved.at.x, moved.at.y), (10.0, 10.0));
    assert_eq!(
        doc.symbols().filter(|s| s.refdes() == shared.0).count(),
        units,
        "a sibling unit went missing"
    );
}

/// The parent of a derived symbol is referenced by nothing placed, but every
/// derived symbol in the file draws its body. Collecting it would leave them
/// with no pins.
#[test]
fn write_keeps_the_parent_a_derived_symbol_extends() {
    let text = r#"(kicad_sch (version 20250114) (generator "t")
        (uuid "00000000-0000-4000-8000-000000000001") (paper "A4")
        (lib_symbols
          (symbol "Device:R"
            (symbol "R_1_1"
              (pin passive line (at 0 3.81 270) (length 1.27) (name "~") (number "1"))
              (pin passive line (at 0 -3.81 90) (length 1.27) (name "~") (number "2"))))
          (symbol "Device:R_Small" (extends "R")))
        (symbol (lib_id "Device:R_Small") (at 100 100 0) (unit 1) (uuid "r1")
          (property "Reference" "R1" (at 100 100 0))
          (property "Value" "1k" (at 100 100 0))))
    "#;
    let mut doc = SchDoc::parse(text).expect("parse");
    let before = sch_doc::placed_pins(&doc).len();
    assert_eq!(before, 2, "the fixture never resolved the parent");

    doc.set_field("R1", "Value", "2k").expect("set_field");
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.kicad_sch");
    doc.write(&out).expect("write");

    let written = SchDoc::read(&out).expect("reparse");
    let libs = written.lib_symbols().expect("libs");
    assert!(libs.contains("Device:R"), "the parent body was collected");
    assert!(libs.contains("Device:R_Small"));
    assert_eq!(sch_doc::placed_pins(&written).len(), before);
}

/// A refused rename must leave nothing behind: the drawn reference and the
/// instance table disagreeing is exactly the corruption the refusal is for.
#[test]
fn a_refused_rename_changes_nothing() {
    let Some(path) = corpus::files()
        .into_iter()
        .find(|p| p.ends_with("multichannel/channel_strip.kicad_sch"))
    else {
        eprintln!("SKIP: corpus not found");
        return;
    };
    let source = std::fs::read_to_string(&path).expect("read");
    let mut doc = SchDoc::parse(&source).expect("parse");
    let refdes = doc.symbols().next().expect("a symbol").refdes().to_string();

    let refused = doc.set_field(&refdes, "Reference", "R999");
    assert!(
        matches!(refused, Err(sch_doc::Error::ForeignInstances(_))),
        "{refused:?}"
    );
    assert!(!doc.is_edited(), "a refused edit marked the document dirty");
    assert_eq!(doc.to_text(), source, "a refused edit changed the file");
    assert!(doc.symbol_by_ref("R999").is_none());
    assert!(doc.symbol_by_ref(&refdes).is_some());
}

/// The same sheet cannot take a new symbol either: one call cannot say what
/// each placement should call it.
#[test]
fn adding_to_a_re_instantiated_sheet_is_refused() {
    let (Some(path), Some(source_lib)) = (
        corpus::files()
            .into_iter()
            .find(|p| p.ends_with("multichannel/channel_strip.kicad_sch")),
        symbol_source(),
    ) else {
        eprintln!("SKIP: corpus or KiCAD not found");
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read");
    let mut doc = SchDoc::parse(&text).expect("parse");
    let added = doc.add_symbol("Device:R", "RX99", "1k", Pose::default(), &source_lib);
    assert!(
        matches!(added, Err(sch_doc::Error::ReInstantiatedSheet)),
        "{added:?}"
    );
    assert_eq!(doc.to_text(), text, "the refused add changed the file");
}

/// Library text carries escapes as much as document text does.
#[test]
fn a_spliced_definition_keeps_its_escaped_newlines() {
    let Some(source_lib) = symbol_source() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    // This symbol's graphic text holds a newline, written `\n`.
    if doc
        .add_symbol(
            "Analog_ADC:AD574A",
            "U9",
            "AD574A",
            Pose::new(40.0, 40.0, 0.0),
            &source_lib,
        )
        .is_err()
    {
        eprintln!("SKIP: Analog_ADC:AD574A not in this library");
        return;
    }
    let text = doc.to_text();
    assert!(
        text.contains("I_{DAC}\\nI_{DAC}"),
        "the escape was eaten on the way in"
    );
    assert!(SchDoc::parse(&text).is_ok());
}

/// `(lib_symbols)` is retained like everything else: a child the typed model
/// does not decode survives an edit that rewrites the block.
#[test]
fn lib_symbols_keeps_children_it_does_not_decode() {
    let (_, source) = fixture();
    let marked = source.replacen(
        "\t(lib_symbols\n",
        "\t(lib_symbols\n\t\t(something_new \"keepme\")\n",
        1,
    );
    assert_ne!(marked, source, "fixture layout changed");
    let mut doc = SchDoc::parse(&marked).expect("parse");
    doc.remove_symbol("C3").expect("remove");
    doc.gc_lib_symbols();
    let text = doc.to_text();
    assert!(
        text.contains("(something_new \"keepme\")"),
        "unknown child dropped"
    );
    assert!(
        !text.contains("(symbol \"Device:C\""),
        "orphan not collected"
    );
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

/// The whole add path through the oracle: a spliced `lib_symbols` entry, the
/// pin UUIDs, the cloned `(instances)` and a wire onto an existing net.
#[test]
fn kicad_sees_an_added_symbol_on_the_net_it_was_wired_to() {
    let (Some(kicad), Some(source_lib)) = (corpus::kicad10(), symbol_source()) else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let (_, source) = fixture();
    let mut doc = SchDoc::parse(&source).expect("parse");
    doc.add_symbol(
        "Device:L",
        "L9",
        "10uH",
        Pose::new(60.0, 40.0, 0.0),
        &source_lib,
    )
    .expect("add_symbol");
    let anchor = sch_doc::placed_pins(&doc)
        .into_iter()
        .find(|p| p.refdes == "R8" && p.number == "2")
        .expect("R8.2");
    let new_pin = sch_doc::placed_pins(&doc)
        .into_iter()
        .find(|p| p.refdes == "L9" && p.number == "1")
        .expect("L9.1");
    let corner = Point2::new(new_pin.at.x, anchor.at.y);
    doc.add_wire(new_pin.at, corner);
    doc.add_wire(corner, anchor.at);

    let ours = connect::extract(&doc);
    let net = ours
        .nets
        .iter()
        .find(|n| n.pins.iter().any(|p| p.refdes == "L9"))
        .expect("L9 landed on no net");
    assert!(net.pins.iter().any(|p| p.refdes == "R8"), "{net:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("edited.kicad_sch");
    doc.write(&out).expect("write");
    let oracle = kicad.netlist(&out).expect("netlist");
    let theirs = oracle
        .nets
        .iter()
        .find(|n| n.nodes.iter().any(|(refdes, _)| refdes == "L9"))
        .expect("kicad did not see L9");
    assert!(
        theirs.nodes.iter().any(|(refdes, _)| refdes == "R8"),
        "kicad put L9 on {theirs:?}"
    );
    assert_eq!(
        oracle
            .components
            .iter()
            .find(|c| c.reference == "L9")
            .map(|c| c.lib_id.as_str()),
        Some("Device:L")
    );
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
