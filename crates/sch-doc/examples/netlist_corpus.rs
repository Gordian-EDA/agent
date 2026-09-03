//! Compare extractor and KiCad pin partitions over a deterministic file sample.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sch_doc::{Item, SchDoc, connect};

type Partition = BTreeSet<Vec<String>>;

fn virtual_pin(refdes: &str) -> bool {
    refdes.starts_with('#') || refdes.contains('?')
}

fn pin_set<'a>(pins: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let mut pins: Vec<_> = pins
        .filter(|(refdes, _)| !virtual_pin(refdes))
        .map(|(refdes, pin)| format!("{refdes}.{pin}"))
        .collect();
    pins.sort();
    pins.dedup();
    pins
}

fn ours(netlist: &connect::Netlist) -> Partition {
    let mut partition: Partition = netlist
        .nets
        .iter()
        .map(|net| {
            pin_set(
                net.pins
                    .iter()
                    .map(|pin| (pin.refdes.as_str(), pin.pin.as_str())),
            )
        })
        .filter(|pins| !pins.is_empty())
        .collect();
    partition.extend(
        netlist
            .unconnected
            .iter()
            .chain(&netlist.no_connect)
            .filter(|pin| !virtual_pin(&pin.refdes))
            .map(|pin| vec![format!("{}.{}", pin.refdes, pin.pin)]),
    );
    partition
}

fn oracle(netlist: &kicad::Netlist) -> Partition {
    netlist
        .nets
        .iter()
        .map(|net| {
            pin_set(
                net.nodes
                    .iter()
                    .map(|(refdes, pin)| (refdes.as_str(), pin.as_str())),
            )
        })
        .filter(|pins| !pins.is_empty())
        .collect()
}

fn files(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "kicad_sch"))
        .collect();
    files.sort();
    files.truncate(limit);
    files
}

fn reason(doc: &SchDoc, warnings: &[String]) -> &'static str {
    if doc
        .items()
        .iter()
        .any(|item| matches!(item, Item::Sheet(_)))
    {
        "hierarchy"
    } else if warnings.iter().any(|warning| warning.contains("buses")) {
        "bus"
    } else if warnings
        .iter()
        .any(|warning| warning.contains("no embedded"))
    {
        "missing-symbol"
    } else if !warnings.is_empty() {
        "ambiguous-reference"
    } else {
        "geometry"
    }
}

fn main() {
    let dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/mimi/kicad-scraper/dataset"));
    let limit = std::env::args()
        .nth(2)
        .map(|value| value.parse().expect("sample size is an integer"))
        .unwrap_or(100);
    let kicad =
        kicad::KicadInstallation::detect().expect("find KiCad 10; set KICAD_CLI if necessary");
    let mut matched = 0;
    let mut mismatched = 0;
    let mut unreadable = 0;

    for path in files(&dir, limit) {
        let doc = match SchDoc::read(&path) {
            Ok(doc) => doc,
            Err(error) => {
                unreadable += 1;
                println!("PARSE {}: {error}", path.display());
                continue;
            }
        };
        let extracted = connect::extract(&doc);
        let exported = match kicad.netlist(&path) {
            Ok(netlist) => netlist,
            Err(error) => {
                unreadable += 1;
                println!("KICAD {}: {error}", path.display());
                continue;
            }
        };
        let (mine, reference) = (ours(&extracted), oracle(&exported));
        if mine == reference {
            matched += 1;
            continue;
        }
        mismatched += 1;
        let missing: Vec<_> = reference.difference(&mine).take(3).collect();
        let extra: Vec<_> = mine.difference(&reference).take(3).collect();
        println!(
            "DIFF {} [{}]: ours={} kicad={} missing={missing:?} extra={extra:?} warnings={:?}",
            path.display(),
            reason(&doc, &extracted.warnings),
            mine.len(),
            reference.len(),
            extracted.warnings,
        );
    }
    println!(
        "SUMMARY sampled={} matched={matched} mismatched={mismatched} unreadable={unreadable}",
        matched + mismatched + unreadable
    );
}
