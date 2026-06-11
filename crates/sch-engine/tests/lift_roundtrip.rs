//! Task 8: emit → lift round-trips the *connectivity and component structure*
//! of the validated bluepill design.
//!
//! Compiling the bluepill fixture yields a kernel `Design`; emitting it to a
//! `.kicad_sch` and lifting it back (via the `kicad-cli` netlist for
//! connectivity + the hidden `ap_*` tags for semantics) reconstructs a kernel
//! model with the same blocks, the same components (refdes → part/value/origin),
//! and the same **net partition** (which physical pins share a net).
//!
//! ## Why not byte-exact `canon_in == canon_out`?
//!
//! Exact canonical equality is blocked by four representable differences that the
//! emitted schematic genuinely does not encode — every one invisible to
//! connectivity, all documented in `lift.rs`:
//!
//! 1. **Pin-key spelling.** A source keys a pin by *name* (`VDD: 3V3`) or
//!    *number* (`"48": VCAP1`); the kernel preserves the author's choice. KiCAD's
//!    netlist node carries only the pin **number**, so lift always keys by
//!    number. *Which physical pin joins which net is identical* — only the key's
//!    spelling differs. The comparison below resolves every key to its physical
//!    pin number(s) via the symbol library, so it sees through this.
//! 2. **Design `name:`** — not written to the schematic in a recoverable form.
//! 3. **Block `layout:` hints** (`edge`/`near`) — placement intent, not encoded.
//! 4. **Net `power:`/`class:`** — author declarations (`rails:` / the `nets:`
//!    block); emit writes neither a rails list nor net classes, so they cannot
//!    be recovered.
//!
//! What the schematic *does* carry — every component with its part/value/origin
//! and the full connectivity graph — round-trips exactly, which is the property
//! the React data loop depends on. The structural assertion proves it; the
//! residuals above are author-intent metadata, not connectivity.

use std::collections::BTreeSet;

use circuit_lang::SymbolProvider;
use circuit_lang::model::{Design, Origin, PinTarget};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

/// A component's identity for structural comparison: part, value, and origin.
/// Origin is rendered to a stable string (the enum is not `Ord`). (Pin
/// connectivity is compared separately via the net partition.)
type CompKey = (String, Option<String>, String);

/// Render an [`Origin`] to a stable, comparable string.
fn origin_key(o: &Origin) -> String {
    match o {
        Origin::Authored => "authored".to_string(),
        Origin::Synthesized {
            parent,
            role,
            index,
        } => format!("synth:{parent}:{role}:{index}"),
    }
}

/// The set of physical-pin endpoints `(refdes, pin_number)` sharing one net,
/// after resolving every kernel pin key to the symbol's physical pin number(s).
type NetGroup = BTreeSet<(String, String)>;

/// Resolve a component pin key to the physical pin number(s) it covers.
///
/// A key may be a pin number (covers that one pin) or a pin name; a name can be
/// *stacked* across several physical pins (e.g. the USB-C connector's two `GND`
/// pins), so it covers all of them. Mirrors the desugar's coverage rule. Unknown
/// symbols (no metadata) fall back to the key verbatim.
fn physical_pins(provider: &dyn SymbolProvider, part: &str, key: &str) -> Vec<String> {
    let Some(meta) = provider.symbol(part) else {
        return vec![key.to_string()];
    };
    let by_number: Vec<String> = meta
        .pins
        .iter()
        .filter(|p| p.number == key)
        .map(|p| p.number.clone())
        .collect();
    if !by_number.is_empty() {
        return by_number;
    }
    let by_name: Vec<String> = meta
        .pins
        .iter()
        .filter(|p| p.name == key)
        .map(|p| p.number.clone())
        .collect();
    if by_name.is_empty() {
        vec![key.to_string()]
    } else {
        by_name
    }
}

/// `block name -> (refdes -> CompKey)`: components and their identity per block.
fn component_structure(d: &Design) -> BTreeSet<(String, String, CompKey)> {
    let mut out = BTreeSet::new();
    for (bname, block) in &d.blocks {
        for (refdes, c) in &block.components {
            out.insert((
                bname.clone(),
                refdes.clone(),
                (c.part.clone(), c.value.clone(), origin_key(&c.origin)),
            ));
        }
    }
    out
}

/// The net partition: the set of endpoint-groups, one per net, where each
/// endpoint is a `(refdes, physical_pin_number)`. Net *names* are abstracted
/// away (only the grouping matters), and `NoConnect` pins are excluded — those
/// are re-derived by `compile`, not stored.
fn net_partition(d: &Design, provider: &dyn SymbolProvider) -> BTreeSet<NetGroup> {
    // net name -> endpoints
    let mut by_net: std::collections::BTreeMap<String, NetGroup> = Default::default();
    for block in d.blocks.values() {
        for (refdes, c) in &block.components {
            for (pin, target) in &c.pins {
                let PinTarget::Net(net) = target else {
                    continue;
                };
                for num in physical_pins(provider, &c.part, pin) {
                    by_net
                        .entry(net.clone())
                        .or_default()
                        .insert((refdes.clone(), num));
                }
            }
        }
    }
    by_net.into_values().collect()
}

#[test]
fn emit_then_lift_roundtrips_connectivity_and_structure() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(KicadEnv::detect().unwrap());
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let design = circuit_lang::compile(src, &provider).design.unwrap();

    // emit -> temp .kicad_sch
    let text = sch_engine::emit_design(&env, &design).unwrap().sch;
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    // lift -> canonical YAML -> recompile to a kernel Design
    let lifted_yaml = sch_engine::lift::lift(&env, tmp.path()).unwrap();
    let lifted = circuit_lang::compile(&lifted_yaml, &provider)
        .design
        .expect("lifted YAML must compile");

    // Same components in the same blocks, with the same part/value/origin.
    // (Origin equality proves the synthesized decouple caps were recovered as
    // Synthesized and re-sugared — `to_canonical_yaml` collapsed them to the
    // parent's `decouple:` line, which `compile` re-expanded identically.)
    assert_eq!(
        component_structure(&design),
        component_structure(&lifted),
        "every component must round-trip into the same block with the same \
         part/value/origin"
    );

    // Same connectivity: identical net partition over physical pins. This is the
    // property the React data loop depends on — pin-key spelling and net names
    // are abstracted, the wiring is not.
    assert_eq!(
        net_partition(&design, &provider),
        net_partition(&lifted, &provider),
        "the net partition (which physical pins share a net) must round-trip \
         exactly"
    );
}
