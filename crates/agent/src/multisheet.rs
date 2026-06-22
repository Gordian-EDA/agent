//! Multi-sheet (hierarchical) commit for DENSE designs — the validated answer to the
//! single-sheet density ceiling: one sheet per functional block scores 8-9 each, vs a
//! cramped single sheet at 4-7. Emits a committable KiCAD project (root `.kicad_sch` +
//! per-block sub-sheet files, global-label cross-sheet connectivity). See
//! `docs/specs/multisheet-commit.md`. Refined block-split/merge lives in the
//! `render_multisheet` example; this lean path is one-sheet-per-block (agent blocks are
//! already well-sized) — split/merge is a follow-up refinement.

use circuit_lang::model::Design;
use indexmap::IndexMap;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use std::path::{Path, PathBuf};

/// Deterministic UUIDv5-style id from a seed (no randomness ⇒ stable re-emits).
pub fn det_uuid(seed: &str) -> String {
    let h = |salt: u64| -> u64 {
        let mut h: u64 = 0xcbf29ce484222325 ^ salt;
        for b in seed.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    };
    let (a, b) = (h(1), h(2));
    format!(
        "{:08x}-{:04x}-5{:03x}-8{:03x}-{:012x}",
        (a & 0xffffffff) as u32,
        ((a >> 32) & 0xffff) as u16,
        ((a >> 48) & 0xfff) as u16,
        (b & 0xfff) as u16,
        (b >> 12) & 0xffffffffffff
    )
}

/// Sanitize a block name into a filename stem.
pub fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Emit a DENSE multi-block `Design` as a hierarchical KiCAD project under `out_dir`:
/// one sub-sheet per block + a root that references them. Returns the root `.kicad_sch`
/// path. Cross-block nets auto-become global labels (single-pin ports) / power symbols, so
/// the sheets connect. Sets `MULTISHEET_REFINE` so each sub-sheet gets the route-aware
/// crossing refinement (the validated sub-sheet path).
pub fn emit_multisheet(env: &KicadEnv, design: &Design, out_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    // SAFETY: process-wide flag read by the engine to opt sub-sheets into route-aware
    // refinement; this whole operation is a multi-sheet emit, so it's the intended scope.
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };
    let mut sheets: Vec<(String, String)> = Vec::new();
    for (bname, block) in &design.blocks {
        if block.components.is_empty() {
            continue;
        }
        let mut sub = design.clone();
        sub.blocks = IndexMap::new();
        sub.blocks.insert(bname.clone(), block.clone());
        let ir = sch_layout::floorplan::infer_ir(env, &sub);
        let emit = sch_layout::floorplan::emit_anneal(env, &sub, &ir)
            .map_err(|e| anyhow::anyhow!("emit block '{bname}': {e}"))?;
        sheets.push((sanitize(bname), emit.sch));
    }
    dedup_pwr_flags(&mut sheets);
    write_project(env, out_dir, &sheets)
}

/// Each sub-sheet emits its OWN `PWR_FLAG` for the power nets it uses; across sheets the
/// same net then has multiple "power output" pins → KiCAD ERC error ("power output connected
/// to power output"). Two rules clean this up while preserving connectivity:
///  - **DRIVEN nets** (a regulator/source drives them — e.g. 3V3 off an LDO): the net appears
///    on some sheet that has NO flag for it (the engine doesn't flag a driven net). Such a net
///    needs NO flag at all → strip EVERY `PWR_FLAG` for it (the flag would conflict with the
///    real driver's power-output pin).
///  - **UNDRIVEN nets** (raw rails off a connector — e.g. VBUS, GND: flagged on every sheet
///    that uses them): keep exactly ONE `PWR_FLAG` globally, strip the duplicates.
fn dedup_pwr_flags(sheets: &mut [(String, String)]) {
    use std::collections::HashSet;
    // Strip a trailing 4-digit instance suffix: `VMOTOR0101` → `VMOTOR`, but keep `3V3`.
    let base = |raw: &str| -> String {
        if raw.len() > 4 && raw[raw.len() - 4..].bytes().all(|c| c.is_ascii_digit()) {
            raw[..raw.len() - 4].to_string()
        } else {
            raw.to_string()
        }
    };
    // 1. Every net that has a PWR_FLAG anywhere.
    let mut flag_nets: HashSet<String> = HashSet::new();
    for (_, sch) in sheets.iter() {
        let mut rest = sch.as_str();
        while let Some(p) = rest.find("\"#FLG_") {
            let after = &rest[p + 6..];
            if let Some(e) = after.find('"') {
                flag_nets.insert(base(&after[..e]));
            }
            rest = after;
        }
    }
    // 2. DRIVEN = some sheet references the net (`"<net>"`) but carries no flag for it.
    let mut driven: HashSet<String> = HashSet::new();
    for net in &flag_nets {
        let needle = format!("\"{net}\"");
        let flag_pfx = format!("\"#FLG_{net}");
        if sheets.iter().any(|(_, s)| s.contains(&needle) && !s.contains(&flag_pfx)) {
            driven.insert(net.clone());
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    // End index (exclusive) of the balanced `(symbol …)` block starting at `s[0..]`.
    let block_end = |s: &str| -> usize {
        let b = s.as_bytes();
        let (mut depth, mut in_str, mut started) = (0i32, false, false);
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'"' => in_str = !in_str,
                b'(' if !in_str => {
                    depth += 1;
                    started = true;
                }
                b')' if !in_str => {
                    depth -= 1;
                    if started && depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        s.len()
    };
    // Net a PWR_FLAG drives, parsed from its `#FLG_<net><4-digit-instance?>` reference.
    let flag_net = |block: &str| -> Option<String> {
        let r = block.find("\"Reference\"")?;
        let q = block[r..].find("\"#FLG_")? + r + 6;
        let raw = &block[q..block[q..].find('"').map(|e| q + e)?];
        Some(if raw.len() > 4 && raw[raw.len() - 4..].bytes().all(|c| c.is_ascii_digit()) {
            raw[..raw.len() - 4].to_string()
        } else {
            raw.to_string()
        })
    };
    for (_, sch) in sheets.iter_mut() {
        let mut out = String::with_capacity(sch.len());
        let mut rest = sch.as_str();
        while let Some(pos) = rest.find("\t(symbol\n") {
            out.push_str(&rest[..pos]);
            let end = block_end(&rest[pos..]);
            let block = &rest[pos..pos + end];
            let drop = block.contains("power:PWR_FLAG")
                && flag_net(block).map(|n| driven.contains(&n) || !seen.insert(n)).unwrap_or(false);
            if !drop {
                out.push_str(block);
            }
            rest = &rest[pos + end..];
        }
        out.push_str(rest);
        *sch = out;
    }
}

/// Write a hierarchical KiCAD project (root + sub-sheet files) from per-block emitted
/// schematics. Rewrites each sub-sheet's symbol instance-paths into the hierarchy
/// (`/SUB_ROOT` → `/MAIN_ROOT/SHEET_UUID`). Returns the root path.
pub fn write_project(
    env: &KicadEnv,
    out_dir: &Path,
    sheets: &[(String, String)],
) -> anyhow::Result<PathBuf> {
    use std::fmt::Write as _;
    let dir = out_dir.to_string_lossy().to_string();
    let main_root = det_uuid(&format!("root:{dir}"));
    let mut root = String::new();
    root.push_str("(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t(generator_version \"9.0\")\n");
    let _ = writeln!(root, "\t(uuid \"{main_root}\")");
    root.push_str("\t(paper \"A4\")\n\t(lib_symbols\n\t)\n");
    let mut inst = String::from("\t(sheet_instances\n\t\t(path \"/\"\n\t\t\t(page \"1\")\n\t\t)\n");
    for (i, (name, sch)) in sheets.iter().enumerate() {
        let page = i + 2;
        let sheet_uuid = det_uuid(&format!("{dir}:{name}"));
        let sub_root =
            sch.split("(uuid \"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("").to_string();
        let mut sub = sch.replace(&format!("/{sub_root}\""), &format!("/{main_root}/{sheet_uuid}\""));
        sub = sub.replacen("(path \"/\"", &format!("(path \"/{sheet_uuid}\""), 1);
        sub = sub.replacen("(page \"1\")", &format!("(page \"{page}\")"), 1);
        std::fs::write(out_dir.join(format!("{name}.kicad_sch")), &sub)?;
        let (x, y) = (25.4 + (i % 4) as f64 * 55.0, 25.4 + (i / 4) as f64 * 35.0);
        let (ny, fy) = (y - 0.7, y + 18.6);
        let _ = write!(
            root,
            "\t(sheet\n\t\t(at {x} {y})\n\t\t(size 35 18)\n\t\t(fields_autoplaced yes)\n\t\t(stroke (width 0.1524) (type solid))\n\t\t(fill (color 0 0 0 0.0000))\n\t\t(uuid \"{sheet_uuid}\")\n\t\t(property \"Sheetname\" \"{name}\"\n\t\t\t(at {x} {ny} 0)\n\t\t\t(effects (font (size 1.27 1.27) (bold yes)) (justify left bottom))\n\t\t)\n\t\t(property \"Sheetfile\" \"{name}.kicad_sch\"\n\t\t\t(at {x} {fy} 0)\n\t\t\t(effects (font (size 1.27 1.27)) (justify left top) (hide yes))\n\t\t)\n\t\t(instances\n\t\t\t(project \"root\"\n\t\t\t\t(path \"/{main_root}\"\n\t\t\t\t\t(page \"{page}\")\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n"
        );
        let _ = write!(inst, "\t\t(path \"/{sheet_uuid}\"\n\t\t\t(page \"{page}\")\n\t\t)\n");
    }
    inst.push_str("\t)\n");
    root.push_str(&inst);
    root.push_str(")\n");
    let root_path = out_dir.join("root.kicad_sch");
    std::fs::write(&root_path, &root)?;
    let _ = env; // reserved (validation hook); kept for signature symmetry
    Ok(root_path)
}

/// Convenience: emit + run ERC, returning (root_path, erc_errors, erc_warnings).
pub fn emit_and_check(
    env: &KicadEnv,
    design: &Design,
    out_dir: &Path,
) -> anyhow::Result<(PathBuf, usize, usize)> {
    let root = emit_multisheet(env, design, out_dir)?;
    let (e, w) = match KicadCli::new(env).erc(&root) {
        Ok(r) => (r.error_count(), r.warning_count()),
        Err(_) => (usize::MAX, 0),
    };
    Ok((root, e, w))
}
