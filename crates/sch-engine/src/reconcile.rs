//! Reconciliation — preserve user positions on re-emit (spec §4/§7).
//!
//! The `.kicad_sch` is the source of truth for *positions*: a user who drags a
//! symbol in the KiCAD editor must not have that move clobbered the next time we
//! emit from the (possibly changed) kernel `Design`. So before emitting, we
//! parse the prior `.kicad_sch` into a map of **identity → prior placement**
//! (`at`, `angle`, `uuid`) and, for every component that survives by identity,
//! reuse its prior position/angle/uuid instead of the placer's auto-position.
//! Only genuinely new components are auto-placed; deleted ones (and their
//! labels / no-connect markers) simply aren't re-emitted.
//!
//! ## Identity — how a component is matched across re-emits
//!
//! - **Authored** components are matched by **refdes**. A user renaming a refdes
//!   is treated as delete-plus-add (it gets a fresh placement), which is the
//!   conservative, predictable behaviour.
//! - **Synthesized** components (sugar-expanded decouple caps etc.) have no
//!   stable author-assigned refdes — the kernel may renumber them — so they are
//!   matched by `(ap_parent, ap_role, ap_index)`, read from the hidden `ap_*`
//!   properties [`crate::emit`] writes on every symbol. This makes the file
//!   self-describing: the prior emit recorded the identity, so reconcile can
//!   recover it without re-running the kernel against the old YAML.
//!
//! The `ap_*` tags are written by [`crate::emit::SchematicWriter::add_symbol_full`]
//! for *every* emitted symbol, so a schematic emitted by a prior version of this
//! engine round-trips cleanly here.

use std::collections::HashMap;
use std::io;

use circuit_lang::model::{Origin, PinTarget};
use circuit_lang::{Design, PinType, SymbolProvider};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use kiutils_kicad::SchematicFile;

use crate::emit::{Dir, SchematicWriter};
use crate::grid::snap_point;
use crate::place;

/// Stub wire length from a pin to its power symbol, in mm (3 grid units = 3.81mm).
const STUB_MM: f64 = 3.81;
/// Vertical riser from a horizontal stub to a power symbol, in mm (2 grid units).
const RISER_MM: f64 = 2.54;

/// Ground-ish rails point down; everything else points up.
fn is_ground(net: &str) -> bool {
    let n = net.to_ascii_uppercase();
    n.contains("GND") || n.starts_with("VSS")
}

/// Initial orientation for a freshly placed component. RailSpan passives stand
/// vertical with the ground-side pin down; KiCAD's Device:R / Device:C bodies
/// are already vertical at angle 0 with pin "1" on top, so the only decision is
/// whether to flip: pin "1" tied to ground -> 180°.
fn initial_angle(comp: &circuit_lang::model::Component) -> f64 {
    use circuit_lang::model::LayoutRole;
    if comp.layout_role != Some(LayoutRole::RailSpan) {
        return 0.0;
    }
    match comp.pins.get("1") {
        Some(PinTarget::Net(n)) if is_ground(n) => 180.0,
        _ => 0.0,
    }
}

/// Choose the power-symbol lib_id for a rail. Exact `power:` match first, then
/// common aliases, then a donor whose Value is overridden to the rail name.
fn power_lib_id(net: &str, provider: &RealSymbolProvider) -> String {
    use circuit_lang::SymbolProvider as _;
    let exact = format!("power:{net}");
    if provider.symbol(&exact).is_some() {
        return exact;
    }
    let alias = match net {
        "3V3" => Some("power:+3V3"),
        "5V" => Some("power:+5V"),
        "12V" => Some("power:+12V"),
        _ => None,
    };
    if let Some(a) = alias {
        if provider.symbol(a).is_some() {
            return a.to_string();
        }
    }
    // Non-standard positive rails (1V8, VDDA, VBUS, …) fall back to the generic
    // VCC arrow; the Value override still names the net correctly. Extend the
    // alias table above if a distinct glyph is wanted.
    if is_ground(net) {
        "power:GND".into()
    } else {
        "power:VCC".into()
    }
}

/// Property key for the block a component belongs to.
pub const AP_BLOCK: &str = "ap_block";
/// Property key for a synthesized component's role (absent / `"authored"` for
/// authored parts).
pub const AP_ROLE: &str = "ap_role";
/// Property key for a synthesized component's parent refdes.
pub const AP_PARENT: &str = "ap_parent";
/// Property key for a synthesized component's index within `(parent, role)`.
pub const AP_INDEX: &str = "ap_index";

/// The `ap_role` value written for authored components.
pub const ROLE_AUTHORED: &str = "authored";

/// Stable identity used to match a component across re-emits (spec §7).
///
/// Authored parts are keyed by refdes; synthesized parts by their sugar
/// provenance `(parent, role, index)` so the kernel renumbering a decouple cap
/// does not lose its position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identity {
    /// An authored component, matched by refdes.
    Authored(String),
    /// A sugar-synthesized component, matched by provenance.
    Synthesized {
        parent: String,
        role: String,
        index: u32,
    },
}

impl Identity {
    /// The identity of a kernel component from its [`Origin`] and refdes.
    pub fn of(refdes: &str, origin: &Origin) -> Identity {
        match origin {
            Origin::Authored => Identity::Authored(refdes.to_string()),
            Origin::Synthesized {
                parent,
                role,
                index,
            } => Identity::Synthesized {
                parent: parent.clone(),
                role: role.clone(),
                index: *index,
            },
        }
    }
}

/// A component's placement recovered from a prior `.kicad_sch`.
#[derive(Debug, Clone)]
pub struct PriorPlacement {
    /// Sheet position (mm) as last saved — may be a user move.
    pub at: [f64; 2],
    /// Orientation in degrees.
    pub angle: f64,
    /// The symbol instance uuid, reused on re-emit so diffs stay minimal.
    pub uuid: Option<String>,
}

/// Parse a prior `.kicad_sch` document into `identity → prior placement`.
///
/// Reads every symbol's `(at …)`/angle, its uuid, and its hidden `ap_*` tags.
/// The identity is `Synthesized` when the symbol carries an `ap_parent`/`ap_role`
/// pair that is not the authored sentinel, otherwise `Authored(reference)`.
/// `PWR_FLAG` and other reference-less / `#`-prefixed symbols are skipped — they
/// are derived fresh on each emit and have no kernel identity to preserve.
///
/// Returns an empty map (not an error) if the text cannot be parsed as a
/// schematic, so a corrupt or foreign base degrades to "auto-place everything"
/// rather than failing the emit.
pub fn parse_prior(prior: &str) -> HashMap<Identity, PriorPlacement> {
    let mut out = HashMap::new();

    // kiutils reads from a path; stage the text in a temp file. If anything
    // about staging or parsing fails, fall back to "no prior placements".
    let Ok(tmp) = tempfile_with(prior) else {
        return out;
    };
    let Ok(doc) = SchematicFile::read(tmp.path()) else {
        return out;
    };

    for sym in &doc.ast().symbols {
        // A symbol with no `(at …)` can't contribute a position.
        let Some(at) = sym.at else { continue };
        let angle = sym.angle.unwrap_or(0.0);

        // Skip power/flag symbols (hidden `#`-prefixed refs): they're emitted
        // fresh each time and carry no kernel identity.
        let reference = sym.reference.clone();
        if reference
            .as_deref()
            .map(|r| r.starts_with('#'))
            .unwrap_or(true)
        {
            continue;
        }
        let reference = reference.unwrap();

        let prop = |key: &str| -> Option<&str> {
            sym.properties
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };

        // Synthesized iff it carries a non-authored ap_role + an ap_parent.
        let role = prop(AP_ROLE);
        let parent = prop(AP_PARENT);
        let index = prop(AP_INDEX).and_then(|s| s.parse::<u32>().ok());
        let identity = match (role, parent, index) {
            (Some(role), Some(parent), Some(index))
                if role != ROLE_AUTHORED && !role.is_empty() && !parent.is_empty() =>
            {
                Identity::Synthesized {
                    parent: parent.to_string(),
                    role: role.to_string(),
                    index,
                }
            }
            // No usable synthesized tags -> match by refdes.
            _ => Identity::Authored(reference.clone()),
        };

        out.insert(
            identity,
            PriorPlacement {
                at,
                angle,
                uuid: sym.uuid.clone(),
            },
        );
    }

    out
}

/// Stage `text` into a temporary `.kicad_sch` file for kiutils to read.
fn tempfile_with(text: &str) -> io::Result<tempfile::NamedTempFile> {
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile()?;
    std::fs::write(tmp.path(), text)?;
    Ok(tmp)
}

/// The reconciliation-aware core of emission (spec §4/§7).
///
/// Identical to the one-shot `emit_design` except that, given a prior
/// `.kicad_sch` text, each surviving component (matched by [`Identity`]) reuses
/// its prior `(at …)`, angle, and instance uuid; only components absent from the
/// prior are auto-placed by [`place::place`]. Every emitted symbol is tagged
/// with its `ap_*` identity properties so the result stays self-describing for
/// the next round. `prior = None` reduces exactly to the from-scratch emit.
///
/// Connectivity (labels), no-connect markers, and power flags are regenerated
/// from the current `Design` every time — they are cheap, deterministic, and
/// always correct for the current model — so a deleted component's labels and
/// markers naturally drop out.
pub fn emit_design_reconciled(
    env: &KicadEnv,
    design: &Design,
    prior: Option<&str>,
) -> io::Result<String> {
    // Per-component approximate sizes drive bbox-aware placement cells. Load
    // each part's geometry; parts whose geometry can't load fall back to the
    // fixed legacy cell inside the placer.
    let mut sizes = place::SizeMap::new();
    let mut size_cache: std::collections::HashMap<String, [f64; 2]> =
        std::collections::HashMap::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            let size = match size_cache.get(&comp.part) {
                Some(s) => Some(*s),
                None => {
                    let loaded = kicad_bridge::geometry::SymbolGeometry::load(env, &comp.part)
                        .ok()
                        .map(|g| g.approx_size());
                    if let Some(s) = loaded {
                        size_cache.insert(comp.part.clone(), s);
                    }
                    loaded
                }
            };
            if let Some(s) = size {
                sizes.insert(refdes.clone(), s);
            }
        }
    }
    let auto = place::place(design, &sizes);
    let prior_map = prior.map(parse_prior).unwrap_or_default();

    let mut w = SchematicWriter::new();
    let provider = RealSymbolProvider::new(env.clone());

    // Collect power nets (declared via `rails:` or explicit `power: true`).
    let power_nets: std::collections::BTreeSet<String> = design
        .nets
        .iter()
        .filter(|(_, a)| a.power)
        .map(|(n, _)| n.clone())
        .collect();

    // Net bookkeeping for power-flag synthesis.
    let mut used_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut driven_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut power_input_nets: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    // Records the last power-symbol attachment point per net (for flag placement).
    let mut power_attach: std::collections::BTreeMap<String, [f64; 2]> =
        std::collections::BTreeMap::new();

    for (block_name, block) in &design.blocks {
        for (refdes, comp) in &block.components {
            let identity = Identity::of(refdes, &comp.origin);

            // Surviving component -> prior position/angle/uuid; new -> placer.
            let (at, angle, uuid) = match prior_map.get(&identity) {
                Some(p) => (snap_point(p.at), p.angle, p.uuid.clone()),
                None => (
                    auto.positions.get(refdes).copied().unwrap_or([0.0, 0.0]),
                    initial_angle(comp),
                    None,
                ),
            };

            let value = comp.value.as_deref().unwrap_or("");
            let extra = ap_properties(block_name, &comp.origin);
            w.add_symbol_full(env, &comp.part, refdes, value, at, angle, &extra, uuid)?;

            let meta = provider.symbol(&comp.part);

            for (pin, target) in &comp.pins {
                emit_pin(
                    &mut w,
                    env,
                    &provider,
                    refdes,
                    pin,
                    target,
                    &power_nets,
                    &mut used_nets,
                    &mut power_attach,
                )?;
                record_power_role(meta, pin, target, &mut driven_nets, &mut power_input_nets);
            }
            for unit_pins in comp.units.values() {
                for (pin, target) in unit_pins {
                    emit_pin(
                        &mut w,
                        env,
                        &provider,
                        refdes,
                        pin,
                        target,
                        &power_nets,
                        &mut used_nets,
                        &mut power_attach,
                    )?;
                    record_power_role(meta, pin, target, &mut driven_nets, &mut power_input_nets);
                }
            }
        }
    }

    // Power flags: for nets that got a power symbol, place a flag pin-coincident
    // at the recorded attach point. For nets without a power symbol (undeclared
    // power-input nets still using a label), use the right-column flag placement.
    let mut needs_flag: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    needs_flag.extend(power_input_nets.iter().map(String::as_str));
    for (net, attrs) in &design.nets {
        if attrs.power && used_nets.contains(net.as_str()) {
            needs_flag.insert(net.as_str());
        }
    }
    for net in &driven_nets {
        needs_flag.remove(net.as_str());
    }

    let rightmost = design
        .blocks
        .values()
        .flat_map(|b| b.components.iter())
        .filter_map(|(refdes, comp)| {
            let identity = Identity::of(refdes, &comp.origin);
            prior_map
                .get(&identity)
                .map(|p| p.at[0])
                .or_else(|| auto.positions.get(refdes).map(|p| p[0]))
        })
        .fold(0.0_f64, f64::max);
    let power_x = rightmost + 50.8;

    for (flag_idx, net) in needs_flag.iter().enumerate() {
        let refdes = format!("#FLG{:02}", flag_idx + 1);
        if let Some(&attach) = power_attach.get(*net) {
            // Place the flag pin-coincident with the existing power symbol.
            w.add_power_flag_at(env, &refdes, attach)?;
        } else {
            // Fallback: right-column label-based flag for undeclared power nets.
            let y = 25.4 + flag_idx as f64 * 12.7;
            let at = snap_point([power_x, y]);
            w.add_power_flag(env, net, &refdes, at)?;
        }
    }

    Ok(w.finish())
}

/// The hidden `ap_*` identity properties for a component.
///
/// Every emitted symbol carries `ap_block`; synthesized parts additionally carry
/// `ap_role`/`ap_parent`/`ap_index`, while authored parts carry the explicit
/// `ap_role = "authored"` sentinel so a reader can distinguish "authored" from
/// "tags missing" (an older file) without ambiguity.
fn ap_properties(block_name: &str, origin: &Origin) -> Vec<(String, String)> {
    let mut props = vec![(AP_BLOCK.to_string(), block_name.to_string())];
    match origin {
        Origin::Authored => {
            props.push((AP_ROLE.to_string(), ROLE_AUTHORED.to_string()));
        }
        Origin::Synthesized {
            parent,
            role,
            index,
        } => {
            props.push((AP_ROLE.to_string(), role.clone()));
            props.push((AP_PARENT.to_string(), parent.clone()));
            props.push((AP_INDEX.to_string(), index.to_string()));
        }
    }
    props
}

/// See `crate::record_power_role` — duplicated signature kept private here so
/// the reconcile path doesn't depend on lib internals; logic is identical.
fn record_power_role(
    meta: Option<&circuit_lang::SymbolMeta>,
    pin: &str,
    target: &PinTarget,
    driven_nets: &mut std::collections::BTreeSet<String>,
    power_input_nets: &mut std::collections::BTreeSet<String>,
) {
    let PinTarget::Net(net) = target else { return };
    let Some(meta) = meta else { return };
    let matched = meta
        .pins
        .iter()
        .find(|p| p.number == pin)
        .or_else(|| meta.pins.iter().find(|p| p.name == pin));
    match matched.map(|pm| pm.etype) {
        Some(PinType::PowerOutput) => {
            driven_nets.insert(net.clone());
        }
        Some(PinType::PowerInput) => {
            power_input_nets.insert(net.clone());
        }
        _ => {}
    }
}

/// Emit one pin's connectivity (label or no-connect), recording referenced nets.
///
/// Power-net pins get a power symbol + stub wire instead of a text label.
/// Signal-net pins get the usual text label.
#[allow(clippy::too_many_arguments)]
fn emit_pin(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    provider: &RealSymbolProvider,
    refdes: &str,
    pin: &str,
    target: &PinTarget,
    power_nets: &std::collections::BTreeSet<String>,
    used_nets: &mut std::collections::BTreeSet<String>,
    power_attach: &mut std::collections::BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    match target {
        PinTarget::Net(net) => {
            used_nets.insert(net.clone());
            if power_nets.contains(net) {
                emit_power_pin(w, env, provider, refdes, pin, net, power_attach)
            } else {
                w.add_signal_label(env, refdes, pin, net)
            }
        }
        PinTarget::NoConnect => w.add_no_connect(env, refdes, pin),
    }
}

/// Emit a power symbol (+ stub wire) for a power-net pin.
///
/// Places a stub wire from the pin endpoint outward along the pin's direction,
/// then places a power symbol at the end of the stub (or via a riser for
/// horizontal pins). Records the attach point in `power_attach` for later
/// flag placement.
fn emit_power_pin(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    provider: &RealSymbolProvider,
    refdes: &str,
    pin: &str,
    net: &str,
    power_attach: &mut std::collections::BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    let lib_id = power_lib_id(net, provider);
    let down = is_ground(net);

    for (idx, (ep, dir)) in w.pin_dirs(env, refdes, pin)?.into_iter().enumerate() {
        let v = dir.vec();
        let stub_end = [ep[0] + v[0] * STUB_MM, ep[1] + v[1] * STUB_MM];
        w.add_wire(ep, stub_end);

        let (attach, angle) = match dir {
            Dir::North | Dir::South => {
                // Vertical pin: place the power symbol directly at the stub end.
                // For ground symbols (pointing down), the conventional orientation
                // is angle=0. For VCC-like (pointing up), also angle=0.
                // The key is that the power symbol's single pin (at origin) is at
                // `attach` — the connection point.
                let angle = if down && dir == Dir::South {
                    0.0
                } else if !down && dir == Dir::North {
                    0.0
                } else {
                    // Pin points wrong way for this net type — use 180° flip.
                    180.0
                };
                (stub_end, angle)
            }
            Dir::East | Dir::West => {
                // Horizontal pin: add a riser (down for GND, up for VCC) to bring
                // the power symbol to a conventional vertical position.
                let dy = if down { RISER_MM } else { -RISER_MM };
                let attach = [stub_end[0], stub_end[1] + dy];
                w.add_wire(stub_end, attach);
                (attach, 0.0)
            }
        };
        let pref = format!("#PWR_{refdes}_{pin}_{idx}");
        w.add_power_symbol(env, &lib_id, &pref, net, attach, angle)?;
        power_attach.entry(net.to_string()).or_insert(attach);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_of_authored_is_refdes() {
        assert_eq!(
            Identity::of("R1", &Origin::Authored),
            Identity::Authored("R1".to_string())
        );
    }

    #[test]
    fn identity_of_synthesized_is_provenance() {
        let o = Origin::Synthesized {
            parent: "U1".to_string(),
            role: "decouple".to_string(),
            index: 2,
        };
        assert_eq!(
            Identity::of("C9", &o),
            Identity::Synthesized {
                parent: "U1".to_string(),
                role: "decouple".to_string(),
                index: 2,
            }
        );
    }

    #[test]
    fn ap_properties_tags_authored_and_synthesized() {
        let authored = ap_properties("blk", &Origin::Authored);
        assert!(authored.contains(&(AP_BLOCK.to_string(), "blk".to_string())));
        assert!(authored.contains(&(AP_ROLE.to_string(), ROLE_AUTHORED.to_string())));

        let synth = ap_properties(
            "blk",
            &Origin::Synthesized {
                parent: "U1".to_string(),
                role: "decouple".to_string(),
                index: 0,
            },
        );
        assert!(synth.contains(&(AP_PARENT.to_string(), "U1".to_string())));
        assert!(synth.contains(&(AP_ROLE.to_string(), "decouple".to_string())));
        assert!(synth.contains(&(AP_INDEX.to_string(), "0".to_string())));
    }

    #[test]
    fn parse_prior_of_empty_text_is_empty() {
        assert!(parse_prior("not a schematic").is_empty());
    }
}
