//! board-lang: parse and canonically emit the auto-pcb BOARD markup language —
//! the PCB-side analog of `circuit-lang`. Pure (no I/O).
//!
//! A board document is the full, geometry-free design intent for one PCB: its
//! outline + design rules, the parts (footprint + pad→net + per-part placement
//! flags + optional explicit lock), and placement-intent groups. The agent
//! authors it as text; it compiles down to the engine's board draft and
//! round-trips with a `.kicad_pcb`. The kernel ([`BoardDesign`]) is the source
//! of truth — coordinates and copper are the engine's output, never authored.

pub mod canon;
pub mod diag;
pub mod model;
pub mod parse;
mod yaml;

pub use canon::to_canonical_yaml;
pub use diag::{Diagnostic, Diagnostics, Severity, Span};
pub use model::BoardDesign;
pub use parse::parse_str;

pub struct CompileResult {
    /// `Some` only when there were no errors (warnings allowed).
    pub design: Option<BoardDesign>,
    pub diagnostics: Diagnostics,
}

/// Parse + structurally validate a board document.
pub fn compile(src: &str) -> CompileResult {
    let (design, diagnostics) = parse_str(src);
    CompileResult {
        design,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POWER_BUCK: &str = r#"
version: 1
name: power-buck
board:
  layers: 4
  outline: {rect: [44, 32]}
  rules:
    clearance: 0.2
    trace_width: 0.2
    via: [0.6, 0.3]
    net_widths: {SW: 0.8, VOUT: 0.8}
    pours: [{net: GND, layer: bottom}, {net: VIN, layer: in1}]
parts:
  U1: {footprint: 'Package_SO:SOIC-8_3.9x4.9mm_P1.27mm', pads: {1: VIN, 2: SW, 3: GND, 4: FB, 5: VIN, 6: VOUT, 7: GND, 8: VIN}}
  L1: {footprint: 'Inductor_SMD:L_1210_3225Metric', pads: {1: SW, 2: VOUT}}
  C1: {footprint: 'Capacitor_SMD:C_1210_3225Metric', pads: {1: VIN, 2: GND}}
  J1: {footprint: 'Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical', pads: {1: VIN, 2: GND}, edge: true}
  H1: {footprint: 'MountingHole:MountingHole_3.2mm_M3', corner: true}
place:
  groups:
    in_decoupling: {members: [C1], surround: U1}
"#;

    #[test]
    fn parses_a_realistic_board() {
        let r = compile(POWER_BUCK);
        assert!(!r.diagnostics.has_errors(), "diags: {:?}", r.diagnostics);
        let d = r.design.unwrap();
        assert_eq!(d.name.as_deref(), Some("power-buck"));
        assert_eq!(d.board.layers, 4);
        assert_eq!(d.parts.len(), 5);
        assert_eq!(d.parts["U1"].pads["6"], "VOUT");
        assert!(d.parts["J1"].edge);
        assert!(d.parts["H1"].corner);
        assert_eq!(d.board.rules.net_widths["SW"], 0.8);
        assert_eq!(d.board.rules.pours.len(), 2);
        assert_eq!(d.groups["in_decoupling"].surround.as_deref(), Some("U1"));
    }

    #[test]
    fn round_trips_through_canonical_form() {
        let d1 = compile(POWER_BUCK).design.expect("first parse");
        let yaml = to_canonical_yaml(&d1);
        let d2 = compile(&yaml).design.unwrap_or_else(|| {
            panic!("re-parse of canonical form failed:\n{yaml}");
        });
        assert_eq!(d1, d2, "canonical YAML did not round-trip:\n{yaml}");
        // Idempotent: canon(parse(canon(d))) == canon(d).
        assert_eq!(yaml, to_canonical_yaml(&d2));
    }

    #[test]
    fn round_trips_all_outline_shapes() {
        for outline in [
            "{rect: [50, 40]}",
            "{circle: 16}",
            "{polygon: [[0, 0], [20, 0], [10, 20]]}",
        ] {
            let src = format!(
                "version: 1\nboard:\n  layers: 2\n  outline: {outline}\n  rules: {{clearance: 0.2, trace_width: 0.2, via: [0.6, 0.3]}}\nparts:\n  R1: {{footprint: 'X:Y', pads: {{1: A, 2: B}}}}\n"
            );
            let d1 = compile(&src).design.unwrap_or_else(|| panic!("parse {outline}"));
            let d2 = compile(&to_canonical_yaml(&d1)).design.unwrap();
            assert_eq!(d1, d2, "outline {outline} did not round-trip");
        }
    }

    #[test]
    fn keepouts_round_trip() {
        let src = "version: 1\nboard:\n  layers: 2\n  outline: {rect: [40, 30]}\n  rules: {clearance: 0.2, trace_width: 0.2, via: [0.6, 0.3]}\nparts:\n  U1: {footprint: 'X:Y', pads: {1: A}}\nkeepouts:\n  - {rect: [5, 5, 15, 12], layers: [top, bottom]}\n";
        let d1 = compile(src).design.expect("parse keepout");
        assert_eq!(d1.keepouts.len(), 1);
        assert_eq!(d1.keepouts[0].rect, [5.0, 5.0, 15.0, 12.0]);
        assert_eq!(d1.keepouts[0].layers, vec!["top".to_string(), "bottom".to_string()]);
        let d2 = compile(&to_canonical_yaml(&d1)).design.unwrap();
        assert_eq!(d1, d2, "keepout did not round-trip");
    }

    #[test]
    fn missing_version_is_an_error() {
        let r = compile("board:\n  layers: 2\nparts: {}\n");
        assert!(r.diagnostics.has_errors());
        assert!(r.diagnostics.0.iter().any(|d| d.code == "version"));
    }

    #[test]
    fn unknown_key_suggests_the_right_one() {
        let r = compile(
            "version: 1\nboard:\n  layers: 2\n  outline: {rect: [10, 10]}\n  rules: {clearnce: 0.2}\nparts: {}\n",
        );
        assert!(r.diagnostics.has_errors());
        let d = r.diagnostics.0.iter().find(|d| d.code == "unknown-key").expect("unknown-key");
        assert_eq!(d.suggestion.as_deref(), Some("clearance"));
    }

    #[test]
    fn bad_layer_count_rejected() {
        let r = compile("version: 1\nboard:\n  layers: 3\n  outline: {rect: [10, 10]}\nparts: {}\n");
        assert!(r.diagnostics.0.iter().any(|d| d.code == "layers"));
    }

    #[test]
    fn duplicate_part_rejected() {
        let src = "version: 1\nboard:\n  layers: 2\n  outline: {rect: [10, 10]}\nparts:\n  R1: {footprint: 'A:B'}\n  R1: {footprint: 'C:D'}\n";
        let r = compile(src);
        assert!(r.diagnostics.has_errors());
    }
}
