//! Regressions reduced from the audio-preamp campaign placement panic.

use std::collections::BTreeMap;

use geom::Point2;
use kicad_symbol::geometry::{PinGeom, SymbolGeometry};
use sch_model::engine::{PlacementEngine, SchematicPlaceProblem};
use sch_model::ir::{Band, LayoutIr};
use sch_model::item::{Incidence, Item};
use sch_model::stub::StubEvaluator;

fn item(refdes: &str, part: &str, nets: &[&str], geometry_pins: usize) -> Item {
    let geom_pins = (0..geometry_pins)
        .map(|index| PinGeom {
            number: (index + 1).to_string(),
            name: format!("p{}", index + 1),
            at: Point2::new(index as f64 * 2.54, 0.0),
            angle: 0.0,
            length: 1.27,
            unit: 1,
            text: Default::default(),
})
        .collect();
    Item {
        refdes: refdes.into(),
        block: String::new(),
        part: part.into(),
        value: String::new(),
        footprint: None,
        geom: SymbolGeometry {
            lib_id: part.into(),
            pins: geom_pins,
            raw_definition: String::new(),
        },
        pins: nets
            .iter()
            .enumerate()
            .map(|(index, net)| {
                (
                    (index + 1).to_string(),
                    format!("p{}", index + 1),
                    Some((*net).into()),
                )
            })
            .collect(),
        at: Point2::new(0.0, 0.0),
        angle: 0.0,
        unit: 1,
        mirror: false,
        frozen: false,
        preseeded: false,
    }
}

fn incidence(items: &[Item]) -> Incidence {
    let mut incidence = BTreeMap::new();
    for (item, value) in items.iter().enumerate() {
        for (pin, _, net) in &value.pins {
            if let Some(net) = net {
                incidence
                    .entry(net.clone())
                    .or_insert_with(Vec::new)
                    .push((item, pin.clone()));
            }
        }
    }
    incidence
}

#[test]
fn adopted_anchor_with_a_rail_ladder_does_not_panic() {
    let items = vec![
        item(
            "U1",
            "Amplifier_Operational:OpAmp",
            &["BUF_IN", "OUT", "FB"],
            5,
        ),
        item(
            "RV1",
            "Device:R_Potentiometer",
            &["VREF", "BUF_IN", "BUF_IN"],
            3,
        ),
        item("R1", "Device:R", &["BUF_IN", "VREF"], 2),
    ];
    let inc = incidence(&items);
    let mut ir = LayoutIr::default();
    ir.rails.insert("VREF".into(), Band::Top);
    let mut problem = SchematicPlaceProblem::new(items, inc.clone(), ir.clone(), 1);

    spine_place::SpinePlace.place(&mut problem, &StubEvaluator::new(&inc, &ir));

    assert!(problem.items.iter().all(|item| item.at.x.is_finite()));
}
