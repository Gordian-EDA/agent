//! Run the layout subagent (Layer 1) live against Bedrock for one fixture,
//! print the proposed Layout IR, then compile + render it (content-cropped).
//!
//! Usage: cargo run -p agent --example layout_spike -- <in.circuit.yaml> <out.png>

use circuit_lang::SymbolProvider;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: layout_spike <in.circuit.yaml> <out.png>");
    let out = args.next().expect("usage: layout_spike <in.circuit.yaml> <out.png>");

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let src = std::fs::read_to_string(&yaml)?;
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| anyhow::anyhow!("compile produced no design"))?;

    let client = agent::llm::from_env()?;
    let ir = agent::layout::propose_layout(&client, &env, &design).await?;
    eprintln!("--- proposed Layout IR ---\n{}\n", serde_json::to_string_pretty(&ir)?);
    // Persist the IR + emitted sch next to the PNG so the (non-deterministic) LLM
    // frame can be re-emitted deterministically while iterating on the engine.
    std::fs::write(
        std::path::Path::new(&out).with_extension("layout.json"),
        serde_json::to_string_pretty(&ir)?,
    )?;

    let emit = sch_engine::floorplan::emit(&env, &design, &ir)
        .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;
    std::fs::write(std::path::Path::new(&out).with_extension("kicad_sch"), emit.sch.as_bytes())?;
    for w in &emit.layout_warnings {
        eprintln!("layout-warning: {w}");
    }

    let tmp = tempfile::tempdir()?;
    let sch_path = tmp.path().join("out.kicad_sch");
    std::fs::write(&sch_path, emit.sch.as_bytes())?;
    match KicadCli::new(&env).erc(&sch_path) {
        Ok(r) => eprintln!("ERC: {} errors, {} warnings", r.error_count(), r.warning_count()),
        Err(e) => eprintln!("ERC failed: {e}"),
    }
    let svg_path = KicadCli::new(&env).export_svg_opts(&sch_path, tmp.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = agent::render::svg_to_png(&svg, 2000)?;
    std::fs::write(&out, png)?;
    println!("rendered {out}");
    Ok(())
}
