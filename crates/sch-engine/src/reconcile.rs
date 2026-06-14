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

use indexmap::IndexMap;

use crate::emit::{Dir, SchematicWriter};
use crate::grammar::is_ground;
use crate::grid::snap_point;
use crate::place;

/// Stub wire length from a pin to its power symbol, in mm (3 grid units = 3.81mm).
const STUB_MM: f64 = 3.81;
/// Vertical riser from a horizontal stub to a power symbol, in mm (2 grid units).
const RISER_MM: f64 = 2.54;

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

/// Property key recording the layout-revision a component was placed under.
pub const AP_LAYOUT_REV: &str = "ap_layout_rev";

/// The `ap_role` value written for authored components.
pub const ROLE_AUTHORED: &str = "authored";

/// Which prior placements to discard on re-emit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Relayout {
    /// Honor every surviving prior placement whose layout-rev still matches.
    #[default]
    None,
    /// Discard all prior placements; the placer lays out everything fresh.
    All,
    /// Discard prior placements only for the named blocks.
    Blocks(std::collections::BTreeSet<String>),
}

impl Relayout {
    /// Whether prior placements for `block_name` must be discarded (placer lays
    /// the block out fresh). The single source of the relayout decision — shared
    /// by `resolve_placement` and the cluster-origin loop.
    fn forces(&self, block_name: &str) -> bool {
        match self {
            Relayout::All => true,
            Relayout::Blocks(names) => names.contains(block_name),
            Relayout::None => false,
        }
    }
}

/// The layout revision of one component: a content hash of its
/// placement-relevant inputs. Membership is deliberately NOT hashed — adding a
/// neighbor must not blow away drags.
///
/// `near` is hashed proactively: the placer does not honor it yet (only `edge`),
/// but including it now means the rev forward-covers `near` so that, once the
/// placer does honor it, changing `near` will correctly re-place — no migration
/// of already-emitted files needed.
fn layout_rev(
    block: &circuit_lang::model::Block,
    comp: &circuit_lang::model::Component,
    cluster: Option<&crate::grammar::Cluster>,
) -> String {
    // The placer token re-places everything once when the placement ALGORITHM
    // changes (anchor-centric placer = v2); bump it on future placer rewrites.
    let mut desc = format!(
        "placer=v2|edge={:?}|near={:?}|role={:?}",
        block.layout.edge, block.layout.near, comp.layout_role,
    );
    // Cluster members additionally hash the cluster's placement-relevant
    // structure, so a YAML edit that changes chains/banks re-places the block.
    if let Some(c) = cluster {
        use std::fmt::Write as _;
        let _ = write!(desc, "|cluster={}", grammar_rev(c));
    }
    crate::ids::stable_uuid("layout_rev", &desc)
}

/// The layout-rev of a cluster member: its component `layout_rev` extended with
/// the owning cluster's `grammar_rev`, so a YAML edit that restructures the
/// cluster re-places it (drags only survive structure-preserving edits).
fn member_rev(
    block: &circuit_lang::model::Block,
    comp: &circuit_lang::model::Component,
    cluster: &crate::grammar::Cluster,
) -> String {
    layout_rev(block, comp, Some(cluster))
}

/// Content hash of a cluster's placement-relevant structure. A change to any of
/// the hashed inputs re-places the cluster (drags survive only structure-
/// preserving edits). Hashed, and thus triggering a re-place:
///   - each chain's class, plus every link's refdes and its `(a_net -> b_net)`;
///   - each bank's `(a_net, b_net)` plus its member list.
pub fn grammar_rev(cluster: &crate::grammar::Cluster) -> String {
    use std::fmt::Write as _;
    let mut desc = String::new();
    for c in &cluster.chains {
        let _ = write!(desc, "chain[{:?}]:", c.class);
        for l in &c.links {
            let _ = write!(desc, "{}({}->{})|", l.refdes, l.a_net, l.b_net);
        }
    }
    for b in &cluster.banks {
        let _ = write!(desc, "bank[{}/{}]:{:?}|", b.a_net, b.b_net, b.members);
    }
    crate::ids::stable_uuid("grammar_rev", &desc)
}

/// The reconciled placement of one component: its emitted position/angle/uuid,
/// the layout-rev it was computed under, and whether it was re-placed (had a
/// prior but the rev changed or relayout forced a fresh placement).
///
/// The SINGLE source of truth for both emission and decoration (power column,
/// block frames): both the main placement loop and the decoration loops call
/// [`resolve_placement`], so framing can never diverge from what was actually
/// emitted (the bug this slice was built to avoid).
struct ResolvedPlacement {
    /// Emitted sheet position (mm) — always concrete (the placer positions every
    /// component), so decoration uses it directly without a `None` fallback.
    at: [f64; 2],
    /// Emitted orientation in degrees.
    angle: f64,
    /// The instance uuid to reuse (`Some` only when preserving a prior).
    uuid: Option<String>,
    /// Whether this component was re-placed (prior existed but the rev changed or
    /// relayout forced a fresh placement).
    replaced: bool,
    /// The layout-rev this component was placed under (recomputed here once; the
    /// caller reuses it for `ap_properties` rather than hashing twice).
    rev: String,
}

/// Resolve one component's reconciled placement — the single decision shared by
/// emission and decoration. Surviving + still-valid (rev matches, no relayout)
/// preserves the prior position/angle/uuid; otherwise the placer's auto-position
/// is used, flagged `replaced` when a prior existed.
fn resolve_placement(
    prior_map: &HashMap<Identity, PriorPlacement>,
    auto: &place::Layout,
    relayout: &Relayout,
    block_name: &str,
    block: &circuit_lang::model::Block,
    refdes: &str,
    comp: &circuit_lang::model::Component,
) -> ResolvedPlacement {
    let identity = Identity::of(refdes, &comp.origin);
    let rev = layout_rev(block, comp, None);
    let block_relayout = relayout.forces(block_name);
    // An old file with no recorded rev preserves (migration path); a recorded rev
    // must match to preserve.
    let prior_ok = |p: &PriorPlacement| p.layout_rev.as_deref().is_none_or(|r| r == rev);
    let fresh = || auto.positions.get(refdes).copied().unwrap_or([0.0, 0.0]);
    // The placer positions cluster members relative to the orientation it chose
    // for them; emitting them at a different angle would swap their pin ends and
    // break connectivity. So a fresh placement uses the placer's angle when it
    // has one. Anchors get angle 0 from the placer, which equals
    // `initial_angle` for them (they are never RailSpan); RailSpan passives are
    // always cluster members, so their geometry angle (which the placer records)
    // supersedes `initial_angle`. Components the placer never positioned (none in
    // practice — `analyze` classifies all) fall back to `initial_angle`.
    let fresh_angle = || {
        auto.angles
            .get(refdes)
            .copied()
            .unwrap_or_else(|| initial_angle(comp))
    };

    match prior_map.get(&identity) {
        // Surviving + still-valid -> prior position/angle/uuid.
        Some(p) if !block_relayout && prior_ok(p) => ResolvedPlacement {
            at: snap_point(p.at),
            angle: p.angle,
            uuid: p.uuid.clone(),
            replaced: false,
            rev,
        },
        // Prior existed but rev changed or relayout forced it -> placer, replaced.
        Some(_) => ResolvedPlacement {
            at: fresh(),
            angle: fresh_angle(),
            uuid: None,
            replaced: true,
            rev,
        },
        // Genuinely new -> placer, not a re-placement.
        None => ResolvedPlacement {
            at: fresh(),
            angle: fresh_angle(),
            uuid: None,
            replaced: false,
            rev,
        },
    }
}

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
    /// The layout-revision recorded when this symbol was last placed, if any.
    /// `None` for an older file written before `ap_layout_rev` existed.
    pub layout_rev: Option<String>,
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
                layout_rev: prop(AP_LAYOUT_REV).map(str::to_string),
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

/// The rendered schematic plus deterministic readability findings.
pub struct EmitOutput {
    /// The assembled `.kicad_sch` document text.
    pub sch: String,
    /// One human-readable warning per overlapping symbol/label pair (empty when
    /// the layout is clean). A side-channel only: it does not alter `sch`.
    pub layout_warnings: Vec<String>,
    /// Per-block count of components re-placed this emit (rev changed or a
    /// `Relayout` forced it). Empty when every surviving placement was preserved.
    pub relayout_blocks: std::collections::BTreeMap<String, usize>,
}

/// All grammar-derived emission inputs, built once per emit and shared by the
/// placer, the rigid-member component loop, and cluster decoration.
pub(crate) struct GrammarInputs {
    pub sizes: place::SizeMap,
    pub graphs: IndexMap<String, crate::grammar::BlockGraph>,
    /// Per-block cluster geometry, indexed parallel to `graphs[block].clusters`
    /// by cluster index `ci`: `geoms[block][ci]` is the geometry of
    /// `graphs[block].clusters[ci]`.
    pub geoms: IndexMap<String, Vec<crate::cluster_geom::ClusterGeom>>,
    /// Angle-0 sheet endpoint + outward direction of each anchor pin, keyed by
    /// `(anchor refdes, pin)`, used to slot clusters against their anchor pins.
    pub anchor_pin_ends: place::AnchorPinEnds,
    /// Cluster membership per refdes.
    pub members: IndexMap<String, MemberInfo>,
    /// All cluster-covered pins.
    pub covered: std::collections::BTreeSet<(String, String)>,
}

/// Where a refdes sits inside its owning cluster's geometry.
///
/// `cluster_key` and `cluster_idx` are two views of the SAME membership:
/// `cluster_key == place::cluster_key(block, cluster_idx)`. `cluster_key` indexes
/// the `origins` map; `cluster_idx` indexes `graphs[block].clusters` / the
/// parallel `geoms[block]`.
pub(crate) struct MemberInfo {
    /// [`place::cluster_key`] of the owning cluster.
    pub cluster_key: String,
    /// Index of the cluster within its block's `BlockGraph::clusters`.
    pub cluster_idx: usize,
    /// Local symbol-origin position inside the cluster geometry.
    pub local: [f64; 2],
    /// Engine-chosen orientation, degrees.
    pub angle: f64,
}

/// Build every grammar-derived input the cluster-as-unit emission needs:
/// per-component sizes, per-block grammar graphs, per-cluster geometry (with the
/// set of nets that carry a label), anchor pin endpoints for slotting, and the
/// member/covered-pin walk over the geometry.
pub(crate) fn build_grammar_inputs(env: &KicadEnv, design: &Design) -> GrammarInputs {
    let provider = RealSymbolProvider::new(env.clone());

    // 1. Per-component approximate sizes (bbox-aware placement cells). Parts
    //    whose geometry can't load fall back to the placer's fixed legacy cell.
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

    // 2. Per-block grammar analysis.
    let mut graphs: IndexMap<String, crate::grammar::BlockGraph> = IndexMap::new();
    for block_name in design.blocks.keys() {
        graphs.insert(
            block_name.clone(),
            crate::grammar::analyze(design, block_name, &provider),
        );
    }

    // 3. Pin callback: angle-0 sheet end of `(refdes, pin)` via `emit::pin_end0`
    //    (first end — multi-end pin names don't occur on 2-pin passives), so
    //    cluster geometry orients each element by its TRUE pin sides (consistent
    //    with what emission draws). pin_end0's loader caches geometry per part.
    let mut refdes_part: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            refdes_part.insert(refdes.clone(), comp.part.clone());
        }
    }
    let pin_cb = |refdes: &str, pin: &str| -> Option<[f64; 2]> {
        let part = refdes_part.get(refdes)?;
        crate::emit::pin_end0(env, part, pin)
            .ok()
            .and_then(|ends| ends.into_iter().next())
    };

    // 4. Power nets (declared rails / explicit power) — excluded from labeling.
    let power_nets: std::collections::BTreeSet<String> = design
        .nets
        .iter()
        .filter(|(_, a)| a.power)
        .map(|(n, _)| n.clone())
        .collect();

    // 5. Per-cluster geometry. `labeled` = nets that must carry exactly one net
    //    label in this block: external, anchor-tapped, or multi-way (>2 chain
    //    pins) signal nets, minus power nets.
    let mut geoms: IndexMap<String, Vec<crate::cluster_geom::ClusterGeom>> = IndexMap::new();
    for (block_name, g) in &graphs {
        let block = &design.blocks[block_name.as_str()];
        let uses = crate::grammar::net_uses(design, block_name, &provider);
        let mut labeled: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (net, u) in &uses {
            if power_nets.contains(net) {
                continue;
            }
            // Label a non-power net when it must reach outside the cluster's own
            // internal wiring:
            //  - external (cross-block label connectivity),
            //  - anchor-tapped (an anchor pin joins it),
            //  - a multi-way node (>2 chain pins — one clarity tap on the star),
            //  - an OPEN endpoint (exactly one chain pin and no anchor pin): its
            //    single covered pin is otherwise dangling, so without a label the
            //    pin would ERC as `pin_not_connected`. (A 2-chain-pin through/node
            //    net is wired pin-to-pin internally and needs no label.)
            let open_endpoint = u.chain_pins.len() == 1 && u.anchor_pins.is_empty();
            if u.external || !u.anchor_pins.is_empty() || u.chain_pins.len() > 2 || open_endpoint {
                labeled.insert(net.clone());
            }
        }
        let gs = g
            .clusters
            .iter()
            .map(|c| {
                crate::cluster_geom::layout_cluster(
                    c,
                    block,
                    &labeled,
                    &pin_cb,
                    &(|r: &str| sizes.get(r).copied().unwrap_or([5.08, 10.16])),
                )
            })
            .collect();
        geoms.insert(block_name.clone(), gs);
    }

    // 6. Anchor pin endpoints for slotting: for every (net, aref, apin) in every
    //    cluster's anchor_taps, the pin's angle-0 offset + outward direction.
    let mut anchor_pin_ends = place::AnchorPinEnds::new();
    for (block_name, g) in &graphs {
        let block = &design.blocks[block_name.as_str()];
        for cluster in &g.clusters {
            for (_net, aref, apin) in &cluster.anchor_taps {
                let key = (aref.clone(), apin.clone());
                if anchor_pin_ends.contains_key(&key) {
                    continue;
                }
                let Some(comp) = block.components.get(aref.as_str()) else {
                    continue;
                };
                let Ok(ends) = crate::emit::pin_end0(env, &comp.part, apin) else {
                    continue;
                };
                let Some(&off) = ends.first() else { continue };
                // Pin angle for direction: load geometry, resolve number-then-name.
                let Ok(geom) = kicad_bridge::geometry::SymbolGeometry::load(env, &comp.part) else {
                    continue;
                };
                let pg = geom
                    .pins
                    .iter()
                    .find(|p| p.number == *apin)
                    .or_else(|| geom.pins.iter().find(|p| p.name == *apin));
                let Some(pg) = pg else { continue };
                let dir = crate::emit::quantize_dir(pg.angle, 0.0, false);
                anchor_pin_ends.insert(key, (off, dir));
            }
        }
    }

    // 7. Members + covered pins, walked from the geometry.
    let mut members: IndexMap<String, MemberInfo> = IndexMap::new();
    let mut covered: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    for (block_name, gs) in &geoms {
        for (ci, geom) in gs.iter().enumerate() {
            let key = place::cluster_key(block_name, ci);
            for (refdes, local, angle) in &geom.placements {
                members.insert(
                    refdes.clone(),
                    MemberInfo {
                        cluster_key: key.clone(),
                        cluster_idx: ci,
                        local: *local,
                        angle: *angle,
                    },
                );
            }
            for (r, p) in &geom.covered {
                covered.insert((r.clone(), p.clone()));
            }
        }
    }

    GrammarInputs {
        sizes,
        graphs,
        geoms,
        anchor_pin_ends,
        members,
        covered,
    }
}

/// Resolve every cluster's sheet ORIGIN before placing members (a self-contained
/// emission phase). A cluster is rigid: its representative member
/// (`members()[0]`) anchors the whole group. If that member's prior placement
/// survives (identity match, rev unchanged, block not forced to relayout), the
/// origin is recovered from it (`prior.at - rep_local`) so a user's whole-cluster
/// drag is preserved; otherwise the placer's auto origin is used and every member
/// counts toward `relayout_blocks`.
///
/// NOTE on member_rev redundancy: this loop computes `member_rev` for the
/// representative, and the component loop later recomputes `member_rev` for every
/// member. That recomputation is NOT eliminated here because `member_rev` →
/// `layout_rev` hashes `comp.layout_role` (a per-COMPONENT field): a cluster can
/// mix a `RailSpan` passive with `None`-role members, so members of one cluster
/// can get *different* revs. Caching the representative's rev by cluster-key would
/// change the rev written to `ap_layout_rev` for the differing members — a
/// behavior change the layout_rev tests would catch. So the per-member recompute
/// stays; only the origin decision is lifted out here.
fn resolve_cluster_origins(
    design: &Design,
    gi: &GrammarInputs,
    auto: &place::Layout,
    prior_map: &HashMap<Identity, PriorPlacement>,
    relayout: &Relayout,
    relayout_blocks: &mut std::collections::BTreeMap<String, usize>,
) -> IndexMap<String, [f64; 2]> {
    let mut origins: IndexMap<String, [f64; 2]> = IndexMap::new();
    for (block_name, graph) in &gi.graphs {
        let block = &design.blocks[block_name.as_str()];
        for (ci, cluster) in graph.clusters.iter().enumerate() {
            let key = place::cluster_key(block_name, ci);
            let auto_origin = auto
                .cluster_origins
                .get(&key)
                .copied()
                .unwrap_or([0.0, 0.0]);
            let members = cluster.members();
            let Some(rep) = members.first() else {
                origins.insert(key, snap_point(auto_origin));
                continue;
            };
            let Some(rep_comp) = block.components.get(rep.as_str()) else {
                origins.insert(key, snap_point(auto_origin));
                continue;
            };
            let rep_local = gi.members.get(rep).map(|m| m.local).unwrap_or([0.0, 0.0]);
            let rev = member_rev(block, rep_comp, cluster);
            let block_relayout = relayout.forces(block_name);
            let prior = prior_map.get(&Identity::of(rep, &rep_comp.origin));
            let origin = match prior {
                Some(p)
                    if !block_relayout && p.layout_rev.as_deref().is_none_or(|r| r == rev) =>
                {
                    [p.at[0] - rep_local[0], p.at[1] - rep_local[1]]
                }
                Some(_) => {
                    *relayout_blocks.entry(block_name.clone()).or_insert(0) += members.len();
                    auto_origin
                }
                None => auto_origin,
            };
            origins.insert(key, snap_point(origin));
        }
    }
    origins
}

/// Cluster DECORATION (a self-contained emission phase): translate each cluster's
/// geometry by its sheet origin and emit the wires/junctions/power-ports/labels
/// that wire its members together, then close the anchor-slot join wires.
///
/// The `#PWR_CL{n:02}` counter is global across all clusters and advances in the
/// deterministic iteration order of `gi.graphs` (an IndexMap) × each block's
/// `clusters` × `geom.ports` — so the pwr_n numbering is reproducible run-to-run.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn emit_cluster_decoration(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    gi: &GrammarInputs,
    origins: &IndexMap<String, [f64; 2]>,
    auto: &place::Layout,
    provider: &RealSymbolProvider,
    pending: &mut PendingNets,
    used_nets: &mut std::collections::BTreeSet<String>,
    power_attach: &mut std::collections::BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    const EPS: f64 = 1e-6;
    // Global counter: `#PWR_CL{n:02}` numbering depends on gi.graphs IndexMap
    // order (deterministic), so the port references are reproducible.
    let mut pwr_n = 0usize;
    for (block_name, graph) in &gi.graphs {
        for (ci, _cluster) in graph.clusters.iter().enumerate() {
            let key = place::cluster_key(block_name, ci);
            let o = origins[&key];
            let t = |p: [f64; 2]| [p[0] + o[0], p[1] + o[1]];
            let geom = &gi.geoms[block_name][ci];
            // A label's STUB wire (the short spur from the net's node to the
            // label text) is deferred together with the label: if the routing
            // pass wires the net, neither is drawn. Structural wires emit now.
            let is_label_stub = |a: [f64; 2], b: [f64; 2], net: &str| {
                geom.labels.iter().find_map(|(ln, lp, dir)| {
                    if ln != net {
                        return None;
                    }
                    let eq = |p: [f64; 2], q: [f64; 2]| {
                        (p[0] - q[0]).abs() < EPS && (p[1] - q[1]).abs() < EPS
                    };
                    if eq(a, *lp) {
                        Some((b, *lp, *dir))
                    } else if eq(b, *lp) {
                        Some((a, *lp, *dir))
                    } else {
                        None
                    }
                })
            };
            for (a, b, net) in &geom.wires {
                if let Some((tap, lp, dir)) = is_label_stub(*a, *b, net) {
                    pending
                        .cluster_labels
                        .entry(net.clone())
                        .or_default()
                        .push((t(tap), t(lp), dir));
                    pending
                        .blocks
                        .entry(net.clone())
                        .or_default()
                        .insert(block_name.to_string());
                    used_nets.insert(net.clone());
                    continue;
                }
                w.add_wire_on_net(t(*a), t(*b), net);
            }
            for j in &geom.junctions {
                w.add_junction(t(*j));
            }
            for (net, p) in &geom.ports {
                pwr_n += 1;
                let lib = power_lib_id(net, provider);
                w.add_power_symbol(env, &lib, &format!("#PWR_CL{pwr_n:02}"), net, t(*p), 0.0)?;
                used_nets.insert(net.clone());
                power_attach.entry(net.clone()).or_insert(t(*p));
            }
            // Labels whose stub was deferred above are covered; a label with
            // NO matching stub wire (sits directly on geometry) defers too,
            // tap == label position.
            for (net, p, dir) in &geom.labels {
                let already = pending
                    .cluster_labels
                    .get(net.as_str())
                    .is_some_and(|v| v.iter().any(|(_, lp, _)| {
                        (lp[0] - t(*p)[0]).abs() < EPS && (lp[1] - t(*p)[1]).abs() < EPS
                    }));
                if !already {
                    pending
                        .cluster_labels
                        .entry(net.clone())
                        .or_default()
                        .push((t(*p), t(*p), *dir));
                    pending
                        .blocks
                        .entry(net.clone())
                        .or_default()
                        .insert(block_name.to_string());
                }
                used_nets.insert(net.clone());
            }
        }
    }
    for (a, b, net) in &auto.joins {
        w.add_wire_on_net(*a, *b, net);
    }
    Ok(())
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
    relayout: &Relayout,
) -> io::Result<EmitOutput> {
    // Build every grammar-derived input (sizes, graphs, cluster geometry, anchor
    // pin ends, cluster membership, covered pins) once. Clusters emit as rigid
    // wired units: members place at `origin + local`, the cluster draws its own
    // wires/buses/ports/labels, and covered pins skip per-pin label emission.
    let gi = build_grammar_inputs(env, design);
    let auto = place::place_with_anchor_pins(
        design,
        &gi.sizes,
        &gi.graphs,
        &gi.geoms,
        &gi.anchor_pin_ends,
    );
    let prior_map = prior.map(parse_prior).unwrap_or_default();

    let mut w = SchematicWriter::new();
    if let Some(name) = &design.name {
        w.set_title(name);
    }
    let provider = RealSymbolProvider::new(env.clone());

    // Collect power nets (declared via `rails:` or explicit `power: true`).
    let power_nets: std::collections::BTreeSet<String> = design
        .nets
        .iter()
        .filter(|(_, a)| a.power)
        .map(|(n, _)| n.clone())
        .collect();

    // Per-block tally of components re-placed (rev changed or relayout forced).
    let mut relayout_blocks: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    // Resolve every cluster's sheet ORIGIN before placing members (a phase lifted
    // into `resolve_cluster_origins`). Accumulates into `relayout_blocks`.
    let origins = resolve_cluster_origins(
        design,
        &gi,
        &auto,
        &prior_map,
        relayout,
        &mut relayout_blocks,
    );

    // Pin endpoints already wired by a placement join: their per-pin signal
    // label would be redundant (the join wire connects them to the cluster,
    // whose own single label names the net). Keyed by snapped-position bits.
    let join_points: std::collections::BTreeSet<(u64, u64)> = auto
        .joins
        .iter()
        .map(|(pin, _, _)| {
            let p = crate::grid::snap_point(*pin);
            (p[0].to_bits(), p[1].to_bits())
        })
        .collect();

    // Signal connectivity deferred for the routing pass (wires vs labels).
    let mut pending = PendingNets::default();

    // Net bookkeeping for power-flag synthesis.
    let mut used_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut driven_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut power_input_nets: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    // Records the last power-symbol attachment point per net (for flag placement).
    let mut power_attach: std::collections::BTreeMap<String, [f64; 2]> =
        std::collections::BTreeMap::new();

    // What each component was ACTUALLY emitted at. The single source of truth
    // for frames/rightmost: filled in the component loop, read by the decoration
    // loops, so framing can never diverge from emission.
    let mut emitted_at: IndexMap<String, [f64; 2]> = IndexMap::new();

    for (block_name, block) in &design.blocks {
        for (refdes, comp) in &block.components {
            // Cluster members short-circuit `resolve_placement`: their position
            // is `origin + local` and their angle comes from the cluster
            // geometry (emitting at a different angle would swap pin ends and
            // short nets). The instance uuid is still preserved by identity when
            // the prior had one. Non-members take the existing anchor path.
            let (at, angle, uuid, rev) = match gi.members.get(refdes.as_str()) {
                Some(m) => {
                    let o = origins[&m.cluster_key];
                    let at = snap_point([o[0] + m.local[0], o[1] + m.local[1]]);
                    let cluster = &gi.graphs[block_name].clusters[m.cluster_idx];
                    let rev = member_rev(block, comp, cluster);
                    let uuid = prior_map
                        .get(&Identity::of(refdes, &comp.origin))
                        .and_then(|p| p.uuid.clone());
                    (at, m.angle, uuid, rev)
                }
                None => {
                    let ResolvedPlacement {
                        at,
                        angle,
                        uuid,
                        replaced,
                        rev,
                    } = resolve_placement(
                        &prior_map, &auto, relayout, block_name, block, refdes, comp,
                    );
                    if replaced {
                        *relayout_blocks.entry(block_name.to_string()).or_insert(0) += 1;
                    }
                    (at, angle, uuid, rev)
                }
            };
            emitted_at.insert(refdes.clone(), at);

            let value = comp.value.as_deref().unwrap_or("");
            let extra = ap_properties(block_name, &comp.origin, &rev);
            w.add_symbol_full(env, &comp.part, refdes, value, at, angle, &extra, uuid)?;

            let meta = provider.symbol(&comp.part);

            // Covered pins (their connectivity is the cluster's own wiring) skip
            // per-pin label/power-symbol emission, but `record_power_role` still
            // runs for EVERY pin so power-flag bookkeeping sees the full picture.
            // Component-level pins and every unit's pins are handled identically,
            // so walk them as one stream to keep the skip logic single-sourced.
            let all_pins = comp
                .pins
                .iter()
                .chain(comp.units.values().flat_map(|u| u.iter()));
            for (pin, target) in all_pins {
                if !gi.covered.contains(&(refdes.clone(), pin.clone())) {
                    emit_pin(
                        &mut w,
                        env,
                        &provider,
                        block_name,
                        refdes,
                        pin,
                        target,
                        &power_nets,
                        &join_points,
                        &mut pending,
                        &mut used_nets,
                        &mut power_attach,
                    )?;
                }
                record_power_role(meta, pin, target, &mut driven_nets, &mut power_input_nets);
            }
        }
    }

    // Cluster DECORATION: translate each cluster's geometry by its origin and
    // emit the wires/junctions/ports/labels that wire its members together, then
    // close the anchor-slot join wires (a phase lifted into
    // `emit_cluster_decoration`).
    emit_cluster_decoration(
        &mut w,
        env,
        &gi,
        &origins,
        &auto,
        &provider,
        &mut pending,
        &mut used_nets,
        &mut power_attach,
    )?;

    // A component's *emitted* position (frames/rightmost). The map is the single
    // source: it records exactly what the component loop wrote.
    let resolved_at = |refdes: &str| -> [f64; 2] {
        emitted_at.get(refdes).copied().unwrap_or([0.0, 0.0])
    };

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
        .flat_map(|b| b.components.keys())
        .map(|refdes| resolved_at(refdes)[0])
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

    // ---- Routing pass: wires for local nets, labels as fallback ----
    //
    // Every deferred net is either ROUTED (real wires + junction dots; its
    // labels are dropped — terminals connect by copper) or LABELED exactly as
    // the pre-router engine did (per-pin stub labels + cluster label with its
    // stub wire). Routable = non-power, all terminals in ONE block, >= 2
    // terminals. A routable net that fails to route falls back to labels AND
    // records a degradation note (spec: visible degradation, never an error).
    let mut route_notes: Vec<String> = Vec::new();
    {
        // Scene: fixed geometry + every OTHER pending terminal as a foreign
        // anchor (those points become labels or wires later; touching one
        // would merge nets).
        let mut scene = w.route_scene();
        for (net, eps) in &pending.signals {
            for (_, _, p, _) in eps {
                scene.points.push((*p, net.clone()));
            }
        }
        for (net, ls) in &pending.cluster_labels {
            for (tap, _, _) in ls {
                scene.points.push((*tap, net.clone()));
            }
        }

        let all_nets: std::collections::BTreeSet<String> = pending
            .signals
            .keys()
            .chain(pending.cluster_labels.keys())
            .cloned()
            .collect();

        for net in &all_nets {
            let sigs = pending.signals.get(net.as_str()).cloned().unwrap_or_default();
            let clabels = pending
                .cluster_labels
                .get(net.as_str())
                .cloned()
                .unwrap_or_default();

            // Terminals: signal pin endpoints (with outward dir) + cluster taps.
            let mut terminals: Vec<([f64; 2], Option<crate::emit::Dir>)> = Vec::new();
            for (_, _, p, dir) in &sigs {
                terminals.push((*p, Some(*dir)));
            }
            for (tap, _, _) in &clabels {
                terminals.push((*tap, None));
            }

            let single_block = pending
                .blocks
                .get(net.as_str())
                .map(|b| b.len() == 1)
                .unwrap_or(false);
            let routable = single_block && terminals.len() >= 2;

            let mut routed_paths: Option<Vec<crate::route::Path>> = None;
            if routable {
                let pts: Vec<[f64; 2]> = terminals.iter().map(|t| t.0).collect();
                let mut paths: Vec<crate::route::Path> = Vec::new();
                let mut ok = true;
                for (i, j) in crate::route::mst_edges(&pts) {
                    // Prefer starting from a terminal with a known outward
                    // dir (a pin); synthesize a direction toward the target
                    // otherwise.
                    let (a, da, b) = match (terminals[i].1, terminals[j].1) {
                        (Some(d), _) => (pts[i], d, pts[j]),
                        (None, Some(d)) => (pts[j], d, pts[i]),
                        (None, None) => {
                            let d = if (pts[j][0] - pts[i][0]).abs()
                                >= (pts[j][1] - pts[i][1]).abs()
                            {
                                if pts[j][0] >= pts[i][0] {
                                    crate::emit::Dir::East
                                } else {
                                    crate::emit::Dir::West
                                }
                            } else if pts[j][1] >= pts[i][1] {
                                crate::emit::Dir::South
                            } else {
                                crate::emit::Dir::North
                            };
                            (pts[i], d, pts[j])
                        }
                    };
                    match crate::route::route_edge(a, da, b, net, &scene) {
                        Some(p) => paths.push(p),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    routed_paths = Some(paths);
                }
            }

            match routed_paths {
                Some(paths) => {
                    for path in &paths {
                        for seg in path.windows(2) {
                            w.add_wire_on_net(seg[0], seg[1], net);
                            scene.segments.push((seg[0], seg[1], net.clone()));
                        }
                    }
                    // Junction dots: 3-way meets among the routed paths plus
                    // the net's pre-existing wires (cluster wiring at taps).
                    let mut all: Vec<crate::route::Path> = paths.clone();
                    for (a, b) in w.wire_segments_on_net(net) {
                        all.push(vec![a, b]);
                    }
                    for j in crate::route::junction_points(&all) {
                        w.add_junction(j);
                    }
                    // A route ending INSIDE an existing same-net wire is a T
                    // that junction_points (ends-only) cannot see.
                    for (p, _) in &terminals {
                        let interior = w.wire_segments_on_net(net).iter().any(|(a, b)| {
                            let ends = ((p[0] - a[0]).abs() < 1e-6
                                && (p[1] - a[1]).abs() < 1e-6)
                                || ((p[0] - b[0]).abs() < 1e-6 && (p[1] - b[1]).abs() < 1e-6);
                            !ends && crate::emit::point_on_segment(*p, *a, *b)
                        });
                        if interior {
                            w.add_junction(*p);
                        }
                    }
                }
                None => {
                    // Label fallback: classic per-pin stub labels (dedup by
                    // pin — add_signal_label covers all endpoints of a pin)
                    // and cluster labels with their stub wires.
                    let mut seen: std::collections::BTreeSet<(String, String)> =
                        std::collections::BTreeSet::new();
                    for (refdes, pin, _, _) in &sigs {
                        if seen.insert((refdes.clone(), pin.clone())) {
                            w.add_signal_label(env, refdes, pin, net)?;
                        }
                    }
                    for (tap, lp, dir) in &clabels {
                        w.add_wire_on_net(*tap, *lp, net);
                        w.add_cluster_label(net, *lp, *dir, false);
                    }
                    if routable {
                        let block = pending
                            .blocks
                            .get(net.as_str())
                            .and_then(|b| b.iter().next().cloned())
                            .unwrap_or_default();
                        route_notes
                            .push(format!("route: block {block}: net {net} fell back to labels"));
                    }
                }
            }
        }
    }

    // Block frames + titles: drawn around the current positions, regenerated
    // every emit like all decoration. Each block's computed content bbox `b` is
    // captured into `frame_bounds` for the post-emit sparseness lint.
    const FRAME_PAD_MM: f64 = 7.62;
    let mut frame_bounds: std::collections::BTreeMap<String, [f64; 4]> =
        std::collections::BTreeMap::new();
    for (block_name, block) in &design.blocks {
        let mut bounds: Option<[f64; 4]> = None; // min_x, min_y, max_x, max_y
        for refdes in block.components.keys() {
            let at = resolved_at(refdes);
            let half = gi
                .sizes
                .get(refdes)
                .map(|s| [s[0] / 2.0, s[1] / 2.0])
                .unwrap_or([12.7, 12.7]);
            let b = bounds.get_or_insert([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
            b[0] = b[0].min(at[0] - half[0]);
            b[1] = b[1].min(at[1] - half[1]);
            b[2] = b[2].max(at[0] + half[0]);
            b[3] = b[3].max(at[1] + half[1]);
        }
        let Some(b) = bounds else { continue };
        frame_bounds.insert(block_name.clone(), b);
        let start = [b[0] - FRAME_PAD_MM, b[1] - FRAME_PAD_MM];
        let end = [b[2] + FRAME_PAD_MM, b[3] + FRAME_PAD_MM];
        w.add_rect(start, end, &format!("frame:{block_name}"));
        w.add_text(
            block_name,
            [start[0], start[1] - 1.27],
            2.54,
            true,
            &format!("title:{block_name}"),
        );
        if let Some(note) = &block.note {
            w.add_text(
                note,
                [start[0], end[1] + 3.81],
                1.27,
                false,
                &format!("note:{block_name}"),
            );
        }
    }

    // Lint the POST-retraction geometry. `finish` retracts colliding signal
    // stubs (moving a label off its stub end back onto the pin endpoint,
    // keeping its outward dir, stub cleared) before rendering, so linting `w`
    // as-is would bbox
    // labels at positions/orientations that never get emitted — yielding false
    // positives (overlaps retraction removes) and false negatives (a retracted
    // label now on its pin may overlap its own body, unseen). Run the retraction
    // pass first, then the text-position solver, so the lint sees exactly what
    // `finish` will emit. Both passes are idempotent, so `finish`'s own calls
    // below are harmless no-ops.
    w.retract_colliding_stubs();
    w.solve_text_positions();

    // Adjacency-aware overlap lint: every PAIR of members within a single
    // CLUSTER is an intentional tight adjacency — bank caps pack at
    // BANK_PITCH, chain links join pin-to-pin, and hangs share their node's
    // column, so the padded body/pin-text lint cells interpenetrate by
    // construction. The cluster geometry is closed-form engine output; the
    // lint's job is collisions BETWEEN units (and text everywhere, which the
    // solver places against real obstacles).
    let mut adjacency_pairs: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    let allow = |a: &str, b: &str, set: &mut std::collections::BTreeSet<(String, String)>| {
        let (a, b) = (a.to_string(), b.to_string());
        set.insert(if a <= b { (a, b) } else { (b, a) });
    };
    for graph in gi.graphs.values() {
        for cluster in &graph.clusters {
            let members = cluster.members();
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    allow(&members[i], &members[j], &mut adjacency_pairs);
                }
            }
        }
    }
    let mut layout_warnings = w.layout_warnings_excluding(&adjacency_pairs);

    // Surface grammar degradations (cycle-break notes etc.) so downstream sees
    // where the analysis had to give up structure. `analyze` already prefixes
    // each note with `block <name>: …`.
    for graph in gi.graphs.values() {
        layout_warnings.extend(graph.degradations.iter().map(|d| format!("grammar: {d}")));
    }
    // Routing degradations: local nets that fell back to label connectivity.
    layout_warnings.extend(route_notes);
    // Sparseness: a block whose frame area dwarfs its content reads as floating
    // parts; surface it so the vision loop isn't spent on mechanical whitespace.
    for (block_name, block) in &design.blocks {
        let content: f64 = block
            .components
            .keys()
            .map(|r| gi.sizes.get(r).map(|s| s[0] * s[1]).unwrap_or(129.0))
            .sum();
        let Some(b) = frame_bounds.get(block_name) else {
            continue;
        };
        let frame = (b[2] - b[0]) * (b[3] - b[1]);
        if content > 0.0 && frame > 8.0 * content {
            layout_warnings.push(format!(
                "sparse: block {block_name} frame is {:.0}x its content area",
                frame / content
            ));
        }
    }

    Ok(EmitOutput {
        sch: w.finish(),
        layout_warnings,
        relayout_blocks,
    })
}

/// The hidden `ap_*` identity properties for a component.
///
/// Every emitted symbol carries `ap_block`; synthesized parts additionally carry
/// `ap_role`/`ap_parent`/`ap_index`, while authored parts carry the explicit
/// `ap_role = "authored"` sentinel so a reader can distinguish "authored" from
/// "tags missing" (an older file) without ambiguity.
fn ap_properties(block_name: &str, origin: &Origin, rev: &str) -> Vec<(String, String)> {
    let mut props = vec![
        (AP_BLOCK.to_string(), block_name.to_string()),
        (AP_LAYOUT_REV.to_string(), rev.to_string()),
    ];
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
    let matched = circuit_lang::find_pin(&meta.pins, pin);
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

/// Connectivity work deferred until routing can decide wires vs labels.
#[derive(Default)]
struct PendingNets {
    /// net -> signal endpoints: (refdes, pin, snapped endpoint, outward dir).
    signals: std::collections::BTreeMap<String, Vec<(String, String, [f64; 2], crate::emit::Dir)>>,
    /// net -> cluster label specs: (tap point on the cluster wiring, label
    /// position, label dir). The tap is the net's wire terminal; the label
    /// stub wire tap->pos is only drawn when the net falls back to labels.
    cluster_labels: std::collections::BTreeMap<String, Vec<([f64; 2], [f64; 2], crate::emit::Dir)>>,
    /// net -> blocks touched (routing is intra-block only).
    blocks: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Emit one pin's connectivity, recording referenced nets.
///
/// Power-net pins get a power symbol + stub wire immediately. Signal-net pins
/// are DEFERRED into `pending`: the routing pass later draws them as wires or
/// falls back to the classic stub label. No-connects emit immediately.
#[allow(clippy::too_many_arguments)]
fn emit_pin(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    provider: &RealSymbolProvider,
    block_name: &str,
    refdes: &str,
    pin: &str,
    target: &PinTarget,
    power_nets: &std::collections::BTreeSet<String>,
    join_points: &std::collections::BTreeSet<(u64, u64)>,
    pending: &mut PendingNets,
    used_nets: &mut std::collections::BTreeSet<String>,
    power_attach: &mut std::collections::BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    match target {
        PinTarget::Net(net) => {
            used_nets.insert(net.clone());
            if power_nets.contains(net) {
                emit_power_pin(w, env, provider, refdes, pin, net, power_attach)
            } else {
                for (ep, dir) in w.pin_dirs(env, refdes, pin)? {
                    let p = crate::grid::snap_point(ep);
                    // A pin already wired by a placement join needs neither
                    // label nor route: the join wire connects it to the
                    // cluster, whose single label names the net.
                    if join_points.contains(&(p[0].to_bits(), p[1].to_bits())) {
                        continue;
                    }
                    pending
                        .signals
                        .entry(net.clone())
                        .or_default()
                        .push((refdes.to_string(), pin.to_string(), p, dir));
                    pending
                        .blocks
                        .entry(net.clone())
                        .or_default()
                        .insert(block_name.to_string());
                }
                Ok(())
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
        let authored = ap_properties("blk", &Origin::Authored, "rev0");
        assert!(authored.contains(&(AP_BLOCK.to_string(), "blk".to_string())));
        assert!(authored.contains(&(AP_ROLE.to_string(), ROLE_AUTHORED.to_string())));
        assert!(authored.contains(&(AP_LAYOUT_REV.to_string(), "rev0".to_string())));

        let synth = ap_properties(
            "blk",
            &Origin::Synthesized {
                parent: "U1".to_string(),
                role: "decouple".to_string(),
                index: 0,
            },
            "rev0",
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
