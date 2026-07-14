//! Composed single-sheet commit — each authored functional block is laid out
//! independently, then the blocks are tiled onto ONE `.kicad_sch` as labeled
//! bounding-box regions. Cross-block nets connect via global labels only.

use circuit_lang::model::{Block, Design, PinTarget};
use indexmap::IndexMap;
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use sch_place::result::EmitOutput;
use std::collections::{HashMap, HashSet};
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
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The non-GND nets a block touches (component- and unit-level pins). A net shared by ≥2
/// blocks is a cross-block PORT; GND/VSS are excluded (every sheet carries them, so they'd
/// drown out the signal coupling that drives the merge target).
fn block_nets(b: &Block) -> HashSet<String> {
    let mut s = HashSet::new();
    let mut add = |t: &PinTarget| {
        if let PinTarget::Net(n) = t {
            let u = n.to_ascii_uppercase();
            if u != "GND" && !u.starts_with("GND") && u != "VSS" {
                s.insert(n.clone());
            }
        }
    };
    for c in b.components.values() {
        for t in c.pins.values() {
            add(t);
        }
        for unit in c.units.values() {
            for t in unit.values() {
                add(t);
            }
        }
    }
    s
}

/// One sheet group: a block name and its single authored block body.
pub type SheetGroup = (String, Block);

/// Non-empty authored blocks, one group each, in declaration order. A block of
/// ONLY `power:` symbols declares rails, not layout — it would tile as an empty
/// frame, so it joins no group (the symbols realize at their usage sites).
pub fn authored_groups(blocks: &IndexMap<String, Block>) -> Vec<SheetGroup> {
    blocks
        .iter()
        .filter(|(_, block)| {
            block
                .components
                .values()
                .any(|c| !c.part.starts_with("power:"))
        })
        .map(|(name, block)| (name.clone(), block.clone()))
        .collect()
}

/// Lay out `design` block-by-block and compose one `.kicad_sch`.
pub fn compose_design(env: &KicadEnv, design: &Design) -> anyhow::Result<EmitOutput> {
    let groups = authored_groups(&design.blocks);
    if groups.is_empty() {
        return sch_floorplan::floorplan::emit_strategy(
            env,
            design,
            crate::tools::schematic_placement_engine(),
            None,
        )
        .map_err(|e| anyhow::anyhow!("emit: {e}"));
    }

    let cross_sheet = cross_sheet_nets(&groups);
    let mut groups_w: Vec<(String, sch_io::write::SchematicWriter)> = Vec::new();
    let mut layout_warnings = Vec::new();
    let mut crossings = sch_place::place::Crossings::default();
    let mut detected_idioms = Vec::new();
    let placer = crate::tools::schematic_placement_engine();
    for (gname, block) in groups {
        let mut sub = design.clone();
        sub.blocks = std::iter::once((gname.clone(), block)).collect();
        mark_cross_sheet_ports(&mut sub, &cross_sheet);
        eprintln!("  [emit] group '{gname}' with {}", placer.name());
        let (w, out) = sch_floorplan::floorplan::emit_group(
            env,
            &sub,
            crate::tools::schematic_placement_engine(),
        )
        .map_err(|e| anyhow::anyhow!("emit group '{gname}': {e}"))?;
        eprintln!("  [emit] group '{gname}' done");
        layout_warnings.extend(out.layout_warnings);
        crossings.body += out.crossings.body;
        crossings.ic += out.crossings.ic;
        crossings.wire += out.crossings.wire;
        detected_idioms.extend(out.detected_idioms);
        groups_w.push((gname, w));
    }

    Ok(EmitOutput {
        sch: sch_floorplan::floorplan::compose_writers(groups_w, design.name.as_deref()),
        layout_warnings,
        crossings,
        detected_idioms,
    })
}

/// Emit a multi-block `Design` as ONE composed `.kicad_sch` under `out_dir` (file
/// `root.kicad_sch`). Returns the composed `.kicad_sch` path.
pub fn compose_single_sheet(
    env: &KicadEnv,
    design: &Design,
    out_dir: &Path,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let composed = compose_design(env, design)?;
    let path = out_dir.join("root.kicad_sch");
    std::fs::write(&path, &composed.sch)?;
    Ok(path)
}

/// Nets that CROSS sheets: a signal net present in ≥2 sheet GROUPS.
pub fn cross_sheet_nets(groups: &[SheetGroup]) -> HashSet<String> {
    let mut net_groups: HashMap<String, usize> = HashMap::new();
    for (_, block) in groups {
        let nets: HashSet<String> = block_nets(block).into_iter().collect();
        for net in nets {
            *net_groups.entry(net).or_default() += 1;
        }
    }
    net_groups
        .into_iter()
        .filter(|(_, c)| *c >= 2)
        .map(|(n, _)| n)
        .collect()
}

/// Mark every cross-sheet net that `sub` touches as a PORT.
pub fn mark_cross_sheet_ports(sub: &mut Design, cross_sheet: &HashSet<String>) {
    let touched: HashSet<String> = sub
        .blocks
        .values()
        .flat_map(block_nets)
        .filter(|n| cross_sheet.contains(n))
        .collect();
    for net in touched {
        sub.nets.entry(net).or_default().port = true;
    }
}

/// Convenience: compose + run ERC, returning (sheet_path, erc_errors, erc_warnings).
pub fn emit_and_check(
    env: &KicadEnv,
    design: &Design,
    out_dir: &Path,
) -> anyhow::Result<(PathBuf, usize, usize)> {
    let root = compose_single_sheet(env, design, out_dir)?;
    let (e, w) = match KicadCli::new(env).erc(&root) {
        Ok(r) => (r.error_count(), r.warning_count()),
        Err(_) => (usize::MAX, 0),
    };
    Ok((root, e, w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use circuit_lang::model::{Component, PinTarget};

    #[test]
    fn authored_groups_one_per_block() {
        let mut block = Block::default();
        for i in 1..=12 {
            let mut c = Component {
                part: "Device:R".to_owned(),
                ..Component::default()
            };
            c.pins
                .insert("1".to_owned(), PinTarget::Net(format!("N{i}")));
            c.pins
                .insert("2".to_owned(), PinTarget::Net(format!("N{}", i + 1)));
            block.components.insert(format!("R{i}"), c);
        }

        let mut blocks = IndexMap::new();
        blocks.insert("main".to_owned(), block);

        let groups = authored_groups(&blocks);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "main");
        assert_eq!(groups[0].1.components.len(), 12);
    }

}
