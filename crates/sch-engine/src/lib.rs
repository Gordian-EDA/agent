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
/// 4. **Power sources.** Each net with `power == true` carries only power-input
///    pins driven by labels, which KiCAD ERC reports as undriven. One
///    `power:PWR_FLAG` per power net supplies the missing power *source*,
///    clearing `power_pin_not_driven`. The flags are placed in their own column
///    to the right of the placed design, away from any component.
/// 5. **Assemble.** [`SchematicWriter::finish`] emits the document.
///
/// ## Why power flags, and why only on *undriven* power nets
///
/// Plain net-name labels already connect every pin (the kernel guarantees power
/// inputs are bound to nets). The residual ERC error class on the bluepill is
/// "power input not driven by a power output" on power rails whose only pins are
/// power *inputs* (e.g. GND, VBUS). A `PWR_FLAG` (a `power_out` graphic with no
/// footprint, excluded from the netlist by its `#FLG` reference) is the minimal,
/// standard fix and supplies the missing source.
///
/// Crucially, a flag is added **only** to a power net that has no real power
/// *output* pin on it. The AMS1117 regulator's `VO` pin is itself a Power-output
/// driving 3V3; a flag there would put two power outputs on one net, which is
/// *also* an ERC error (`pin_to_pin`). So we query pin types from the symbol
/// libraries, collect the nets already driven by a power-output pin, and flag
/// every *other* referenced power net. Declared-but-unused rails get no flag.
///
/// Returns the first I/O error from symbol resolution / endpoint computation, or
/// the assembled schematic text.
pub fn emit_design(env: &KicadEnv, design: &Design) -> io::Result<String> {
    let layout = place::place(design);
    let mut w = SchematicWriter::new();
    let provider = RealSymbolProvider::new(env.clone());

    // 1) Place every component as a symbol, then 2) emit its pin connectivity.
    // While doing so, track (a) which nets are referenced at all, and (b) which
    // nets already carry a real power-output pin (so they need no PWR_FLAG).
    let mut used_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut driven_nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            let at = layout.positions.get(refdes).copied().unwrap_or([0.0, 0.0]);
            let value = comp.value.as_deref().unwrap_or("");
            w.add_symbol(env, &comp.part, refdes, value, at, 0.0)?;

            // Pin types for this part (None if the symbol is unknown — then we
            // simply never mark a net driven via this component).
            let meta = provider.symbol(&comp.part);

            // Component-level pins.
            for (pin, target) in &comp.pins {
                emit_pin(&mut w, env, refdes, pin, target, &mut used_nets)?;
                record_driver(meta, pin, target, &mut driven_nets);
            }
            // Per-unit pins (multi-unit parts). The bluepill has none, but the
            // emitter must handle them for the general case.
            for unit_pins in comp.units.values() {
                for (pin, target) in unit_pins {
                    emit_pin(&mut w, env, refdes, pin, target, &mut used_nets)?;
                    record_driver(meta, pin, target, &mut driven_nets);
                }
            }
        }
    }

    // 3) Power sources. Flag each referenced power net that is not already driven
    // by a real power-output pin. Lay flags out in a dedicated column to the
    // right of everything placed, on the grid, well clear of any component so
    // their labels never collide with a real pin.
    let power_x = layout
        .positions
        .values()
        .map(|p| p[0])
        .fold(0.0_f64, f64::max)
        + 50.8; // two cells clear of the rightmost component
    let mut flag_idx = 0u32;
    for (net, attrs) in &design.nets {
        let needs_flag =
            attrs.power && used_nets.contains(net.as_str()) && !driven_nets.contains(net.as_str());
        if needs_flag {
            let y = 25.4 + flag_idx as f64 * 12.7;
            let at = snap_point([power_x, y]);
            let refdes = format!("#FLG{:02}", flag_idx + 1);
            w.add_power_flag(env, net, &refdes, at)?;
            flag_idx += 1;
        }
    }

    Ok(w.finish())
}

/// If `pin` on a part with `meta` is a power-*output* pin bound to a net, record
/// that net as already driven (so it needs no `PWR_FLAG`).
///
/// The pin key from the kernel may be a pin *number* or a pin *name*; resolve it
/// against `meta` the same way the geometry lookup does — number first, then
/// name — and consult the matched pin's `etype`.
fn record_driver(
    meta: Option<&circuit_lang::SymbolMeta>,
    pin: &str,
    target: &PinTarget,
    driven_nets: &mut std::collections::BTreeSet<String>,
) {
    let PinTarget::Net(net) = target else { return };
    let Some(meta) = meta else { return };
    let matched = meta
        .pins
        .iter()
        .find(|p| p.number == pin)
        .or_else(|| meta.pins.iter().find(|p| p.name == pin));
    if let Some(pm) = matched
        && pm.etype == PinType::PowerOutput
    {
        driven_nets.insert(net.clone());
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
