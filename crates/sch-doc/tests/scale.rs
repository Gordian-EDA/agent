//! The extractor runs inside an edit loop, so it has to stay fast on the
//! largest sheets in the corpus, not just the small ones.

mod corpus;

use std::time::Instant;

use sch_doc::{SchDoc, connect};

#[test]
fn the_busiest_sheet_extracts_quickly() {
    // Busiest by item count, not by bytes: a file can be mostly lib_symbols and
    // barely exercise the partitioner.
    let Some(path) = corpus::files().into_iter().max_by_key(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| SchDoc::parse(&text).ok())
            .map_or(0, |doc| doc.items().len())
    }) else {
        eprintln!("SKIP: corpus not found");
        return;
    };
    let source = std::fs::read_to_string(&path).expect("read");
    let started = Instant::now();
    let doc = SchDoc::parse(&source).expect("parse");
    let parsed = started.elapsed();
    let started = Instant::now();
    let netlist = connect::extract(&doc);
    let extracted = started.elapsed();

    eprintln!(
        "{} ({} KiB, {} items, {} nets): parse {parsed:?}, extract {extracted:?}",
        corpus::label(&path),
        source.len() / 1024,
        doc.items().len(),
        netlist.nets.len()
    );
    // Generous enough not to be flaky on a loaded machine, tight enough to
    // catch a quadratic sneaking into the node or wire handling.
    assert!(parsed.as_secs_f64() < 2.0, "parse took {parsed:?}");
    assert!(extracted.as_secs_f64() < 2.0, "extract took {extracted:?}");
}
