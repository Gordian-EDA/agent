//! One label scope per net, and one no-connect marker per point.
//!
//! Both are rules about the drawing rather than the netlist: KiCAD reports a net
//! carrying a `label` and a `global_label` of one name as
//! `same_local_global_label`, and it counts two markers at one coordinate twice.

use geom::Point2;
use sch_doc::{Item, LabelKind, Pose, SchDoc};

const ROOT: &str = "00000000-0000-4000-8000-000000000042";

fn sheet() -> SchDoc {
    SchDoc::parse(&format!(
        "(kicad_sch (version 20250114) (generator \"test\") (uuid \"{ROOT}\") (paper \"A4\")\n\
         (lib_symbols)\n)\n"
    ))
    .expect("fixture parses")
}

fn kinds(doc: &SchDoc, net: &str) -> Vec<LabelKind> {
    doc.labels()
        .filter(|label| sch_doc::unescape(&label.text) == net)
        .map(|label| label.kind)
        .collect()
}

/// A net drawn with both scopes is given one, at every one of its labels.
#[test]
fn set_label_scope_gives_a_net_a_single_scope() {
    let mut doc = sheet();
    doc.add_label(LabelKind::Global, "USB_D+", Pose::new(50.0, 50.0, 0.0));
    doc.add_label(LabelKind::Local, "USB_D+", Pose::new(60.0, 50.0, 0.0));
    doc.add_label(LabelKind::Local, "USB_D+", Pose::new(70.0, 50.0, 0.0));
    doc.add_label(LabelKind::Local, "GND", Pose::new(80.0, 50.0, 0.0));

    assert_eq!(doc.set_label_scope("USB_D+", LabelKind::Global), 2);

    assert_eq!(kinds(&doc, "USB_D+"), vec![LabelKind::Global; 3]);
    assert_eq!(
        kinds(&doc, "GND"),
        vec![LabelKind::Local],
        "GND was touched"
    );
}

/// Demotion is the other half of the rule: a sheet whose net is local stays
/// local when a later block reaches it with a pennant.
#[test]
fn set_label_scope_demotes_a_pennant_to_the_sheets_own_scope() {
    let mut doc = sheet();
    doc.add_label(LabelKind::Local, "HSE1", Pose::new(50.0, 50.0, 0.0));
    doc.add_label(LabelKind::Global, "HSE1", Pose::new(60.0, 50.0, 0.0));

    assert_eq!(doc.set_label_scope("HSE1", LabelKind::Local), 1);

    assert_eq!(kinds(&doc, "HSE1"), vec![LabelKind::Local; 2]);
    let text = doc.to_text();
    assert!(!text.contains("global_label"), "{text}");
}

/// A rewritten label keeps its place, and is a well-formed label of the new
/// scope — not a re-headed node still carrying the other scope's children.
#[test]
fn a_rescoped_label_keeps_its_position_and_sheds_its_old_shape() {
    let mut doc = sheet();
    doc.add_label(LabelKind::Global, "NRST", Pose::new(50.0, 63.5, 90.0));
    doc.set_label_scope("NRST", LabelKind::Local);
    let text = doc.to_text();

    let reread = SchDoc::parse(&text).expect("the rewritten sheet parses");

    let label = reread.labels().next().expect("the label survived");
    assert_eq!(label.kind, LabelKind::Local);
    assert_eq!(label.at.point(), Point2::new(50.0, 63.5));
    assert_eq!(label.at.rot, 90.0);
    assert!(!text.contains("Intersheetrefs"), "{text}");
}

/// A hierarchical label names a sheet pin, not a sheet net; it is nobody's
/// duplicate scope.
#[test]
fn a_hierarchical_label_is_left_alone() {
    let mut doc = sheet();
    doc.add_label(LabelKind::Hier, "IN", Pose::new(50.0, 50.0, 0.0));
    doc.add_label(LabelKind::Local, "IN", Pose::new(60.0, 50.0, 0.0));

    assert_eq!(doc.set_label_scope("IN", LabelKind::Global), 1);

    assert_eq!(kinds(&doc, "IN"), vec![LabelKind::Hier, LabelKind::Global]);
}

/// Two markers at one coordinate say nothing a single one does not, and KiCAD
/// reports each of them against the pin.
#[test]
fn a_second_no_connect_at_one_point_is_the_first_one() {
    let mut doc = sheet();
    let at = Point2::new(50.0, 46.19);

    let first = doc.add_no_connect(at);
    let second = doc.add_no_connect(at);

    assert_eq!(first, second);
    let count = doc
        .items()
        .iter()
        .filter(|item| matches!(item, Item::NoConnect(_)))
        .count();
    assert_eq!(count, 1);
}
