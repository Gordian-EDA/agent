//! `sch-engine` — turns a [`circuit_lang::Design`] into a real `.kicad_sch`
//! file (and back), deterministically.
//!
//! This crate is built incrementally per the sch-engine plan. Task 2
//! establishes the deterministic primitives every later stage relies on:
//!
//! - [`ids`] — content-derived (UUIDv5) identifiers so re-emitting the same
//!   `Design` yields byte-identical output (spec §5.1).
//! - [`grid`] — snapping coordinates onto KiCAD's 1.27 mm schematic grid.

pub mod emit;
pub mod grid;
pub mod ids;
pub mod place;

use std::io;

use circuit_lang::model::PinTarget;
use circuit_lang::{Design, PinType, SymbolProvider};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

use crate::emit::SchematicWriter;
use crate::grid::snap_point;

/// Emit a complete `circuit_lang::Design` as a deterministic, ERC-clean
/// `.kicad_sch` document.
///
/// This is **MVP acceptance criterion #1**: the validated bluepill design must
/// pass through here into a schematic KiCAD 10 loads and ERCs with zero errors.
/// The pipeline:
///
/// 1. **Placement.** [`place::place`] assigns every component a grid-snapped
///    sheet position deterministically (same `Design` → same positions).
/// 2. **Symbols.** Each component (across all blocks) becomes a placed symbol
///    via [`SchematicWriter::add_symbol`] at its placed position, angle 0.
/// 3. **Connectivity & no-connects.** For every pin (component-level *and*
///    per-unit): a `PinTarget::Net` pin gets a net-name label at its endpoint
///    ([`SchematicWriter::add_pin_label`]); a `PinTarget::NoConnect` pin — the
///    kernel auto-NCs every unmentioned symbol pin — gets a `(no_connect)`
///    marker at the same endpoint ([`SchematicWriter::add_no_connect`]) so ERC
///    treats the disconnection as intentional.
/// 4. **Power sources.** A net carrying a power-*input* pin (e.g. GND, VBUS, or
///    a regulator's VI) is reported undriven by KiCAD ERC unless a power *source*
///    sits on it. One `power:PWR_FLAG` per such net supplies the missing source,
///    clearing `power_pin_not_driven`. The flags are placed in their own column
///    to the right of the placed design, away from any component.
/// 5. **Assemble.** [`SchematicWriter::finish`] emits the document.
///
/// ## Which nets get a power flag, and why by *etype*
///
/// Plain net-name labels already connect every pin (the kernel guarantees power
/// inputs are bound to nets). The residual ERC error class is "power input not
/// driven by a power output" (`power_pin_not_driven`, ERROR severity) on any net
/// whose pins include a power *input* but no power *output*. A `PWR_FLAG` (a
/// `power_out` graphic with no footprint, excluded from the netlist by its `#FLG`
/// reference) is the minimal, standard fix and supplies the missing source.
///
/// The set of nets to flag is computed by **pin etype**, not by the author's
/// `rails:` declarations. Lint only requires a power-input pin to be on *some*
/// net — not a power-declared one — so an LLM can legitimately put a power-input
/// pin (e.g. a regulator's VI) on a net it forgot to list under `rails:`. Were we
/// to flag only declared power nets, that net would ERC-fail. So we query pin
/// types from the symbol libraries and flag
/// `((power_input_nets ∪ declared_power_used_nets) − driven_nets)`:
///   - every net carrying a power-input pin (the general case), plus
///   - every *declared* power net that's referenced (a declared GND rail with
///     only passive connections still wants a flag),
///   - minus every net already driven by a real power *output* pin. The AMS1117
///     regulator's `VO` pin is a power-output driving 3V3; a flag there would put
///     two power outputs on one net — itself an ERC error (`pin_to_pin`).
///
/// The set is deduplicated and iterated in sorted (`BTreeSet`) order for stable
/// `#FLG` numbering. Declared-but-unused rails, and nets with no power pins at
/// all, get no flag.
///
/// Returns the first I/O error from symbol resolution / endpoint computation, or
/// the assembled schematic text.
pub fn emit_design(env: &KicadEnv, design: &Design) -> io::Result<String> {
    let layout = place::place(design);
    let mut w = SchematicWriter::new();
    let provider = RealSymbolProvider::new(env.clone());

    // 1) Place every component as a symbol, then 2) emit its pin connectivity.
    // While doing so, track three sets, all keyed by net name:
    //   - `used_nets`: every net referenced by any pin label (declared or not).
    //   - `driven_nets`: nets already carrying a real power-*output* pin, which
    //     therefore need no `PWR_FLAG` (and must not get one — a flag would put
    //     two power outputs on the net, itself an ERC `pin_to_pin` error).
    //   - `power_input_nets`: nets carrying at least one power-*input* pin. These
    //     need a power *source*; KiCAD ERC raises `power_pin_not_driven` (ERROR)
    //     on any such net that isn't driven — regardless of whether the author
    //     declared it a rail. Driving by *etype* (not just declared `rails:`) is
    //     what makes the emitter general: an LLM may put a power-input pin on a
    //     net it forgot to list under `rails:`, and lint permits that.
    let mut used_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut driven_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut power_input_nets: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            let at = layout.positions.get(refdes).copied().unwrap_or([0.0, 0.0]);
            let value = comp.value.as_deref().unwrap_or("");
            w.add_symbol(env, &comp.part, refdes, value, at, 0.0)?;

            // Pin types for this part (None if the symbol is unknown — then we
            // simply never mark a net driven/power-input via this component).
            let meta = provider.symbol(&comp.part);

            // Component-level pins.
            for (pin, target) in &comp.pins {
                emit_pin(&mut w, env, refdes, pin, target, &mut used_nets)?;
                record_power_role(meta, pin, target, &mut driven_nets, &mut power_input_nets);
            }
            // Per-unit pins (multi-unit parts). The bluepill has none, but the
            // emitter must handle them for the general case.
            for unit_pins in comp.units.values() {
                for (pin, target) in unit_pins {
                    emit_pin(&mut w, env, refdes, pin, target, &mut used_nets)?;
                    record_power_role(meta, pin, target, &mut driven_nets, &mut power_input_nets);
                }
            }
        }
    }

    // 3) Power sources. A net needs a `PWR_FLAG` iff it must be driven and isn't
    // already driven by a real power-output pin. "Must be driven" is the union of
    //   - every net carrying a power-INPUT pin (by etype — the general case that
    //     covers undeclared rails), and
    //   - every *declared* power net that's actually referenced (a declared GND
    //     rail with only passive connections still wants a flag so ERC sees it as
    //     a real power net rather than a floating label),
    // minus the nets already driven by a power-output pin. Iterating `design.nets`
    // and the components in IndexMap order and collecting into the `BTreeSet`
    // unions above yields a deterministic, deduplicated set; we then sort for
    // stable `#FLG` numbering independent of insertion order.
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

    // Lay flags out in a dedicated column to the right of everything placed, on
    // the grid, well clear of any component so their labels never collide with a
    // real pin. `needs_flag` is a `BTreeSet`, so iteration is sorted by net name
    // — a deterministic, stable order for `#FLG` numbering.
    let power_x = layout
        .positions
        .values()
        .map(|p| p[0])
        .fold(0.0_f64, f64::max)
        + 50.8; // two cells clear of the rightmost component
    for (flag_idx, net) in needs_flag.iter().enumerate() {
        let y = 25.4 + flag_idx as f64 * 12.7;
        let at = snap_point([power_x, y]);
        let refdes = format!("#FLG{:02}", flag_idx + 1);
        w.add_power_flag(env, net, &refdes, at)?;
    }

    Ok(w.finish())
}

/// Classify `pin` on a part with `meta` by its power role and record the net it
/// drives or needs driving on:
///   - a power-*output* pin marks its net as already `driven` (needs no flag, and
///     must not get one — two power outputs on a net is an ERC error), and
///   - a power-*input* pin marks its net as a `power_input` net (one that needs a
///     power source, i.e. a `PWR_FLAG`, unless already driven).
///
/// The pin key from the kernel may be a pin *number* or a pin *name*; resolve it
/// against `meta` the same way the geometry lookup does — number first, then
/// name — and consult the matched pin's `etype`. Covers both component-level and
/// per-unit pins since both call this.
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

/// Emit one pin's connectivity: a net label for a `Net` target, a no-connect
/// marker for a `NoConnect` target. Records referenced net names in `used_nets`.
fn emit_pin(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    refdes: &str,
    pin: &str,
    target: &PinTarget,
    used_nets: &mut std::collections::BTreeSet<String>,
) -> io::Result<()> {
    match target {
        PinTarget::Net(net) => {
            used_nets.insert(net.clone());
            w.add_pin_label(env, refdes, pin, net)
        }
        PinTarget::NoConnect => w.add_no_connect(env, refdes, pin),
    }
}
