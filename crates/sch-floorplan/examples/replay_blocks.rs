//! `replay_blocks` — render every validation fixture the way the AGENT builds a sheet:
//! one `place_parts` call per block, onto a sheet that already holds the blocks before it.
//!
//! [`super::render_corpus`] emits each fixture in a single whole-sheet pass, which is not
//! how a sheet is actually built. `crates/gordian-core/src/prompts.rs` asks the model for
//! one call per functional section of three to twelve parts, so a real sheet is ten or
//! fifteen sequential seatings, each seeing only what is already down. Every defect that
//! lives in the SEAM between two calls is therefore invisible to the whole-sheet corpus:
//! two of them — block seating that grew the page against the whole hull, and a declared
//! net silently dropped when it has one pin in its block — sat undetected behind it.
//!
//! ```text
//! cargo run --release -p sch-floorplan --example replay_blocks -- <out-dir> [name…]
//! ```
//!
//! Per fixture it reports the block count, the symbols placed, the page, the labels and
//! wires drawn, and — the seam check — the engine's own truthfulness verdict on the
//! FINISHED sheet: nets whose pins ended up scattered over more than one, and nets fused
//! into one. Each call already gates on the block it draws, so only the finished sheet
//! can show a net lost in the seam between two calls.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;
use sch_check::PlacePartsInput;
use sch_doc::SchDoc;
use sch_floorplan::live;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

/// The fixture's blocks in the order its parts first mention them — the order the model
/// would place them in, which is the order that decides what each seating can see.
///
/// A part that names no block joins the payload's own, exactly as `place_parts` reads it.
/// Deriving the name from the part alone dropped every payload-level block on the floor,
/// and with it the authored tree keyed by that name: two thirds of the corpus replayed as
/// untreed bare rows.
fn blocks(value: &serde_json::Value) -> Vec<String> {
    let mut seen = Vec::new();
    for part in value["parts"].as_array().into_iter().flatten() {
        let block = block_of(value, part);
        if !seen.contains(&block) {
            seen.push(block);
        }
    }
    seen
}

fn block_of(value: &serde_json::Value, part: &serde_json::Value) -> String {
    part["block"]
        .as_str()
        .or_else(|| value["block"].as_str())
        .unwrap_or(sch_check::DEFAULT_BLOCK)
        .to_string()
}

/// One call's payload: the fixture narrowed to `block`, carrying that block's layout tree
/// and — on the first call only — the sheet-wide intent.
fn payload(value: &serde_json::Value, block: &str, first: bool) -> serde_json::Value {
    let parts: Vec<serde_json::Value> = value["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| block_of(value, part) == block)
        .cloned()
        .collect();
    let mut out = serde_json::json!({
        "name": value["name"].as_str().unwrap_or("replay"),
        "block": block,
        "parts": parts,
    });
    if let Some(tree) = value["layout"].get(block) {
        out["layout"] = serde_json::json!({ block: tree });
    }
    if first && let Some(intent) = value.get("intent") {
        out["intent"] = intent.clone();
    }
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("usage: replay_blocks <out-dir> [name…]"));
    let only: Vec<String> = args.collect();
    std::fs::create_dir_all(&out).unwrap();
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment");
        return;
    };
    let provider = kicad_symbol::SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());

    let mut names: Vec<String> = std::fs::read_dir(corpus())
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".place-parts.json").map(str::to_string)
        })
        .collect();
    names.sort();

    for name in names {
        if !only.is_empty() && !only.contains(&name) {
            continue;
        }
        let source = std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json")));
        let Ok(source) = source else { continue };
        let value: serde_json::Value = serde_json::from_str(&source).unwrap();
        // The design the fixture DECLARES, built once, so the finished sheet can be asked
        // whether it drew it.
        let whole: PlacePartsInput = serde_json::from_str(&source).unwrap();
        let (design, diags, _) = sch_check::into_design(&whole, &provider, &Default::default());
        if diags.has_errors() {
            eprintln!("{name}: {diags:#?}");
            continue;
        }

        let mut doc = live::blank_sheet().unwrap();
        let mut rolled_back = Vec::new();
        let order = blocks(&value);
        for (i, block) in order.iter().enumerate() {
            let call = payload(&value, block, i == 0);
            let input: PlacePartsInput = match serde_json::from_value(call) {
                Ok(input) => input,
                Err(error) => {
                    rolled_back.push(format!("{block}: {error}"));
                    continue;
                }
            };
            match live::place_parts(&env, &mut doc, &input) {
                Ok(report) if report.committed => {}
                Ok(report) => rolled_back.push(format!("{block}: {:?}", report.mismatch)),
                Err(error) => rolled_back.push(format!("{block}: {error}")),
            }
            if std::env::var_os("REPLAY_STEPS").is_some() {
                doc.write(&out.join(format!("{name}.{i}.kicad_sch"))).unwrap();
            }
        }

        let path = out.join(format!("{name}.kicad_sch"));
        doc.write(&path).unwrap();
        report(&design, &name, &doc, order.len(), &rolled_back);
    }
}

fn report(
    design: &sch_check::Design,
    name: &str,
    doc: &SchDoc,
    blocks: usize,
    rolled_back: &[String],
) {
    let symbols = doc.symbols().filter(|s| !s.refdes().starts_with('#')).count();
    let labels = doc.labels().count();
    let wires = doc.wires().count();
    let page = doc
        .page()
        .map(|p| format!("{:.0}x{:.0}", p[0], p[1]))
        .unwrap_or_else(|| "?".into());
    // The seam check. `live::verify` is the engine's own truthfulness oracle — every
    // intended net's pins on one extracted net, no two intended nets fused — asked of the
    // FINISHED sheet rather than of one call. That distinction is the whole point: each
    // `place_parts` gates on the block it is drawing, where a net with one pin so far has
    // nothing to be scattered from, so a net dropped in the SEAM between two calls passes
    // every per-call gate and only shows up here.
    let mismatch = live::verify(doc, design);
    // A symbol landed on another symbol is never a legal sheet: the seatings each see
    // only what is already down, so this is the other seam defect the whole-sheet corpus
    // cannot show.
    let overlaps = sch_floorplan::visual::measure(doc).body_overlaps;
    // How much of the sheet the drawing actually uses: the pieces' own frames against
    // the hull they span. A sheet built one call at a time is sparse precisely here.
    let pieces = sch_floorplan::reseat::pieces(doc).unwrap_or_default();
    let area = |r: &geom::Rect| r.width() * r.height();
    let ink: f64 = pieces.iter().map(|p| area(&p.frame)).sum();
    let hull = doc.content_bbox().map(|r| area(&r)).unwrap_or(0.0);
    let fill = if hull > 0.0 { 100.0 * ink / hull } else { 0.0 };
    let rects = doc
        .items()
        .iter()
        .filter(|item| matches!(item, sch_doc::Item::Rectangle(_)))
        .count();
    let captions = doc
        .items()
        .iter()
        .filter(|item| matches!(item, sch_doc::Item::Text(_)))
        .count();
    // Blocks meant as one row or column whose edges do not meet: pairs of pieces whose
    // tops (or lefts) differ by more than a grid step and less than a band.
    let framed: Vec<&geom::Rect> = pieces
        .iter()
        .filter(|p| !p.blocks.is_empty())
        .map(|p| &p.frame)
        .collect();
    let off = |a: f64, b: f64| (a - b).abs() > 1.27 + 0.01 && (a - b).abs() <= 15.24;
    let mut misaligned = 0;
    for (i, a) in framed.iter().enumerate() {
        for b in framed.iter().skip(i + 1) {
            if off(a.min_y, b.min_y) || off(a.min_x, b.min_x) {
                misaligned += 1;
            }
        }
    }
    print!(
        "{name}: blocks={blocks} symbols={symbols} page={page} hull={hull:.0} fill={fill:.0}% \
         pieces={} frames={rects} captions={captions} labels={labels} wires={wires} \
         misaligned={misaligned} scattered={} shorted={} body_overlaps={}",
        pieces.len(),
        mismatch.scattered.len(),
        mismatch.shorted.len(),
        overlaps.len()
    );
    if !overlaps.is_empty() {
        print!(" overlapping={overlaps:?}");
    }
    if !mismatch.scattered.is_empty() {
        let mut show = mismatch.scattered.clone();
        show.truncate(4);
        print!(" e.g.={show:?}");
    }
    if !rolled_back.is_empty() {
        print!(" ROLLED_BACK={rolled_back:?}");
    }
    println!();
}
