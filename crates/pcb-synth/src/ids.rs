//! Content-derived identifiers for synthesized board elements, so a given board
//! re-emits byte-identically.

/// Fixed namespace UUID for synthesized board identifiers (distinct from the
/// copper namespace in [`kicad_sexpr::pcb`]). Content-derived so a given board
/// re-emits byte-identically.
const SYNTH_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x7b2e_91c0_4d3a_5e6f_8a9b_0c1d_2e3f_4a5b);

/// Content-derived UUID for a synthesized board element.
pub fn synth_uuid(key: &str) -> String {
    geom::hash::uuid_v5(SYNTH_NAMESPACE, key.as_bytes())
}
