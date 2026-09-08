//! temporary diagnostic
use std::path::{Path, PathBuf};
#[test]
fn dump() {
    let dir = PathBuf::from("/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols");
    let lib = sch_engine::Library::load(&dir).unwrap();
    let f = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ne555_netlist");
    let design: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f.join("design.json")).unwrap()).unwrap();
    let out = PathBuf::from("/tmp/claude-1000/-home-mimi-agent/76f1e5cf-bf11-4eae-90a0-a01abe27430c/scratchpad/rust_ne555.kicad_sch");
    let r = sch_engine::build(&lib, &design, &out).unwrap();
    std::fs::write("/tmp/claude-1000/-home-mimi-agent/76f1e5cf-bf11-4eae-90a0-a01abe27430c/scratchpad/rust_ne555_raw.json",
        serde_json::to_string_pretty(&r.raw).unwrap()).unwrap();
    println!("paper={} issues={:?}", r.paper, r.issues);
}
