//! Render a multi-BLOCK circuit as MULTI-SHEET: one clean .png per block.
//!
//! The single-sheet sprawl ceiling caps complete boards ~5-7; drawing each block on its own
//! sheet (the professional practice) lets each sheet score like the clean fixtures (9-10).
//! Compiles the design with the REAL parser (handles `between:`, `power:`, multi-line, units),
//! then for each block emits a single-block sub-design (shared nets auto-become labeled ports)
//! through the normal anneal path and renders it. Critic each PNG with tools/schematic_critic.py.
//!
//! Usage: cargo run --release -p agent --example render_multisheet -- <draft.yaml> <out_dir>

use circuit_lang::SymbolProvider;
use indexmap::IndexMap;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

/// Deterministic UUIDv5-style id from a seed (engine forbids random; keeps re-emits stable).
fn det_uuid(seed: &str) -> String {
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

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// COMMIT per-block sub-sheets as a hierarchical KiCAD project (root + sub-sheet files,
/// global-label connectivity). See docs/specs/multisheet-commit.md.
fn write_multisheet_project(env: &KicadEnv, out_dir: &str, sheets: &[(String, String)]) -> anyhow::Result<()> {
    use std::fmt::Write as _;
    let main_root = det_uuid(&format!("root:{out_dir}"));
    let mut root = String::new();
    root.push_str("(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t(generator_version \"9.0\")\n");
    let _ = writeln!(root, "\t(uuid \"{main_root}\")");
    root.push_str("\t(paper \"A4\")\n\t(lib_symbols\n\t)\n");
    let mut inst = String::from("\t(sheet_instances\n\t\t(path \"/\"\n\t\t\t(page \"1\")\n\t\t)\n");
    for (i, (name, sch)) in sheets.iter().enumerate() {
        let page = i + 2;
        let sheet_uuid = det_uuid(&format!("{out_dir}:{name}"));
        let fname = sanitize(name);
        let sub_root =
            sch.split("(uuid \"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("").to_string();
        let mut sub = sch.replace(&format!("/{sub_root}\""), &format!("/{main_root}/{sheet_uuid}\""));
        sub = sub.replacen("(path \"/\"", &format!("(path \"/{sheet_uuid}\""), 1);
        sub = sub.replacen("(page \"1\")", &format!("(page \"{page}\")"), 1);
        std::fs::write(format!("{out_dir}/{fname}.kicad_sch"), &sub)?;
        let (x, y) = (25.4 + (i % 4) as f64 * 55.0, 25.4 + (i / 4) as f64 * 35.0);
        let (ny, fy) = (y - 0.7, y + 18.6);
        let _ = write!(
            root,
            "\t(sheet\n\t\t(at {x} {y})\n\t\t(size 35 18)\n\t\t(fields_autoplaced yes)\n\t\t(stroke (width 0.1524) (type solid))\n\t\t(fill (color 0 0 0 0.0000))\n\t\t(uuid \"{sheet_uuid}\")\n\t\t(property \"Sheetname\" \"{name}\"\n\t\t\t(at {x} {ny} 0)\n\t\t\t(effects (font (size 1.27 1.27) (bold yes)) (justify left bottom))\n\t\t)\n\t\t(property \"Sheetfile\" \"{fname}.kicad_sch\"\n\t\t\t(at {x} {fy} 0)\n\t\t\t(effects (font (size 1.27 1.27)) (justify left top) (hide yes))\n\t\t)\n\t\t(instances\n\t\t\t(project \"root\"\n\t\t\t\t(path \"/{main_root}\"\n\t\t\t\t\t(page \"{page}\")\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n"
        );
        let _ = write!(inst, "\t\t(path \"/{sheet_uuid}\"\n\t\t\t(page \"{page}\")\n\t\t)\n");
    }
    inst.push_str("\t)\n");
    root.push_str(&inst);
    root.push_str(")\n");
    let root_path = format!("{out_dir}/root.kicad_sch");
    std::fs::write(&root_path, &root)?;
    println!("wrote multi-sheet project -> {root_path} ({} sheets)", sheets.len());
    match KicadCli::new(env).erc(std::path::Path::new(&root_path)) {
        Ok(r) => println!("PROJECT ERC: {} errors, {} warnings", r.error_count(), r.warning_count()),
        Err(e) => println!("PROJECT ERC failed: {e}"),
    }
    Ok(())
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

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    let out_dir = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    std::fs::create_dir_all(&out_dir)?;

    // These ARE multi-sheet sub-sheets, so opt them into the route-aware crossing refinement
    // on the small path (a peripheral/bus sub-sheet tangles its port fanout; the refinement
    // takes e.g. an I2C sheet 16→13 xings and a power sheet 8→9). Single-sheet emit paths
    // (bench_corpus, agent_design) don't set this, so references stay byte-identical.
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(&yaml)?;
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    if design.blocks.len() < 2 {
        eprintln!("only {} block(s) — multi-sheet needs a multi-block design", design.blocks.len());
    }

    // SPLIT large blocks: the agent over-crams (a 17-part MCU sheet, a 21-part relay sheet), and
    // per-sheet DENSITY is the dominant critic gate (dense boards floor at 5-7). Bisect any block with
    // > SPLIT_MAX parts along its CONNECTED COMPONENTS — where the graph excludes high-degree rail/bus
    // nets (deg > RAIL_DEG ⇒ VCC/GND/VM/etc.), so two parts are "coupled" only by a point-to-point
    // signal. Independent sub-circuits (relay channels, a loosely-coupled indicator block) thus fall
    // into separate components and split at a near-ZERO signal cut (the only cut is shared rails, which
    // are ports/power symbols anyway) — never trading density for port-crowding. A single tightly-
    // coupled component (everything hangs off one MCU) is left intact. Pairs with the merge below so
    // sheets converge to a uniform ~TARGET parts. render_multisheet-only ⇒ engine/snapshots untouched.
    const SPLIT_MAX: usize = 16;
    const TARGET: usize = 11;
    const RAIL_DEG: usize = 4;
    let mut eff: IndexMap<String, circuit_lang::model::Block> = IndexMap::new();
    for (bname, block) in &design.blocks {
        if block.components.len() <= SPLIT_MAX {
            eff.insert(bname.clone(), block.clone());
            continue;
        }
        let refs: Vec<String> = block.components.keys().cloned().collect();
        // net -> indices of parts on it (any net, rail or signal).
        let mut net_refs: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
        for (i, rd) in refs.iter().enumerate() {
            let c = &block.components[rd];
            let mut nets = std::collections::HashSet::new();
            for t in c.pins.values() {
                if let circuit_lang::model::PinTarget::Net(n) = t {
                    nets.insert(n.clone());
                }
            }
            for u in c.units.values() {
                for t in u.values() {
                    if let circuit_lang::model::PinTarget::Net(n) = t {
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
        let mut comps: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
        for i in 0..refs.len() {
            let r = uf_find(&mut parent, i);
            comps.entry(r).or_default().push(i);
        }
        let comp_list: Vec<Vec<usize>> = comps.into_values().collect();
        if comp_list.len() < 2 {
            eff.insert(bname.clone(), block.clone()); // one tightly-coupled component — don't split
            continue;
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
            eff.insert(format!("{bname}_{g}"), sub);
        }
    }

    // Merge tiny blocks (< MERGE_MIN parts) into the bigger block they share the most (non-GND)
    // nets with, so the agent's occasional over-split — e.g. a 3-part "power_out" that's just the
    // output cap + terminal — doesn't render as a near-empty sheet the critic dings (buck
    // power_out=6). Connectivity-based + deterministic; only the smallest blocks move.
    const MERGE_MIN: usize = 4;
    let block_nets = |b: &circuit_lang::model::Block| -> std::collections::HashSet<String> {
        let mut s = std::collections::HashSet::new();
        let mut add = |t: &circuit_lang::model::PinTarget| {
            if let circuit_lang::model::PinTarget::Net(n) = t {
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
    };
    let bnames: Vec<String> = eff.keys().cloned().collect();
    let bnets: std::collections::HashMap<String, std::collections::HashSet<String>> =
        bnames.iter().map(|n| (n.clone(), block_nets(&eff[n]))).collect();
    let bsize = |n: &str| eff[n].components.len();
    // A net shared by ≥2 blocks is a cross-block PORT. A small block that's nonetheless PORT-RICH (≥5
    // ports) is a meaningful breakout — a connector pinout sheet (SWD/GPIO header) that reads cleanly
    // on its own — NOT a sparse fragment. Don't fold it into a neighbour (that re-crams two breakout
    // headers onto one sheet = the io-sheet collision); keep it as its own pinout sheet.
    let mut net_blocks: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for n in &bnames {
        for net in &bnets[n] {
            *net_blocks.entry(net.clone()).or_default() += 1;
        }
    }
    let port_rich = |n: &str| {
        bnets[n].iter().filter(|net| net_blocks.get(*net).copied().unwrap_or(0) >= 2).count() >= 5
    };
    let mut merge_into: std::collections::HashMap<String, String> = std::collections::HashMap::new();
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
    let mut groups: IndexMap<String, Vec<String>> = IndexMap::new();
    for n in &bnames {
        if !merge_into.contains_key(n) {
            groups.entry(n.clone()).or_default().push(n.clone());
        }
    }
    for (n, t) in &merge_into {
        groups.entry(t.clone()).or_default().push(n.clone());
    }

    let mut sheets: Vec<(String, String)> = Vec::new();
    for (name, gblocks) in &groups {
        // A sub-design holding this group's block(s): cross-group nets touch only these pins, so the
        // engine auto-labels the single-pin ones as ports and keeps multi-pin ones internal.
        let mut sub = design.clone();
        sub.blocks = IndexMap::new();
        for bn in gblocks {
            sub.blocks.insert(bn.clone(), eff[bn].clone());
        }
        let nparts: usize = gblocks.iter().map(|bn| eff[bn].components.len()).sum();

        // Shelf-pack seed + Anneal search (the premium tier). The crossmin-seed A/B was dropped:
        // measured marginal/mixed quality (occasional +1 critic, sometimes -1) for a 100x-3000x slower
        // seed (up to ~114 ms/sheet, scaling worst on dense sheets) AND a doubled emit — a bad trade.
        let ir = sch_layout::floorplan::infer_ir(&env, &sub);
        let emit = match sch_layout::floorplan::emit_anneal(&env, &sub, &ir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{name}: emit failed: {e}");
                continue;
            }
        };
        let tmp = tempfile::tempdir()?;
        let sch = tmp.path().join("s.kicad_sch");
        std::fs::write(&sch, emit.sch.as_bytes())?;
        let svg_dir = tempfile::tempdir()?;
        let svg_path = KicadCli::new(&env).export_svg_opts(&sch, svg_dir.path(), true)?;
        let svg = std::fs::read_to_string(&svg_path)?;
        let png = agent::render::svg_to_png(&svg, 1600)?;
        let out_png = format!("{out_dir}/{name}.png");
        std::fs::write(&out_png, png)?;
        println!(
            "{name}: {nparts} parts, {} warnings, {} wire-xings -> {out_png}",
            emit.layout_warnings.len(),
            emit.wire_crossings
        );
        for w in &emit.layout_warnings {
            println!("    WARN[{name}]: {w}");
        }
        sheets.push((sanitize(name), emit.sch.clone()));
    }

    // COMMIT a hierarchical KiCAD project (root + per-block sub-sheets) so the multi-sheet
    // design is openable + ERC-checkable, not just separate rendered PNGs.
    write_multisheet_project(&env, &out_dir, &sheets)?;
    Ok(())
}
