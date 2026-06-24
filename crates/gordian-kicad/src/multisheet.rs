//! Composed single-sheet commit — the validated answer to the single-sheet density
//! ceiling: each functional block is laid out INDEPENDENTLY (8-9 each), then the blocks
//! are tiled onto ONE `.kicad_sch` as labeled bounding-box regions. No hierarchy, no
//! sub-sheet files, no root nav page: cross-block nets connect via GLOBAL LABELS only
//! (matching names auto-join on a single sheet, so no wire ever crosses a block border).
//!
//! Per-block independent layout is the RULE here, not a dense-only special case.
//! [`refine_blocks`] first normalizes the agent's blocks into uniform-sized SHEET GROUPS
//! (split over-crammed blocks, merge tiny fragments), then [`compose_single_sheet`] runs a
//! SEPARATE anneal per group (each in its own coordinate space), shelf-packs the group
//! regions onto one sheet with margins so they never touch, translates each group's
//! geometry to its tile, and frames each with a graphic rectangle + a name label. The page
//! is enlarged to fit; global labels mean a large sheet still has no long wires.

use circuit_lang::model::{Block, Design, PinTarget};
use indexmap::IndexMap;
use kicad_cli_rs::cli::KicadCli;
use kicad_cli_rs::env::KicadEnv;
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
/// - **SPLIT** (see `split_block`): any block with > 16 parts bisects along its rail-excluded
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

/// Emit a multi-block `Design` as ONE composed `.kicad_sch` under `out_dir` (file
/// `root.kicad_sch`). Refines the blocks into uniform sheet GROUPS ([`refine_blocks`]), runs
/// a SEPARATE anneal per group (each in its own coordinate space, as a TYPED writer), then
/// hands the group writers to the engine's `compose_writers`, which tiles the group regions
/// onto a single enlarged page (translating each writer's items in mm) and frames each with a
/// labeled bounding box. Cross-group nets auto-become global labels (single-pin ports) / power
/// symbols, and matching global-label names join across the sheet — no wire crosses a block
/// border. Sets `MULTISHEET_REFINE` so each group gets the route-aware crossing refinement.
/// Returns the composed `.kicad_sch` path.
pub fn compose_single_sheet(
    env: &KicadEnv,
    design: &Design,
    out_dir: &Path,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    // SAFETY: process-wide flag read by the engine to opt each group into route-aware
    // refinement; this whole operation is a composed multi-block emit, the intended scope.
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };
    let groups = refine_blocks(&design.blocks);
    let cross_sheet = cross_sheet_nets(&groups);

    let mut groups_w: Vec<(String, sch_layout::write::SchematicWriter)> = Vec::new();
    for (gname, members) in groups {
        // A sub-design holding this group's block(s). Mark every cross-sheet net this group
        // touches as a PORT so the engine emits one global label per group for the hop and
        // wires any ≥2 local pins together (instead of duplicate local labels).
        let mut sub = design.clone();
        sub.blocks = members.into_iter().collect();
        mark_cross_sheet_ports(&mut sub, &cross_sheet);
        let ir = sch_layout::floorplan::infer_ir(env, &sub);
        // Lay out each group INDEPENDENTLY and keep its TYPED writer (not a rendered
        // string): the engine composer translates each group's items to its tile in mm
        // and folds them into one sheet — no string-level geometry math here.
        let w = sch_layout::floorplan::emit_writer(env, &sub, &ir, Box::new(anneal_place::Anneal))
            .map_err(|e| anyhow::anyhow!("emit group '{gname}': {e}"))?;
        groups_w.push((sanitize(&gname), w));
    }
    let composed = sch_layout::floorplan::compose_writers(groups_w, design.name.as_deref());
    let path = out_dir.join("root.kicad_sch");
    std::fs::write(&path, &composed)?;
    let _ = env; // reserved (validation hook); kept for signature symmetry
    Ok(path)
}

/// Nets that CROSS sheets: a signal net (`block_nets` excludes GND/VSS rails) present in ≥2
/// sheet GROUPS. On the sheet where such a net has exactly one pin, the engine's degree-1 rule
/// already makes it a global-label port. But where it has ≥2 LOCAL pins (an op-amp follower's
/// OUT+IN-, any feedback loop), the degree-1 rule can't see it, so the engine either wires it
/// locally with NO cross-sheet label (a silent disconnect) or — when a local tee can't form —
/// drops a duplicate LOCAL label on each pin (the "confusing duplicate ISENSE_W label" defect,
/// BLDC current_sense). [`mark_cross_sheet_ports`] flags these as ports so each sheet emits one
/// global label for the hop and wires its local pins together. A purely single-sheet net (in one
/// group only) is excluded, so its wiring is untouched.
pub fn cross_sheet_nets(groups: &[SheetGroup]) -> HashSet<String> {
    let mut net_groups: HashMap<String, usize> = HashMap::new();
    for (_, members) in groups {
        let nets: HashSet<String> = members.iter().flat_map(|(_, b)| block_nets(b)).collect();
        for net in nets {
            *net_groups.entry(net).or_default() += 1;
        }
    }
    net_groups.into_iter().filter(|(_, c)| *c >= 2).map(|(n, _)| n).collect()
}

/// Mark every cross-sheet net (see [`cross_sheet_nets`]) that `sub` touches as a PORT, so
/// `infer_ir` treats it as one global-label hop per sheet instead of N local labels. Power nets
/// are inert (`infer_ir` never makes a power net a port), and connectivity is unchanged: a local
/// wire + one global label is electrically identical to a label on each local pin.
pub fn mark_cross_sheet_ports(sub: &mut Design, cross_sheet: &HashSet<String>) {
    let touched: HashSet<String> =
        sub.blocks.values().flat_map(block_nets).filter(|n| cross_sheet.contains(n)).collect();
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
