//! The "does this read as a drawn schematic" gate, over the whole validation corpus.
//!
//! Three properties a human sheet has and a machine sheet does not, each measured on the
//! emitted `.kicad_sch` rather than judged: the drawing is inside the page, the page is
//! one people use, and no single wire runs across the sheet. They are here because the
//! defects they catch are silent — a part drawn off the page renders as nothing at all,
//! and every renderer agrees with the file. SKIPs without KiCAD.

use std::path::Path;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use sch_model::engine::PlacementEngine;

/// Longest wire the realiser may draw (mm): the rail cap and the signal-label threshold
/// are the same corpus rule, plus the elbow router's detour around one body.
const LONGEST_WIRE: f64 = 60.0;

/// The fixtures whose PLACEMENT is still one long row, so no page holds them. Their width
/// comes from `spine-place`'s row ordering, not from anything the writer or the page
/// fitter decides — a sheet 938 mm wide is a placement that never folded. Listed rather
/// than tolerated: the assertion is EQUALITY, so a new oversize sheet fails here and so
/// does a fixed one, which is what makes the list shrink.
const OVERSIZE: [&str; 2] = ["campaign-stm32-buck", "esp32-multifunction"];

fn corpus() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

fn fixtures() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(corpus())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".place-parts.json")
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

fn emit(env: &KicadInstallation, provider: &SymbolTable, name: &str) -> String {
    let src = std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
    let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
    let (design, diags, _) = sch_check::into_design(&input, provider, &Default::default());
    assert!(!diags.has_errors(), "{name}: {diags:#?}");
    let ir = input
        .intent
        .clone()
        .map(sch_check::Intent::into_layout_ir)
        .unwrap_or_else(|| floorplan::baseline_ir(&design));
    let engine: Box<dyn PlacementEngine> = Box::new(spine_place::SpinePlace);
    floorplan::emit_strategy(env, &design, engine, Some(ir))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .sch
}

/// Every `(at x y …)` and `(xy x y)` outside `(lib_symbols …)`, i.e. sheet-space ink.
fn sheet_points(sch: &str) -> Vec<[f64; 2]> {
    let body = match sch.find("(lib_symbols") {
        Some(start) => {
            let end = start + matching(&sch[start..]);
            format!("{}{}", &sch[..start], &sch[end..])
        }
        None => sch.to_string(),
    };
    let mut out = Vec::new();
    for tag in ["(at ", "(xy "] {
        let mut rest = body.as_str();
        while let Some(i) = rest.find(tag) {
            rest = &rest[i + tag.len()..];
            let mut nums = rest.split_whitespace();
            let parsed = nums.next().zip(nums.next()).and_then(|(x, y)| {
                Some([
                    x.parse::<f64>().ok()?,
                    y.trim_end_matches(')').parse::<f64>().ok()?,
                ])
            });
            out.extend(parsed);
        }
    }
    out
}

/// Length of the balanced s-expression starting at `text[0]`.
fn matching(text: &str) -> usize {
    let (mut depth, mut quoted, mut escaped) = (0i32, false, false);
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// The declared page, and `None` when it is not one of [`sch_doc::STANDARD_PAGES`].
fn page(sch: &str) -> (String, Option<[f64; 2]>) {
    let line = sch
        .lines()
        .find(|l| l.trim_start().starts_with("(paper"))
        .expect("every sheet declares a page")
        .trim()
        .to_string();
    for (name, size) in sch_doc::STANDARD_PAGES {
        if line.contains(&format!("\"{name}\"")) {
            return (line, Some(size));
        }
    }
    (line, None)
}

fn wires(sch: &str) -> Vec<f64> {
    sch.split("(wire")
        .skip(1)
        .filter_map(|w| {
            let pts = sheet_points(w.split("(uuid").next()?);
            let (a, b) = (pts.first()?, pts.get(1)?);
            Some(((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt())
        })
        .collect()
}

#[test]
fn every_sheet_is_drawn_inside_a_standard_page_with_no_wire_across_it() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let (mut faults, mut oversize) = (Vec::new(), Vec::new());
    for name in fixtures() {
        let sch = emit(&env, &provider, &name);
        let (declared, standard) = page(&sch);
        let Some([w, h]) = standard else {
            oversize.push(name.clone());
            continue;
        };
        if OVERSIZE.contains(&name.as_str()) {
            faults.push(format!(
                "{name}: fits {declared} now — drop it from OVERSIZE"
            ));
        }
        let points = sheet_points(&sch);
        let off: Vec<[f64; 2]> = points
            .iter()
            .copied()
            .filter(|p| p[0] < 0.0 || p[1] < 0.0 || p[0] > w || p[1] > h)
            .collect();
        if !off.is_empty() {
            faults.push(format!(
                "{name}: {} points outside the {w}x{h} page, e.g. {:?}",
                off.len(),
                &off[..off.len().min(3)]
            ));
        }
        let longest = wires(&sch).into_iter().fold(0.0, f64::max);
        if longest > LONGEST_WIRE {
            faults.push(format!("{name}: {longest:.1} mm wire"));
        }
    }
    assert_eq!(
        oversize, OVERSIZE,
        "the set of sheets no standard page holds has changed"
    );
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}
