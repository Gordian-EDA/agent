use sch_doc::{SchDoc, WireFaultKind};

#[test]
fn wire_faults_reports_diagonal_and_degenerate_segments() {
    let doc = SchDoc::parse(
        r#"(kicad_sch
	(version 20250114)
	(generator "eeschema")
	(uuid "00000000-0000-0000-0000-000000000001")
	(wire (pts (xy 10 10) (xy 20 20)) (stroke (width 0) (type default)) (uuid "diagonal"))
	(wire (pts (xy 30 30) (xy 30 30)) (stroke (width 0) (type default)) (uuid "degenerate"))
	(wire (pts (xy 40 40) (xy 50 40) (xy 50 60)) (stroke (width 0) (type default)) (uuid "orthogonal"))
)"#,
    )
    .expect("fixture parses");

    let faults = doc.wire_faults();
    assert_eq!(faults.len(), 2);
    assert_eq!(faults[0].wire, "diagonal");
    assert_eq!(faults[0].segment, 0);
    assert_eq!(faults[0].kind, WireFaultKind::Diagonal);
    assert_eq!(faults[1].wire, "degenerate");
    assert_eq!(faults[1].segment, 0);
    assert_eq!(faults[1].kind, WireFaultKind::Degenerate);
}
