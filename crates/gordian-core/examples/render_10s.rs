use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let yaml_path = std::path::Path::new(".gordian/10s.circuit.yaml");
    let out = std::path::Path::new("/tmp/renders/ours-10s.png");
    std::fs::create_dir_all(out.parent().unwrap())?;
    let src = std::fs::read_to_string(yaml_path)?;
    let provider = SymbolTable::from_env(&env);
    let result = circuit_lang::compile(&src, &provider);
    let design = result.design.ok_or_else(|| anyhow::anyhow!("no design"))?;
    let emit = sch_floorplan::floorplan::emit_strategy(
        &env,
        &design,
        Box::new(anneal_place::Anneal),
        None,
    )
    .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;
    for w in &emit.layout_warnings {
        eprintln!("WARN: {w}");
    }
    std::fs::write(out.with_extension("kicad_sch"), emit.sch.as_bytes())?;
    let svg_dir = tempfile::tempdir()?;
    let tmp = tempfile::tempdir()?;
    let sch_path = tmp.path().join("out.kicad_sch");
    std::fs::write(&sch_path, emit.sch.as_bytes())?;
    KicadCli::new(&env).export_png(&sch_path, out, svg_dir.path(), 1600)?;
    println!("rendered {}", out.display());
    Ok(())
}
