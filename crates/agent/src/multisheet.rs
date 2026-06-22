//! Multi-sheet (hierarchical) commit — the validated answer to the single-sheet density
//! ceiling: one sheet per functional block scores 8-9 each, vs a cramped single sheet at
//! 4-7. Emits a committable KiCAD project (root `.kicad_sch` + per-block sub-sheet files,
//! global-label cross-sheet connectivity). See `docs/specs/multisheet-commit.md`.
//!
//! Per-block independent layout is the RULE here, not a dense-only special case.
//! [`refine_blocks`] first normalizes the agent's blocks into uniform-sized SHEET GROUPS
//! (split over-crammed blocks, merge tiny fragments), then [`emit_multisheet`] runs a
//! SEPARATE anneal per group → one sub-sheet per group, disjoint by construction. A group
//! holding ≥2 blocks is emitted together as a sub-design; the single-sheet floorplan is
//! block-aware (Tier-B disjoint-region placement) so those blocks stay spatially disjoint.

use circuit_lang::model::{Block, Design, PinTarget};
use indexmap::IndexMap;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// Union-find root with path-halving.
fn uf_find(parent: &mut [usize], x: usize) -> usize {
    let mut r = x;
    while parent[r] != r {
        r = parent[r];
    }
    let mut c = x;
    while parent[c] != r {
        let n = parent[c];
        parent[c] = r;
        c = n;
    }
    r
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

/// One sheet group from [`refine_blocks`]: a name (the sub-sheet / file stem) and the member
/// blocks (each a `(block_name, Block)`) to lay out TOGETHER on that sheet. A single-block
/// group → one sub-sheet; a multi-block group is emitted as a sub-design (Tier-B keeps the
/// members disjoint). Carrying the `Block` bodies means the split partition is computed ONCE
/// here — callers never re-derive it.
pub type SheetGroup = (String, Vec<(String, Block)>);

/// SPLIT a single block along its CONNECTED COMPONENTS (rail-excluded graph) into effective
/// blocks, the shared primitive behind [`refine_blocks`]. Returns `(name, Block)` pairs:
/// ≤ SPLIT_MAX parts (or one tightly-coupled component) → the block unchanged (`[(name, b)]`);
/// else → `ceil(parts/TARGET)` bin-packed fragments named `{bname}_{g}`.
///
/// "Coupled" = sharing a LOW-degree (deg ≤ `RAIL_DEG`) point-to-point signal net; high-degree
/// rail/bus nets (VCC/GND/VM/…) are excluded, so independent sub-circuits fall into separate
/// components and split at a near-zero signal cut, while a half-bridge sharing a high-degree
/// rail stays intact.
fn split_block(bname: &str, block: &Block) -> Vec<(String, Block)> {
    const SPLIT_MAX: usize = 16;
    const TARGET: usize = 11;
    const RAIL_DEG: usize = 4;
    if block.components.len() <= SPLIT_MAX {
        return vec![(bname.to_string(), block.clone())];
    }
    let refs: Vec<String> = block.components.keys().cloned().collect();
    // net -> indices of parts on it (any net, rail or signal).
    let mut net_refs: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, rd) in refs.iter().enumerate() {
        let c = &block.components[rd];
        let mut nets = HashSet::new();
        for t in c.pins.values() {
            if let PinTarget::Net(n) = t {
                nets.insert(n.clone());
            }
        }
        for u in c.units.values() {
            for t in u.values() {
                if let PinTarget::Net(n) = t {
                    nets.insert(n.clone());
                }
            }
        }
        for n in nets {
            net_refs.entry(n).or_default().push(i);
        }
    }
    // Union parts that share a LOW-degree (point-to-point signal) net; skip rails/buses.
    let mut parent: Vec<usize> = (0..refs.len()).collect();
    for ids in net_refs.values() {
        if ids.len() > RAIL_DEG {
            continue;
        }
        for w in ids.windows(2) {
            let (a, b) = (uf_find(&mut parent, w[0]), uf_find(&mut parent, w[1]));
            parent[a] = b;
        }
    }
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..refs.len() {
        let r = uf_find(&mut parent, i);
        comps.entry(r).or_default().push(i);
    }
    let comp_list: Vec<Vec<usize>> = comps.into_values().collect();
    if comp_list.len() < 2 {
        return vec![(bname.to_string(), block.clone())]; // one tightly-coupled component — don't split
    }
    // Bin-pack components into ceil(parts/TARGET) groups, largest component to the smallest group.
    let ngroups = block.components.len().div_ceil(TARGET).clamp(2, comp_list.len());
    let mut order: Vec<usize> = (0..comp_list.len()).collect();
    order.sort_by_key(|&ci| std::cmp::Reverse(comp_list[ci].len()));
    let mut groups_idx: Vec<Vec<usize>> = vec![Vec::new(); ngroups];
    let mut sizes = vec![0usize; ngroups];
    for &ci in &order {
        let g = (0..ngroups).min_by_key(|&g| sizes[g]).unwrap();
        groups_idx[g].extend(&comp_list[ci]);
        sizes[g] += comp_list[ci].len();
    }
    println!(
        "  [split] block '{bname}' ({} parts, {} signal-components) -> {ngroups} sheets",
        block.components.len(),
        comp_list.len()
    );
    let mut out = Vec::new();
    let mut g = 0;
    for ris in &groups_idx {
        if ris.is_empty() {
            continue;
        }
        g += 1;
        let mut sub = block.clone();
        sub.components = IndexMap::new();
        for &ri in ris {
            let rd = &refs[ri];
            sub.components.insert(rd.clone(), block.components[rd].clone());
        }
        out.push((format!("{bname}_{g}"), sub));
    }
    out
}

/// Normalize the agent's blocks into uniform-sized SHEET GROUPS — the single source of truth
/// for per-block independent layout. Each [`SheetGroup`] carries the actual member blocks, so
/// the split partition is computed exactly once. Two refinements, both connectivity-based and
/// deterministic (the agent over/under-partitions despite the ~6-10 parts/block prompt):
///
/// - **SPLIT** (see [`split_block`]): any block with > 16 parts bisects along its rail-excluded
///   connected components into ~11-part fragments. A single tightly-coupled component is left
///   intact, so half-bridge / one-MCU motifs never split.
/// - **MERGE** any block with < `MERGE_MIN` parts into the block it shares the most non-GND
///   nets with, UNLESS it is PORT-RICH (≥5 cross-block nets — a real breakout/header sheet
///   that reads cleanly on its own, e.g. an SWD/GPIO pinout). Only the smallest blocks move.
pub fn refine_blocks(blocks: &IndexMap<String, Block>) -> Vec<SheetGroup> {
    const MERGE_MIN: usize = 4;

    // ── SPLIT ── into the effective (post-split) blocks, keyed by name, in deterministic order.
    let mut eff: IndexMap<String, Block> = IndexMap::new();
    for (bname, block) in blocks {
        if block.components.is_empty() {
            continue;
        }
        for (mname, frag) in split_block(bname, block) {
            eff.insert(mname, frag);
        }
    }

    // ── MERGE ── fold each tiny, non-port-rich block into its strongest neighbour.
    let bnames: Vec<String> = eff.keys().cloned().collect();
    let bnets: HashMap<String, HashSet<String>> =
        bnames.iter().map(|n| (n.clone(), block_nets(&eff[n]))).collect();
    let bsize = |n: &str| eff[n].components.len();
    // A net shared by ≥2 blocks is a cross-block PORT.
    let mut net_blocks: HashMap<String, usize> = HashMap::new();
    for n in &bnames {
        for net in &bnets[n] {
            *net_blocks.entry(net.clone()).or_default() += 1;
        }
    }
    let port_rich = |n: &str| {
        bnets[n].iter().filter(|net| net_blocks.get(*net).copied().unwrap_or(0) >= 2).count() >= 5
    };
    let mut merge_into: HashMap<String, String> = HashMap::new();
    for n in &bnames {
        if bsize(n) >= MERGE_MIN || port_rich(n) {
            continue;
        }
        if let Some(t) = bnames
            .iter()
            .filter(|m| m.as_str() != n.as_str() && bsize(m) >= MERGE_MIN)
            .max_by_key(|m| (bnets[n].intersection(&bnets[*m]).count(), bsize(m)))
        {
            println!("  [merge] tiny block '{n}' ({} parts) -> '{t}'", bsize(n));
            merge_into.insert(n.clone(), t.clone());
        }
    }

    // ── ASSEMBLE ── sheet groups keyed by surviving block, members carrying their Block body.
    // Preserve the (split) block order; merged blocks fold into their target's group.
    let mut groups: IndexMap<String, Vec<String>> = IndexMap::new();
    for n in &bnames {
        if !merge_into.contains_key(n) {
            groups.entry(n.clone()).or_default().push(n.clone());
        }
    }
    for n in &bnames {
        if let Some(t) = merge_into.get(n) {
            groups.entry(t.clone()).or_default().push(n.clone());
        }
    }
    groups
        .into_iter()
        .map(|(name, members)| {
            let blocks = members.into_iter().map(|m| (m.clone(), eff[&m].clone())).collect();
            (name, blocks)
        })
        .collect()
}

/// Emit a multi-block `Design` as a hierarchical KiCAD project under `out_dir`. Refines the
/// blocks into uniform sheet GROUPS ([`refine_blocks`]), then runs a SEPARATE anneal per
/// group → one sub-sheet per group + a root that references them. Returns the root
/// `.kicad_sch` path. Cross-group nets auto-become global labels (single-pin ports) / power
/// symbols, so the sheets connect. Sets `MULTISHEET_REFINE` so each sub-sheet gets the
/// route-aware crossing refinement (the validated sub-sheet path).
pub fn emit_multisheet(env: &KicadEnv, design: &Design, out_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    // SAFETY: process-wide flag read by the engine to opt sub-sheets into route-aware
    // refinement; this whole operation is a multi-sheet emit, so it's the intended scope.
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };
    let mut sheets: Vec<(String, String)> = Vec::new();
    for (gname, members) in refine_blocks(&design.blocks) {
        // A sub-design holding this group's block(s): cross-group nets touch only these pins,
        // so the engine auto-labels single-pin ones as ports and keeps multi-pin ones internal.
        let mut sub = design.clone();
        sub.blocks = members.into_iter().collect();
        let ir = sch_layout::floorplan::infer_ir(env, &sub);
        let emit = sch_layout::floorplan::emit_anneal(env, &sub, &ir)
            .map_err(|e| anyhow::anyhow!("emit sheet '{gname}': {e}"))?;
        sheets.push((sanitize(&gname), emit.sch));
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
