use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::SchDoc;
use sch_floorplan::live::{self, Selection};

fn ldo_block() -> PlacePartsInput {
    serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "U9", "part": "Regulator_Linear:MCP1703Ax-330xxTT",
             "pins": {"VI": "VBULK", "VO": "V3P3", "GND": "AGND"}},
            {"ref": "C9", "part": "Device:C", "value": "10u", "pins": {"1": "VBULK", "2": "AGND"}},
            {"ref": "C10", "part": "Device:C", "value": "1u", "pins": {"1": "V3P3", "2": "AGND"}},
            {"ref": "R9", "part": "Device:R", "value": "330", "pins": {"1": "V3P3", "2": "LEDA"}},
            {"ref": "D9", "part": "Device:LED", "value": "red", "pins": {"1": "LEDA", "2": "AGND"}},
            {"ref": "C11", "part": "Device:C", "value": "100n", "pins": {"1": "V3P3", "2": "AGND"}}
        ],
        "intent": {"relations": [{"kind": "group", "name": "ldo",
            "members": ["U9", "C9", "C10", "R9", "D9", "C11"], "side": ["right", "R1"]}]}
    }))
    .unwrap()
}

#[test]
fn dump() {
    let Some(env) = KicadInstallation::detect() else { return };
    let demo = std::path::Path::new(
        "/home/mimi/agent/.local/kicad-10.0.4/AppDir/share/kicad/demos/simulation/rectifier/rectifier.kicad_sch",
    )
    .to_path_buf();
    if !demo.exists() {
        eprintln!("SKIP: no demo at {}", demo.display());
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut doc = SchDoc::read(&demo).unwrap();
    live::place_parts(&env, &mut doc, &ldo_block(), Box::new(cluster_place::ClusterPlace), None)
        .unwrap();
    let seeded = dir.path().join("seeded.kicad_sch");
    std::fs::write(&seeded, doc.to_text()).unwrap();
    let before: Vec<String> = env.erc(&seeded).unwrap().violations.iter()
        .map(|v| format!("{} {:?}", v.kind, v.items.iter().filter_map(|i| i.pos).map(|p| (p.x, p.y)).collect::<Vec<_>>())).collect();

    let refs = ["U9", "C9", "C10", "R9", "D9", "C11"].map(String::from).to_vec();
    let report = live::arrange(&env, &mut doc, &Selection::Refs(refs), None,
        Box::new(cluster_place::ClusterPlace), None).unwrap();
    eprintln!("committed={} labelled={}", report.committed, report.labelled);
    let after_path = dir.path().join("after.kicad_sch");
    std::fs::write(&after_path, doc.to_text()).unwrap();
    for v in env.erc(&after_path).unwrap().violations {
        let key = format!("{} {:?}", v.kind, v.items.iter().filter_map(|i| i.pos).map(|p| (p.x, p.y)).collect::<Vec<_>>());
        if v.kind == "label_dangling" {
            eprintln!("DANGLING {key} :: {}", v.description);
        }
        let _ = &before;
    }
    for label in doc.labels() {
        eprintln!("LABEL {:?} {} at {:?}", label.kind, label.text, label.at.point());
    }
}
