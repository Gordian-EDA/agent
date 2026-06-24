//! Synthesize a KiCAD `.kicad_pcb` from a placed [`synth::BoardModel`]: footprints
//! at their placed positions, copper planes/zones/pours, a tightened outline, and
//! silk.
//!
//! Synthesis is an **engine SDK** — a [`synth::Synthesizer`] consumes one
//! self-contained [`synth::BoardModel`] and returns the emitted text, so the
//! KiCAD-9 emitter ([`synth::KicadV9Synth`]) is one swappable implementation. A
//! third party targets a different EDA format by implementing the trait for the
//! same model. The paren-surgery kernel ([`sexpr`]) and the zone/pour emit
//! ([`zone`]) are dedicated, reusable modules.

mod ids;
pub mod placefp;
pub mod sexpr;
pub mod synth;
pub mod zone;
