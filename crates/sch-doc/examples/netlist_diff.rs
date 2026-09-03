//! Print pin-partition differences between the pure-Rust extractor and KiCad.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sch_doc::{SchDoc, connect, placed_pins};

type PinSet = BTreeSet<String>;
type NamedNets = Vec<(String, PinSet)>;

fn virtual_pin(refdes: &str) -> bool {
    refdes.starts_with('#')
}

fn ours(netlist: &connect::Netlist) -> NamedNets {
    netlist
        .nets
        .iter()
        .map(|net| {
            let pins = net
                .pins
                .iter()
                .filter(|pin| !virtual_pin(&pin.refdes))
                .map(|pin| format!("{}.{}", pin.refdes, pin.pin))
                .collect();
            (net.name.clone(), pins)
        })
        .filter(|(_, pins): &(String, PinSet)| !pins.is_empty())
        .collect()
}

fn oracle(netlist: &kicad::Netlist) -> NamedNets {
    netlist
        .nets
        .iter()
        .filter(|net| !net.name.starts_with("unconnected-"))
        .map(|net| {
            let pins = net
                .nodes
                .iter()
                .filter(|(refdes, _)| !virtual_pin(refdes))
                .map(|(refdes, pin)| format!("{refdes}.{pin}"))
                .collect();
            (net.name.clone(), pins)
        })
        .filter(|(_, pins): &(String, PinSet)| !pins.is_empty())
        .collect()
}

fn owners(nets: &NamedNets) -> BTreeMap<String, PinSet> {
    nets.iter()
        .flat_map(|(_, pins)| pins.iter().map(move |pin| (pin.clone(), pins.clone())))
        .collect()
}

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .unwrap_or_else(|| panic!("usage: netlist_diff FILE.kicad_sch"));
    let path = Path::new(&path);
    let doc = SchDoc::read(path).expect("read schematic");
    let extracted = connect::extract(&doc);
    let kicad = kicad::KicadInstallation::detect()
        .expect("find KiCad 10; set KICAD_CLI if necessary")
        .netlist(path)
        .expect("export KiCad netlist");
    let (ours, oracle) = (ours(&extracted), oracle(&kicad));

    println!(
        "ours: {}, kicad: {} non-pseudo nets",
        ours.len(),
        oracle.len()
    );
    println!("warnings: {:?}", extracted.warnings);
    let (our_owner, oracle_owner) = (owners(&ours), owners(&oracle));
    let pins: BTreeSet<_> = our_owner.keys().chain(oracle_owner.keys()).collect();
    for pin in pins {
        let mine = our_owner.get(pin);
        let theirs = oracle_owner.get(pin);
        if mine != theirs {
            let placed = placed_pins(&doc)
                .into_iter()
                .find(|placed| format!("{}.{}", placed.refdes, placed.number) == *pin);
            println!(
                "PIN {pin}: ours={mine:?} kicad={theirs:?} at={:?}",
                placed.map(|placed| placed.at)
            );
        }
    }
    for (side, nets, other) in [("OURS", &ours, &oracle), ("KICAD", &oracle, &ours)] {
        for (name, members) in nets {
            if !other.iter().any(|(_, candidate)| candidate == members) {
                println!("{side} {name:?}: {members:?}");
            }
        }
    }
}
