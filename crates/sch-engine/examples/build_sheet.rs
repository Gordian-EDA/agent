//! `cargo run --example build_sheet -- design.json out.kicad_sch` — build one design and print its report.

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let design = args.next().expect("design.json");
    let out = args.next().expect("out.kicad_sch");
    let symbols = std::env::var("KICAD_SYMBOL_DIR")
        .unwrap_or_else(|_| "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols".into());
    let lib = sch_engine::Library::load(std::path::Path::new(&symbols))?;
    let design: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(design)?)?;
    let r = sch_engine::build(&lib, &design, std::path::Path::new(&out))?;
    println!("paper {}", r.paper);
    for (tag, list) in [("ISSUE", &r.issues), ("WARN", &r.warnings), ("NOTE", &r.notes)] {
        for m in list {
            println!("{tag}: {m}");
        }
    }
    Ok(())
}
