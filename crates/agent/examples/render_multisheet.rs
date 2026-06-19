//! Render a multi-BLOCK circuit as MULTI-SHEET: one clean .png per block.
//!
//! The single-sheet sprawl ceiling caps complete boards ~5-7; drawing each block on its own
//! sheet (the professional practice) lets each sheet score like the clean fixtures (9-10).
//! Compiles the design with the REAL parser (handles `between:`, `power:`, multi-line, units),
//! then for each block emits a single-block sub-design (shared nets auto-become labeled ports)
//! through the normal anneal path and renders it. Critic each PNG with tools/schematic_critic.py.
//!
//! Usage: cargo run --release -p agent --example render_multisheet -- <draft.yaml> <out_dir>

use circuit_lang::SymbolProvider;
use indexmap::IndexMap;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    let out_dir = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    std::fs::create_dir_all(&out_dir)?;

    // These ARE multi-sheet sub-sheets, so opt them into the route-aware crossing refinement
    // on the small path (a peripheral/bus sub-sheet tangles its port fanout; the refinement
    // takes e.g. an I2C sheet 16→13 xings and a power sheet 8→9). Single-sheet emit paths
    // (bench_corpus, agent_design) don't set this, so references stay byte-identical.
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(&yaml)?;
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    if design.blocks.len() < 2 {
        eprintln!("only {} block(s) — multi-sheet needs a multi-block design", design.blocks.len());
    }

    for (name, block) in &design.blocks {
        // A single-block sub-design: cross-block nets now touch only this block's pins, so the
        // engine auto-labels the single-pin ones as ports and keeps multi-pin ones internal.
        let mut sub = design.clone();
        sub.blocks = IndexMap::new();
        sub.blocks.insert(name.clone(), block.clone());

        // ADDITIVE ROUTED A/B (the documented crossmin path to a default win): emit with the
        // shelf-pack seed AND the crossmin (GLOBAL_OPT) seed, keep whichever ROUTES with fewer
        // (warnings, wire-crossings). The graph crossing count isn't faithful and the anneal
        // washes out a pure seed, so the choice must be at the routed level. min-of-two ⇒ never
        // regresses; captures crossmin's ~9% crossing wins on dense multi-anchor sub-sheets.
        let emit_with = |go: bool| {
            if go {
                unsafe { std::env::set_var("GLOBAL_OPT", "1") };
            } else {
                unsafe { std::env::remove_var("GLOBAL_OPT") };
            }
            let ir = sch_layout::floorplan::infer_ir(&env, &sub);
            sch_layout::floorplan::emit_anneal(&env, &sub, &ir)
        };
        let emit = match (emit_with(false), emit_with(true)) {
            (Ok(a), Ok(b)) => {
                let pick_b = (b.layout_warnings.len(), b.wire_crossings)
                    < (a.layout_warnings.len(), a.wire_crossings);
                println!(
                    "  [crossmin A/B {name}] shelf=({},{}) crossmin=({},{}) -> {}",
                    a.layout_warnings.len(), a.wire_crossings,
                    b.layout_warnings.len(), b.wire_crossings,
                    if pick_b { "crossmin" } else { "shelf" }
                );
                if pick_b { b } else { a }
            }
            (Ok(a), Err(_)) => a,
            (Err(_), Ok(b)) => b,
            (Err(e), Err(_)) => {
                eprintln!("{name}: emit failed: {e}");
                continue;
            }
        };
        unsafe { std::env::remove_var("GLOBAL_OPT") };
        let tmp = tempfile::tempdir()?;
        let sch = tmp.path().join("s.kicad_sch");
        std::fs::write(&sch, emit.sch.as_bytes())?;
        let svg_dir = tempfile::tempdir()?;
        let svg_path = KicadCli::new(&env).export_svg_opts(&sch, svg_dir.path(), true)?;
        let svg = std::fs::read_to_string(&svg_path)?;
        let png = agent::render::svg_to_png(&svg, 1600)?;
        let out_png = format!("{out_dir}/{name}.png");
        std::fs::write(&out_png, png)?;
        println!(
            "{name}: {} parts, {} warnings, {} wire-xings -> {out_png}",
            block.components.len(),
            emit.layout_warnings.len(),
            emit.wire_crossings
        );
        for w in &emit.layout_warnings {
            println!("    WARN[{name}]: {w}");
        }
    }
    Ok(())
}
