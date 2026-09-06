//! Three placements without blocks, then blocks made and tiled: `blocks_demo <out.kicad_sch>`.
use sch_check::place_parts::PlacePartsInput;
use sch_floorplan::{blocks, live};
fn call(v: serde_json::Value) -> PlacePartsInput { serde_json::from_value(v).unwrap() }
fn main() {
    let env = kicad::KicadInstallation::detect().expect("kicad");
    let mut doc = live::blank_sheet().unwrap();
    for input in [
        call(serde_json::json!({"parts": [
            {"ref": "J1", "part": "Connector:Barrel_Jack", "pins": {"1": "+12V", "2": "GND"}},
            {"ref": "C1", "part": "Device:C", "value": "10uF", "pins": {"1": "+12V", "2": "GND"}},
            {"ref": "U1", "part": "Regulator_Linear:L7805", "value": "L7805", "pins": {"1": "+12V", "2": "GND", "3": "+5V"}},
            {"ref": "C2", "part": "Device:C", "value": "10uF", "pins": {"1": "+5V", "2": "GND"}}],
          "block": "a", "layout": {"a": {"row": [{"part": "J1"}, {"col": [{"part": "C1"}]}, {"part": "U1"}, {"col": [{"part": "C2"}]}]}}})),
        call(serde_json::json!({"parts": [
            {"ref": "U2", "part": "Timer:NE555P", "value": "NE555", "pins": {"VCC": "+5V", "GND": "GND", "~{RST}": "+5V", "CONT": "CV", "TRIG": "TRIG", "THRES": "TRIG", "DISCH": "DIS", "OUT": "OUT"}},
            {"ref": "C4", "part": "Device:C", "value": "10nF", "pins": {"1": "CV", "2": "GND"}},
            {"ref": "R1", "part": "Device:R", "value": "1K", "pins": {"1": "+5V", "2": "DIS"}},
            {"ref": "R2", "part": "Device:R", "value": "680K", "pins": {"1": "DIS", "2": "TRIG"}},
            {"ref": "C3", "part": "Device:C_Polarized", "value": "1uF", "pins": {"1": "TRIG", "2": "GND"}}],
          "block": "b", "layout": {"b": {"row": [{"col": [{"part": "C4"}]}, {"part": "U2"}, {"col": [{"part": "R1"}, {"part": "R2"}, {"part": "C3"}]}]}}})),
        call(serde_json::json!({"parts": [
            {"ref": "R3", "part": "Device:R", "value": "1K", "pins": {"1": "OUT", "2": "LED_A"}},
            {"ref": "D1", "part": "Device:LED", "value": "LED", "pins": {"2": "LED_A", "1": "GND"}},
            {"ref": "R4", "part": "Device:R", "value": "1K", "pins": {"1": "+5V", "2": "PWR_A"}},
            {"ref": "D2", "part": "Device:LED", "value": "PWR", "pins": {"2": "PWR_A", "1": "GND"}}],
          "block": "c", "layout": {"c": {"row": [{"col": [{"part": "R3"}, {"part": "D1"}]}, {"col": [{"part": "R4"}, {"part": "D2"}]}]}}})),
    ] {
        let r = live::place_parts(&env, &mut doc, &input).unwrap();
        assert!(r.committed, "{:?}", r.mismatch);
    }
    let out = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    doc.write(&out.with_extension("placed.kicad_sch")).unwrap();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    blocks::create_block(&mut doc, "POWER", &s(&["J1", "C1", "U1", "C2"]), Some("POWER ENTRY & 5V")).unwrap();
    blocks::create_block(&mut doc, "TIMER", &s(&["U2", "C4", "R1", "R2", "C3"]), Some("NE555 ASTABLE")).unwrap();
    blocks::create_block(&mut doc, "LEDS", &s(&["R3", "D1", "R4", "D2"]), Some("INDICATORS")).unwrap();
    let rows = vec![vec!["POWER".to_string(), "TIMER".to_string()], vec!["LEDS".to_string()]];
    let r = blocks::arrange_blocks(&mut doc, &rows).unwrap();
    eprintln!("arranged {:?} page {:?}", r.moved, r.page);
    doc.write(&out).unwrap();
}
