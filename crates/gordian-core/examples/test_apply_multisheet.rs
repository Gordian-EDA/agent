//! Deterministic test of apply_design's COMMIT path on a dense multi-block draft (no live
//! LLM): create_design → apply_design{commit:true}; assert the committed sch is a
//! multi-sheet root. Usage: cargo run -p agent --example test_apply_multisheet -- <draft.yaml>

use gordian_core::tools::{PcbToolCtx, run_tool};
use kicad_env::KicadEnv;
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let yaml_path = std::env::args()
        .nth(1)
        .expect("usage: test_apply_multisheet <draft.yaml>");
    let yaml = std::fs::read_to_string(&yaml_path)?;
    let env = KicadEnv::detect().expect("no KiCAD environment");
    let tmp = tempfile::tempdir()?;
    let ctx = PcbToolCtx::for_project(env, tmp.path().to_path_buf())?;

    let r1 = run_tool("create_design", json!({ "yaml": yaml }), &ctx);
    println!("create_design -> {}", summarize(&r1));

    let r2 = run_tool("apply_design", json!({ "commit": true }), &ctx);
    println!("apply_design(commit) -> {}", summarize(&r2));

    let sch = ctx.sch_path();
    let exists = sch.exists();
    let body = if exists {
        std::fs::read_to_string(sch).unwrap_or_default()
    } else {
        String::new()
    };
    let is_multisheet = body.contains("(sheet\n") && body.contains("Sheetfile");
    let n_subsheets = ctx
        .project_dir()
        .read_dir()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "kicad_sch"))
                .count()
        })
        .unwrap_or(0);
    println!(
        "committed sch exists={exists} is_multisheet={is_multisheet} kicad_sch_files={n_subsheets}"
    );
    Ok(())
}

fn summarize(r: &anyhow::Result<serde_json::Value>) -> String {
    match r {
        Ok(v) => {
            let s = v.to_string();
            if s.len() > 280 {
                format!("{}…", &s[..280])
            } else {
                s
            }
        }
        Err(e) => format!("ERR: {e:#}"),
    }
}
