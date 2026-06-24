//! Read a `.kicad_sch` back into canonical kernel YAML — the inverse of `write` (spec §4).
//!
//! This closes the React data loop — emit writes a schematic; lift reads one
//! back into the same kernel language the agent authored. Two sources are
//! combined:
//!
//! - **Connectivity is taken from `kicad-cli`'s netlist** (`--format kicadxml`),
//!   the authoritative oracle for which pins share a net. Every `<net>` becomes
//!   a kernel net; every `<node ref=.. pin=..>` adds a `pin → Net(name)` mapping
//!   to that component. KiCAD reports pins by **number**, so the reconstructed
//!   kernel keys pins by number (see the round-trip note below).
//!
//! - **Semantics come from the hidden `ap_*` properties** that emit writes onto
//!   every symbol (`ap_block`, `ap_role`, `ap_parent`, `ap_index`). These survive
//!   into the netlist as `<property>`/`<field>` entries, so we read them straight
//!   off each `<comp>` — no second kiutils pass is needed. `ap_block` groups
//!   components into blocks; `ap_role = "decouple"` + `ap_parent`/`ap_index`
//!   reconstructs a synthesized decouple cap as [`Origin::Synthesized`], which
//!   [`circuit_lang::canon`] re-sugars back onto its parent's `decouple:` line.
//!
//! ## What is intentionally dropped (keeping the lifted YAML sparse)
//!
//! - **Auto-NC pins.** The kernel auto-NCs every unmentioned symbol pin; emit
//!   turns those into `(no_connect)` markers, which KiCAD surfaces as singleton
//!   `unconnected-(..)` nets in the netlist. We skip every `unconnected-*` net:
//!   `compile` re-derives those no-connects from the symbol library, so storing
//!   them would only bloat the YAML. Author-declared `nc` pins are likewise
//!   re-derived (they are unmentioned at the kernel→symbol boundary too).
//!
//! - **Positions, angles, UUIDs.** Placement is not part of the kernel model and
//!   is deliberately absent from YAML (reconcile preserves it across re-emits).
//!
//! ## Round-trip fidelity
//!
//! `emit → lift` preserves the kernel model up to two representable differences
//! the netlist cannot carry, both invisible to connectivity:
//!
//! 1. **Pin-key representation.** A source may key a pin by *name* (`VDD: 3V3`)
//!    or *number* (`"48": VCAP1`); the desugar preserves whichever the author
//!    wrote. The netlist node carries only the pin **number**, so lift always
//!    keys by number. The *connectivity* (which pin on which part joins which
//!    net) is identical — only the spelling of the key differs.
//! 2. **Net names.** KiCAD prefixes sheet-local net names with `/` and
//!    auto-names otherwise-unnamed nets; we strip the leading `/`, but a net the
//!    kernel left unnamed (e.g. a `between` pin-ref synthesis) may come back
//!    under KiCAD's chosen name. The net *partition* is identical.
//!
//! Both are erased by the netlist, not by lift; the lifted Design is the unique
//! sparse kernel model consistent with the schematic's connectivity.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use circuit_lang::canon::to_canonical_yaml;
use circuit_lang::model::{Block, Component, Design, Origin, PinTarget};
use kicad_cli_rs::cli::KicadCli;
use kicad_cli_rs::env::KicadEnv;

use sch_model::result::{AP_BLOCK, AP_INDEX, AP_PARENT, AP_ROLE, ROLE_AUTHORED};

/// Block a component lands in when it carries no `ap_block` tag (e.g. a symbol
/// authored by hand or by an older emitter). Mirrors the kernel default.
const DEFAULT_BLOCK: &str = "main";

/// Lift a `.kicad_sch` at `sch_path` back into canonical kernel YAML.
///
/// Runs `kicad-cli` for connectivity, reads `ap_*` semantics off the netlist
/// components, reconstructs a [`circuit_lang::Design`], and emits it via
/// [`circuit_lang::canon`]. See the module docs for the source-of-truth split
/// and the (connectivity-preserving) round-trip differences.
///
/// Returns the underlying `kicad-cli` failure if the schematic cannot be loaded
/// or its netlist parsed.
pub fn lift(env: &KicadEnv, sch_path: &Path) -> io::Result<String> {
    let netlist = KicadCli::new(env).netlist(sch_path)?;
    let design = design_from_netlist(&netlist);
    // Canon sorts pins/components/nets and re-sugars synthesized decouple caps,
    // so the output is the deterministic canonical kernel YAML.
    Ok(to_canonical_yaml(&design))
}

/// Reconstruct the sparse kernel [`Design`] from a parsed netlist.
///
/// Pure (no I/O): the connectivity oracle goes in, the kernel model comes out.
/// Factored from [`lift`] so the block-grouping, identity, auto-NC-skipping, and
/// net-name-stripping rules can be unit-tested without a KiCAD install.
fn design_from_netlist(netlist: &kicad_cli_rs::cli::Netlist) -> Design {
    let mut design = Design::default();

    // Phase 1: place every (real) component into its block, carrying identity.
    for comp in &netlist.components {
        // PWR_FLAG and other `#`-reference symbols are derived fresh on each
        // emit and never appear in the netlist; guard anyway so a stray one
        // can't leak into the kernel model.
        if comp.reference.starts_with('#') || comp.reference.is_empty() {
            continue;
        }

        let block_name = comp
            .properties
            .get(AP_BLOCK)
            .filter(|b| !b.is_empty())
            .map(String::as_str)
            .unwrap_or(DEFAULT_BLOCK)
            .to_string();

        let origin = origin_of(comp);

        let kernel = Component {
            part: comp.lib_id.clone(),
            value: kernel_value(&comp.value),
            footprint: kernel_footprint(&comp.properties),
            origin,
            ..Component::default()
        };

        design
            .blocks
            .entry(block_name)
            .or_insert_with(Block::default)
            .components
            .insert(comp.reference.clone(), kernel);
    }

    // An index from refdes -> (block, refdes) so net nodes can find their
    // component regardless of which block it landed in.
    let mut locate: BTreeMap<String, String> = BTreeMap::new();
    for (bname, block) in &design.blocks {
        for refdes in block.components.keys() {
            locate.insert(refdes.clone(), bname.clone());
        }
    }

    // Phase 2: connectivity. Each kept net contributes `pin -> Net(name)` to
    // every component it touches.
    // Disambiguate any two distinct nets that fold to the SAME kernel name (only
    // possible among auto-generated names whose sanitisation collides) so the net
    // partition is preserved exactly — a merge would silently short two nets.
    let mut used_names: std::collections::BTreeSet<String> = Default::default();
    for net in &netlist.nets {
        // Skip KiCAD's synthetic unconnected nets — they are this schematic's
        // no-connect markers, which `compile` re-derives. Keeping the YAML
        // sparse means never re-listing an auto-NC pin.
        if is_unconnected_net(&net.name) {
            continue;
        }
        let base = kernel_net_name(&net.name);
        let mut net_name = base.clone();
        let mut k = 2;
        while !used_names.insert(net_name.clone()) {
            net_name = format!("{base}_{k}");
            k += 1;
        }
        for (refdes, pin) in &net.nodes {
            let Some(bname) = locate.get(refdes) else {
                continue; // node on a component we skipped (e.g. a flag)
            };
            if let Some(comp) = design
                .blocks
                .get_mut(bname)
                .and_then(|b| b.components.get_mut(refdes))
            {
                comp.pins
                    .insert(pin.clone(), PinTarget::Net(net_name.clone()));
            }
        }
    }

    // Net attributes (`power:` the author's power-net list, `class:` from the
    // `nets:` block) and the design `name:` are author declarations the kernel
    // does NOT encode into the schematic — emit writes neither a power list nor
    // net classes — so they cannot be recovered here and are intentionally
    // absent from the lifted YAML. Connectivity and component identity, which
    // ARE in the schematic, round-trip fully.

    design
}

/// Reconstruct a component's [`Origin`] from its `ap_*` tags.
///
/// A `decouple`-role synth with a parent and a parseable index becomes
/// [`Origin::Synthesized`] (re-sugared by canon); everything else — including
/// the explicit `authored` sentinel and any tag-less older symbol — is
/// [`Origin::Authored`].
fn origin_of(comp: &kicad_cli_rs::cli::NetComp) -> Origin {
    let role = comp.properties.get(AP_ROLE).map(String::as_str);
    let parent = comp.properties.get(AP_PARENT).map(String::as_str);
    let index = comp
        .properties
        .get(AP_INDEX)
        .and_then(|s| s.parse::<u32>().ok());
    match (role, parent, index) {
        (Some(role), Some(parent), Some(index))
            if role != ROLE_AUTHORED && !role.is_empty() && !parent.is_empty() =>
        {
            Origin::Synthesized {
                parent: parent.to_string(),
                role: role.to_string(),
                index,
            }
        }
        _ => Origin::Authored,
    }
}

/// `true` for KiCAD's auto-named unconnected-pin nets (`unconnected-(..)`).
fn is_unconnected_net(name: &str) -> bool {
    name.starts_with("unconnected-")
}

/// Strip KiCAD's sheet-path prefix from a net name (`/3V3` -> `3V3`) and fold
/// any auto-generated name the kernel grammar would REJECT into a valid one.
///
/// Top-sheet nets are reported as `/<label>`; the kernel uses the bare label. A
/// clean author label (`+5V`, `GND`, `RXD`) round-trips verbatim. But KiCAD
/// auto-names an UNLABELED net after a pin function — `Net-(U1-XTAL1/PB6)`,
/// `Net-(U1-~{RESET}/PC6)` — and those carry `/`, `~{}`, parens, which the kernel's
/// net-name grammar rejects (`/` is reserved for hierarchy, spaces forbidden), so
/// the lifted YAML would fail to recompile. Fold such names to a valid UPPER_SNAKE
/// identifier: the exact spelling is irrelevant (the kernel never named this net
/// either), only the PARTITION — caller dedups to keep distinct nets distinct.
fn kernel_net_name(name: &str) -> String {
    let s = name.strip_prefix('/').unwrap_or(name);
    let rejected =
        |c: char| matches!(c, '/' | ' ' | '(' | ')' | '~' | '{' | '}') || c == '\t';
    if s.starts_with("Net-(") || s.chars().any(rejected) {
        let mut out = String::new();
        let mut gap = false;
        for c in s.chars() {
            if c.is_ascii_alphanumeric() {
                if gap && !out.is_empty() {
                    out.push('_');
                }
                out.push(c.to_ascii_uppercase());
                gap = false;
            } else {
                gap = true;
            }
        }
        let out = out.trim_matches('_').to_string();
        // Net names must start with a letter (UPPER_SNAKE).
        if out.chars().next().map_or(true, |c| !c.is_ascii_alphabetic()) {
            format!("N_{out}")
        } else {
            out
        }
    } else {
        s.to_string()
    }
}

/// A component's kernel value, or `None` when it has none.
///
/// KiCAD stores a symbol's `Value` field for every placed part; when emit had
/// no kernel value to write it leaves KiCAD's default-value placeholder `~`,
/// which the netlist echoes back. Both an empty string and a bare `~` mean
/// "no value" to the kernel, so both map to `None`.
fn kernel_value(s: &str) -> Option<String> {
    if s.is_empty() || s == "~" {
        None
    } else {
        Some(s.to_string())
    }
}

/// A component's footprint lib_id, or `None` when it has none.
///
/// The netlist parser folds `<footprint>` / a `Footprint` `<field>` / `<property>`
/// into `NetComp.properties["Footprint"]` (`cli.rs`). Emit writes KiCAD's empty
/// placeholder (`""`, or `~` for some fields) when a part is unassigned, so both
/// map to `None` here — exactly as `kernel_value` treats the `Value` field.
fn kernel_footprint(props: &std::collections::HashMap<String, String>) -> Option<String> {
    match props.get("Footprint") {
        Some(f) if !f.is_empty() && f != "~" => Some(f.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_cli_rs::cli::{NetComp, Netlist};

    fn comp(reference: &str, value: &str, lib_id: &str, props: &[(&str, &str)]) -> NetComp {
        NetComp {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn lifts_footprint_from_property() {
        let netlist = Netlist {
            components: vec![comp(
                "C1",
                "100nF",
                "Device:C",
                &[("Footprint", "Capacitor_SMD:C_0603_1608Metric")],
            )],
            nets: vec![],
        };
        let d = design_from_netlist(&netlist);
        let c = d
            .blocks
            .values()
            .flat_map(|b| b.components.iter())
            .find(|(r, _)| r.as_str() == "C1")
            .map(|(_, c)| c)
            .expect("C1 present");
        assert_eq!(
            c.footprint.as_deref(),
            Some("Capacitor_SMD:C_0603_1608Metric")
        );
    }

    #[test]
    fn empty_or_tilde_footprint_lifts_to_none() {
        for fp in ["", "~"] {
            let netlist = Netlist {
                components: vec![comp("R1", "1k", "Device:R", &[("Footprint", fp)])],
                nets: vec![],
            };
            let d = design_from_netlist(&netlist);
            let c = d
                .blocks
                .values()
                .flat_map(|b| b.components.iter())
                .find(|(r, _)| r.as_str() == "R1")
                .map(|(_, c)| c)
                .expect("R1 present");
            assert_eq!(c.footprint, None, "footprint {fp:?} must lift to None");
        }
    }

    #[test]
    fn net_name_strips_only_leading_sheet_slash() {
        assert_eq!(kernel_net_name("/3V3"), "3V3");
        assert_eq!(kernel_net_name("USB_CC1"), "USB_CC1");
        // A `Net-(..)` auto-name is folded to a valid UPPER_SNAKE identifier so the
        // lifted YAML recompiles (see kernel_net_name_preserves_valid_and_folds...).
        assert_eq!(kernel_net_name("Net-(C1-Pad2)"), "NET_C1_PAD2");
        // The leading sheet slash is stripped; an interior slash is reserved by the
        // grammar, so a sub-sheet path is folded rather than left to break recompile.
        assert_eq!(kernel_net_name("/sub/NET"), "SUB_NET");
    }

    #[test]
    fn empty_and_tilde_values_are_none() {
        assert_eq!(kernel_value(""), None);
        assert_eq!(kernel_value("~"), None);
        assert_eq!(kernel_value("10uF"), Some("10uF".to_string()));
    }

    #[test]
    fn unconnected_nets_are_detected() {
        assert!(is_unconnected_net("unconnected-(J1-SBU1-PadA8)"));
        assert!(!is_unconnected_net("/GND"));
        assert!(!is_unconnected_net("Net-(C1-Pad2)"));
    }

    #[test]
    fn kernel_net_name_preserves_valid_and_folds_auto_names() {
        // Clean author labels round-trip verbatim (the sheet-path `/` is stripped).
        assert_eq!(kernel_net_name("/GND"), "GND");
        assert_eq!(kernel_net_name("+5V"), "+5V");
        assert_eq!(kernel_net_name("RXD"), "RXD");
        assert_eq!(kernel_net_name("I2C_SDA"), "I2C_SDA");
        // KiCAD auto-names carry `/`, `~{}`, parens — the kernel grammar rejects
        // those (the lifted YAML would fail to recompile). Fold to UPPER_SNAKE.
        assert_eq!(kernel_net_name("Net-(U1-XTAL1/PB6)"), "NET_U1_XTAL1_PB6");
        assert_eq!(kernel_net_name("Net-(U1-~{RESET}/PC6)"), "NET_U1_RESET_PC6");
        assert_eq!(kernel_net_name("Net-(D1-A)"), "NET_D1_A");
        // The folded names carry none of the grammar's hard-error characters.
        for raw in ["Net-(U1-XTAL1/PB6)", "Net-(U1-~{RESET}/PC6)", "Net-(D1-A)"] {
            let k = kernel_net_name(raw);
            assert!(!k.contains('/') && !k.contains(' ') && !k.contains('('));
            assert!(k.chars().next().unwrap().is_ascii_alphabetic());
        }
    }

    #[test]
    fn origin_from_ap_tags() {
        let authored = comp("R1", "10k", "Device:R", &[("ap_role", "authored")]);
        assert_eq!(origin_of(&authored), Origin::Authored);

        // Missing tags => authored (older/foreign symbol).
        let bare = comp("R2", "1k", "Device:R", &[]);
        assert_eq!(origin_of(&bare), Origin::Authored);

        let synth = comp(
            "__dec_U2_3",
            "100nF",
            "Device:C",
            &[
                ("ap_role", "decouple"),
                ("ap_parent", "U2"),
                ("ap_index", "3"),
            ],
        );
        assert_eq!(
            origin_of(&synth),
            Origin::Synthesized {
                parent: "U2".into(),
                role: "decouple".into(),
                index: 3,
            }
        );

        // A decouple role missing its index falls back to authored (can't form a
        // valid synthesized identity).
        let broken = comp(
            "C9",
            "100nF",
            "Device:C",
            &[("ap_role", "decouple"), ("ap_parent", "U2")],
        );
        assert_eq!(origin_of(&broken), Origin::Authored);
    }

    #[test]
    fn reconstructs_blocks_components_and_connectivity() {
        // Two authored resistors in block "power", their pin-2s tied to GND
        // (one shared net) and pin-1s on distinct labelled nets; plus a
        // KiCAD `unconnected-*` net that must be dropped, and a tag-less
        // component that defaults to the "main" block.
        let netlist = Netlist {
            components: vec![
                comp(
                    "R1",
                    "5.1k",
                    "Device:R",
                    &[("ap_block", "power"), ("ap_role", "authored")],
                ),
                comp(
                    "R2",
                    "5.1k",
                    "Device:R",
                    &[("ap_block", "power"), ("ap_role", "authored")],
                ),
                comp("X9", "", "Device:R", &[]), // no ap_block -> "main"
            ],
            nets: vec![
                kicad_cli_rs::cli::Net {
                    name: "/A".into(),
                    nodes: vec![("R1".into(), "1".into())],
                },
                kicad_cli_rs::cli::Net {
                    name: "/B".into(),
                    nodes: vec![("R2".into(), "1".into())],
                },
                kicad_cli_rs::cli::Net {
                    name: "/GND".into(),
                    nodes: vec![("R1".into(), "2".into()), ("R2".into(), "2".into())],
                },
                kicad_cli_rs::cli::Net {
                    name: "unconnected-(X9-Pad1)".into(),
                    nodes: vec![("X9".into(), "1".into())],
                },
            ],
        };

        let d = design_from_netlist(&netlist);

        // Blocks: "power" (R1,R2) and "main" (X9).
        let power = d.blocks.get("power").expect("power block");
        assert_eq!(power.components.len(), 2);
        assert!(d.blocks.get("main").unwrap().components.contains_key("X9"));

        let r1 = &power.components["R1"];
        assert_eq!(r1.part, "Device:R");
        assert_eq!(r1.value.as_deref(), Some("5.1k"));
        assert_eq!(r1.origin, Origin::Authored);
        assert_eq!(r1.pins["1"], PinTarget::Net("A".into()));
        assert_eq!(r1.pins["2"], PinTarget::Net("GND".into()));
        assert_eq!(
            power.components["R2"].pins["2"],
            PinTarget::Net("GND".into())
        );

        // The unconnected net is dropped: X9 has no pins (compile re-derives NC).
        let x9 = &d.blocks["main"].components["X9"];
        assert!(x9.pins.is_empty(), "auto-NC pins must not be lifted");
        assert_eq!(x9.value, None, "empty value lifts to None");
    }

    #[test]
    fn pwr_flag_and_hash_refs_are_skipped() {
        let netlist = Netlist {
            components: vec![
                comp("#FLG01", "PWR_FLAG", "power:PWR_FLAG", &[]),
                comp("R1", "1k", "Device:R", &[("ap_block", "main")]),
            ],
            nets: vec![kicad_cli_rs::cli::Net {
                name: "/GND".into(),
                nodes: vec![("#FLG01".into(), "1".into()), ("R1".into(), "2".into())],
            }],
        };
        let d = design_from_netlist(&netlist);
        // Only R1 survives; the flag is not a kernel component, and its node on
        // GND is ignored (no panic locating a skipped ref).
        let total: usize = d.blocks.values().map(|b| b.components.len()).sum();
        assert_eq!(total, 1);
        assert_eq!(
            d.blocks["main"].components["R1"].pins["2"],
            PinTarget::Net("GND".into())
        );
    }
}
