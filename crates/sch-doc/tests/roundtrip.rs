//! Gate 1 — nothing is lost on the way out.
//!
//! For every corpus schematic: writing a freshly parsed document is a fixed
//! point of parsing, and KiCAD itself agrees, seeing the same net partition and
//! the same ERC counts before and after the rewrite.

mod corpus;

use sch_doc::SchDoc;

/// Files KiCAD 8+ wrote come back byte for byte. A file still in the older
/// layout (leading atoms on the root line) is re-laid-out in the modern dialect
/// — lossless, but not byte-stable, so it is exempt.
#[test]
fn write_reproduces_the_input_byte_for_byte() {
    let files = corpus::files();
    assert!(!files.is_empty(), "corpus not found");
    let mut differing = Vec::new();
    let mut compared = 0;
    for path in &files {
        let source = std::fs::read_to_string(path).expect("read");
        if !source.starts_with("(kicad_sch\n") {
            continue;
        }
        compared += 1;
        let doc = SchDoc::parse(&source).expect("parse");
        if doc.to_text() != source {
            differing.push(corpus::label(path));
        }
    }
    assert!(compared > 100, "only {compared} files were in the modern layout");
    assert!(
        differing.is_empty(),
        "{} of {} files did not round-trip byte-identically: {:?}",
        differing.len(),
        files.len(),
        &differing[..differing.len().min(8)]
    );
}

#[test]
fn rewriting_is_a_fixed_point_of_the_model() {
    for path in corpus::files() {
        let once = SchDoc::read(&path).expect("parse");
        let text = once.to_text();
        let twice = SchDoc::parse(&text).expect("reparse");
        assert_eq!(
            twice.to_text(),
            text,
            "{} is not a fixed point",
            corpus::label(&path)
        );
        assert_eq!(
            twice.items().len(),
            once.items().len(),
            "{} lost items",
            corpus::label(&path)
        );
    }
}

/// Force every item through the pretty-printer rather than its retained bytes,
/// which is the path an edited item takes.
#[test]
fn reprinted_items_survive_a_reparse() {
    for path in corpus::files() {
        let doc = SchDoc::read(&path).expect("parse");
        let mut text = String::from("(kicad_sch\n");
        for item in doc.items() {
            sch_doc::print_item(item, &mut text);
        }
        text.push_str(")\n");
        let reparsed = SchDoc::parse(&text).expect("reparse of printed form");
        assert_eq!(
            reparsed.items().len(),
            doc.items().len(),
            "{} lost items when printed",
            corpus::label(&path)
        );
        let mut again = String::from("(kicad_sch\n");
        for item in reparsed.items() {
            sch_doc::print_item(item, &mut again);
        }
        again.push_str(")\n");
        assert_eq!(
            again,
            text,
            "{} printer is not a fixed point",
            corpus::label(&path)
        );
    }
}

#[test]
fn kicad_sees_the_same_netlist_and_erc_after_a_rewrite() {
    let Some(kicad) = corpus::kicad10() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let mut checked = 0;
    for path in corpus::files() {
        let doc = SchDoc::read(&path).expect("parse");
        // Compare two copies of the same project so only the rewrite differs:
        // sibling sheets and the project file drive ERC severities as well.
        let (original_dir, original) = stage(&path);
        let (rewritten_dir, rewritten) = stage(&path);
        std::fs::write(&rewritten, doc.to_text()).expect("write");

        let Ok(before) = kicad.netlist(&original) else {
            continue;
        };
        let after = kicad.netlist(&rewritten).expect("netlist of rewrite");
        assert_eq!(
            partition(&before),
            partition(&after),
            "{} net partition changed",
            corpus::label(&path)
        );
        if let (Ok(before), Ok(after)) = (kicad.erc(&original), kicad.erc(&rewritten)) {
            assert_eq!(
                (before.error_count(), before.warning_count()),
                (after.error_count(), after.warning_count()),
                "{} ERC counts changed",
                corpus::label(&path)
            );
        }
        drop((original_dir, rewritten_dir));
        checked += 1;
    }
    assert!(checked > 50, "only {checked} files reached the oracle");
}

/// Copy the whole project directory into a scratch dir and hand back the copy
/// of `path` inside it.
fn stage(path: &std::path::Path) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let source_dir = path.parent().expect("parent");
    for entry in std::fs::read_dir(source_dir).expect("read_dir").flatten() {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), dir.path().join(entry.file_name())).expect("copy");
        }
    }
    let staged = dir.path().join(path.file_name().expect("file name"));
    (dir, staged)
}

fn partition(netlist: &kicad::Netlist) -> Vec<Vec<(String, String)>> {
    let mut out: Vec<Vec<(String, String)>> = netlist
        .nets
        .iter()
        .map(|net| {
            let mut nodes = net.nodes.clone();
            nodes.sort();
            nodes
        })
        .collect();
    out.sort();
    out
}
