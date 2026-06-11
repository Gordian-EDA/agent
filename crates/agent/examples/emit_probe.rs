//! Probe: emit one validation fixture, print layout warnings (debug helper).
use circuit_lang::SymbolProvider;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: emit_probe <fixture.yaml>");
    let env = KicadEnv::detect().expect("env");
    let src = std::fs::read_to_string(&path)?;
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.expect("design");
    let out = sch_engine::emit_design(&env, &design).map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write("/tmp/probe.kicad_sch", out.sch.as_bytes())?;
    for w in &out.layout_warnings {
        println!("WARN: {w}");
    }
    Ok(())
}
