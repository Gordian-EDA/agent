//! The project's own input files, attached to the opening message.
//!
//! A case may ship a netlist or a spec beside the prompt ("draw the circuit in
//! `netlist.json`"). The model cannot read the project directory, so whatever the
//! prompt refers to has to travel with it — verbatim, because a netlist redraw is
//! judged pin for pin against the file it was given.

use std::path::Path;

/// Files that describe the answer rather than the task, or that a run writes itself.
const IGNORED: &[&str] = &["design.json", "report.json", "netlist.txt"];
const EXTENSIONS: &[&str] = &["json", "txt", "csv"];
const MAX_BYTES: usize = 200_000;

/// The prompt block naming and quoting every input file in `project_dir`, empty when
/// there are none.
pub fn prompt_block(project_dir: &Path) -> String {
    let mut files: Vec<(String, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return String::new();
    };
    let mut names: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    let mut budget = MAX_BYTES;
    for path in names {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !EXTENSIONS.contains(&ext) || IGNORED.contains(&name) || name.starts_with("reference.")
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.len() > budget {
            continue;
        }
        budget -= text.len();
        files.push((name.to_string(), text));
    }
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nPROJECT INPUT FILES\nThese files are in the project directory and are part of \
         the request. When one of them is a netlist or a part list, copy it into the design \
         JSON verbatim: the same refs, the same lib ids, the same values, and every pin \
         written exactly as the file writes it - if it keys pins by number, key them by \
         number; never substitute a pin name for a pin number, or the other way round. \
         Add no part and drop no part. A pin whose net is \"nc\" stays unconnected. Your \
         freedom is the layout trees, not the netlist. List every supply rail the circuit only \
         consumes - the ones no part on the sheet drives - in the design's \"flags\", or \
         KiCad's ERC reports each of them as a power pin that nothing drives.\n",
    );
    for (name, text) in files {
        out.push_str(&format!("\n--- {name} ---\n{}\n", text.trim_end()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_input_files_and_holds_back_the_reference() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("netlist.json"), r#"{"parts":[]}"#).unwrap();
        std::fs::write(dir.path().join("reference.kicad_sch"), "(kicad_sch)").unwrap();
        std::fs::write(dir.path().join("design.json"), "{}").unwrap();

        let block = prompt_block(dir.path());

        assert!(block.contains("--- netlist.json ---"));
        assert!(block.contains(r#"{"parts":[]}"#));
        assert!(!block.contains("reference"));
        assert!(!block.contains("--- design.json ---"));
    }

    #[test]
    fn an_empty_project_attaches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prompt_block(dir.path()).is_empty());
    }
}
