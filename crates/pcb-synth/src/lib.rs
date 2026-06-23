//! Synthesize a KiCAD `.kicad_pcb` from a placed BoardDraft: footprints at their
//! placed positions, copper planes/zones/pours, a tightened outline, and silk.

pub mod placefp;
pub mod synth;
