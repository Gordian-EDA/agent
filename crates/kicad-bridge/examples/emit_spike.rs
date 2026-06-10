//! THROWAWAY INVESTIGATION SPIKE — schematic emission de-risking.
//!
//! Run with:  cargo run -p kicad-bridge --example emit_spike
//!
//! This is NOT production code. It exists to answer three questions for Plan 3:
//!   Q1 — Can kiutils_kicad round-trip rc_pair.kicad_sch losslessly and still ERC?
//!   Q2 — Can we programmatically build a NEW schematic KiCAD accepts (one R,
//!        a value, a ref, and connectivity that ERC tolerates)?
//!   Q3 — Is kiutils_kicad sufficient, or do we need a different emission path?
//!
//! Findings are printed to stdout as it runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use kiutils_kicad::{SchematicFile, SymbolLibFile};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rc_pair.kicad_sch")
}

/// Run `kicad-cli sch erc` and return (exit_code, total_violations, error_count,
/// warning_count). Parses the JSON report directly so we don't depend on the
/// crate's ErcReport shape here.
fn erc(path: &Path) -> (i32, usize, usize, usize) {
    let out = std::env::temp_dir().join(format!(
        "emit_spike_erc_{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let status = Command::new("kicad-cli")
        .args(["sch", "erc", "--format", "json"])
        .arg("--output")
        .arg(&out)
        .args(["--severity-all", "--exit-code-violations"])
        .arg(path)
        .output()
        .expect("run kicad-cli");
    let code = status.status.code().unwrap_or(-1);
    let json = std::fs::read_to_string(&out).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
    let mut total = 0;
    let mut errors = 0;
    let mut warnings = 0;
    if let Some(sheets) = v.get("sheets").and_then(|s| s.as_array()) {
        for sheet in sheets {
            if let Some(vios) = sheet.get("violations").and_then(|x| x.as_array()) {
                for vio in vios {
                    total += 1;
                    match vio.get("severity").and_then(|s| s.as_str()) {
                        Some("error") => errors += 1,
                        Some("warning") => warnings += 1,
                        _ => {}
                    }
                }
            }
        }
    }
    if !json.is_empty() {
        // Print the violation kinds for visibility.
        if let Some(sheets) = v.get("sheets").and_then(|s| s.as_array()) {
            for sheet in sheets {
                if let Some(vios) = sheet.get("violations").and_then(|x| x.as_array()) {
                    for vio in vios {
                        eprintln!(
                            "      - [{}] {}: {}",
                            vio.get("severity").and_then(|s| s.as_str()).unwrap_or("?"),
                            vio.get("type").and_then(|s| s.as_str()).unwrap_or("?"),
                            vio.get("description")
                                .and_then(|s| s.as_str())
                                .unwrap_or(""),
                        );
                    }
                }
            }
        }
    } else {
        eprintln!("      (no ERC report written — schematic likely failed to LOAD)");
    }
    let _ = std::fs::remove_file(&out);
    (code, total, errors, warnings)
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "emit_spike_{name}_{}.kicad_sch",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn main() {
    println!("==================================================================");
    println!(" Q1 — ROUND-TRIP WRITE via kiutils_kicad");
    println!("==================================================================");
    q1_roundtrip();

    println!();
    println!("==================================================================");
    println!(" Q2 — PROGRAMMATIC CONSTRUCTION");
    println!("==================================================================");
    q2_construct();
}

fn q1_roundtrip() {
    let src = fixture();
    println!("Reading fixture: {}", src.display());

    // EXACT working kiutils API:
    let doc = SchematicFile::read(&src).expect("SchematicFile::read");
    println!(
        "  parsed: version={:?} symbol_count={} wire_count={} label_count={} lib_symbol_count={}",
        doc.ast().version,
        doc.ast().symbol_count,
        doc.ast().wire_count,
        doc.ast().label_count,
        doc.ast().lib_symbol_count
    );
    println!("  diagnostics: {}", doc.diagnostics().len());

    let out = tmp("roundtrip");
    // Lossless write (default). This is byte-identical to the input.
    doc.write(&out).expect("doc.write (lossless)");

    let orig = std::fs::read_to_string(&src).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();
    println!("  lossless byte-identical to input: {}", orig == written);

    println!("  ERC on ORIGINAL:");
    let (_c0, t0, e0, w0) = erc(&src);
    println!("    -> total={t0} errors={e0} warnings={w0}");
    println!("  ERC on ROUND-TRIPPED output:");
    let (_c1, t1, e1, w1) = erc(&out);
    println!("    -> total={t1} errors={e1} warnings={w1}");
    println!(
        "  Q1 RESULT: round-trip {} (same violation counts: {})",
        if (t1, e1, w1) == (t0, e0, w0) {
            "PRESERVED"
        } else {
            "DIVERGED"
        },
        (t1, e1, w1) == (t0, e0, w0)
    );
    let _ = std::fs::remove_file(&out);
}

fn q2_construct() {
    // ---- Step A: pull the Device:R symbol DEFINITION out of Device.kicad_sym
    //      using kiutils, so we can embed it in (lib_symbols).
    let lib_path = PathBuf::from("/usr/share/kicad/symbols/Device.kicad_sym");
    println!("Loading symbol library: {}", lib_path.display());
    let symlib = SymbolLibFile::read(&lib_path).expect("read Device.kicad_sym");

    let r_sym = symlib
        .ast()
        .symbols
        .iter()
        .find(|s| s.name.as_deref() == Some("R"))
        .expect("Device:R present");

    // Report pin geometry that kiutils exposes (symlib.rs currently DROPS this).
    println!("  Device:R pins (kiutils SymPin geometry):");
    for unit in &r_sym.units {
        for p in &unit.pins {
            println!(
                "    pin number={:?} name={:?} at={:?} angle={:?} length={:?} etype={:?}",
                p.number, p.name, p.at, p.angle, p.length, p.electrical_type
            );
        }
    }
    // Direct pins (some symbols hold pins directly, not in units).
    for p in &r_sym.pins {
        println!(
            "    (direct) pin number={:?} at={:?} angle={:?} length={:?}",
            p.number, p.at, p.angle, p.length
        );
    }

    // ---- Step B: extract the EXACT (symbol "Device:R" ...) s-expr text from the
    //      .kicad_sym so we can splice it into the schematic's (lib_symbols).
    //      kiutils gives no "render one symbol" API, so we grab it from the CST raw
    //      bytes via the symbol's span... but spans aren't exposed on Symbol.
    //      Easiest robust route: take it from our OWN fixture, which already embeds
    //      a correct (symbol "Device:R" ...) block. We read the fixture text and
    //      lift the Device:R lib_symbol definition out of it. For Plan 3 the real
    //      source is the .kicad_sym; see Q3 notes.
    let fixture_text = std::fs::read_to_string(fixture()).unwrap();
    let r_lib_def = extract_balanced_block(&fixture_text, "(symbol \"Device:R\"")
        .expect("extract Device:R lib_symbol from fixture");
    println!(
        "  extracted Device:R lib_symbol definition: {} chars",
        r_lib_def.len()
    );

    // ---- Step C: compute pin endpoint coordinates for the placed instance.
    // Device:R local pin tips: pin1 at (0, 3.81), pin2 at (0, -3.81) in SYMBOL
    // coords. KiCAD schematic Y grows DOWNWARD, symbol Y grows UPWARD, so the
    // schematic Y of a pin = instance_y - local_pin_y (for angle 0, no mirror).
    // Place instance on the 1.27 mm grid so endpoints are on-grid too.
    let inst_x = 127.0_f64; // 100 * 1.27
    let inst_y = 63.5_f64; // 50 * 1.27
    let pin1 = [inst_x, inst_y - 3.81]; // (127.0, 59.69)
    let pin2 = [inst_x, inst_y + 3.81]; // (127.0, 67.31)
    println!("  instance at ({inst_x},{inst_y}); pin1 endpoint {pin1:?}; pin2 endpoint {pin2:?}");

    // ---- Step D: assemble the full .kicad_sch as text. Connectivity strategy:
    // a local LABEL placed exactly at each pin endpoint. A label on a pin makes a
    // named net; ERC's "isolated_pin_label" is only a WARNING and only fires when
    // the same label appears once AND nothing else shares it. We give both pins
    // DISTINCT labels (NET1, NET2): each is single-pin, which KiCAD treats the
    // same way the fixture's VIN/VOUT do.
    let sch = build_schematic(&r_lib_def, inst_x, inst_y, pin1, pin2);
    let out = tmp("built");
    std::fs::write(&out, &sch).unwrap();
    println!("  wrote built schematic: {}", out.display());

    // ---- Step E: prove it parses with kiutils (well-formed + typed-loadable).
    match SchematicFile::read(&out) {
        Ok(doc) => println!(
            "  kiutils re-read OK: symbol_count={} label_count={} lib_symbol_count={} diagnostics={}",
            doc.ast().symbol_count,
            doc.ast().label_count,
            doc.ast().lib_symbol_count,
            doc.diagnostics().len()
        ),
        Err(e) => println!("  kiutils re-read FAILED: {e}"),
    }

    // ---- Step F: the real gate — does KiCAD load it and what does ERC say?
    println!("  ERC on BUILT schematic:");
    let (code, total, errors, warnings) = erc(&out);
    println!("    -> exit={code} total={total} errors={errors} warnings={warnings}");
    println!(
        "  Q2 RESULT: KiCAD {} the built schematic; {} errors, {} warnings",
        if code >= 0 && (total > 0 || code == 0) {
            "LOADED"
        } else {
            "did NOT load"
        },
        errors,
        warnings
    );

    // Also write a copy to /tmp with a stable name for manual inspection.
    let stable = std::env::temp_dir().join("emit_spike_built_LATEST.kicad_sch");
    let _ = std::fs::write(&stable, &sch);
    println!("  (stable copy for inspection: {})", stable.display());
    let _ = std::fs::remove_file(&out);

    // ---- Step G: a fully-clean variant — wire the two R pins together into ONE
    // net (a self-loop). This proves a real multi-endpoint net and clears the
    // single-pin-label warning, leaving ZERO ERC violations. We route the wire
    // out from each pin and join them, all on the 1.27 mm grid.
    println!();
    println!("  --- variant: wire both pins into one net (expect 0 violations) ---");
    let sch2 = build_schematic_wired(&r_lib_def, inst_x, inst_y, pin1, pin2);
    let out2 = tmp("wired");
    std::fs::write(&out2, &sch2).unwrap();
    match SchematicFile::read(&out2) {
        Ok(d) => println!(
            "  kiutils re-read OK: wire_count={} symbol_count={}",
            d.ast().wire_count,
            d.ast().symbol_count
        ),
        Err(e) => println!("  kiutils re-read FAILED: {e}"),
    }
    let (c2, t2, e2, w2) = erc(&out2);
    println!("    ERC -> exit={c2} total={t2} errors={e2} warnings={w2}");

    // Confirm the net is REAL via the netlist exporter.
    let nl = std::env::temp_dir().join("emit_spike_wired_netlist.xml");
    let netlist_ok = Command::new("kicad-cli")
        .args(["sch", "export", "netlist", "--format", "kicadxml"])
        .arg("--output")
        .arg(&nl)
        .arg(&out2)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if netlist_ok {
        let xml = std::fs::read_to_string(&nl).unwrap_or_default();
        let nets = xml.matches("<net ").count();
        let nodes = xml.matches("<node ").count();
        println!(
            "    netlist: {nets} net(s), {nodes} node(s) — both R pins on one net = {}",
            nodes == 2 && nets == 1
        );
    }
    let stable2 = std::env::temp_dir().join("emit_spike_wired_LATEST.kicad_sch");
    let _ = std::fs::write(&stable2, &sch2);
    println!("  (stable copy: {})", stable2.display());
    let _ = std::fs::remove_file(&out2);
    let _ = std::fs::remove_file(&nl);
}

/// Variant of `build_schematic` that wires the two R pins together into a
/// single net (an L-shaped 3-segment wire), instead of using labels. Produces a
/// schematic with ZERO ERC violations.
fn build_schematic_wired(
    r_lib_def: &str,
    inst_x: f64,
    inst_y: f64,
    pin1: [f64; 2],
    pin2: [f64; 2],
) -> String {
    let sch_uuid = "aaaaaaaa-0000-4000-8000-000000000002";
    let r_uuid = "bbbbbbbb-0000-4000-8000-000000000002";
    // Route to the right of the symbol on the grid (132.08 = 104 * 1.27).
    let bus_x = 132.08_f64;
    format!(
        r#"(kicad_sch
	(version 20250114)
	(generator "auto-pcb-spike")
	(generator_version "0.1")
	(uuid "{sch_uuid}")
	(paper "A4")
	(lib_symbols
		{r_lib_def}
	)
	(wire (pts (xy {p1x} {p1y}) (xy {bus_x} {p1y})) (stroke (width 0) (type default)) (uuid "eeeeeeee-0000-4000-8000-000000000001"))
	(wire (pts (xy {bus_x} {p1y}) (xy {bus_x} {p2y})) (stroke (width 0) (type default)) (uuid "eeeeeeee-0000-4000-8000-000000000002"))
	(wire (pts (xy {bus_x} {p2y}) (xy {p2x} {p2y})) (stroke (width 0) (type default)) (uuid "eeeeeeee-0000-4000-8000-000000000003"))
	(symbol
		(lib_id "Device:R")
		(at {inst_x} {inst_y} 0)
		(unit 1)
		(exclude_from_sim no)
		(in_bom yes)
		(on_board yes)
		(dnp no)
		(uuid "{r_uuid}")
		(property "Reference" "R1"
			(at {ref_x} {ref_y} 0)
			(effects (font (size 1.27 1.27)) (justify left))
		)
		(property "Value" "1k"
			(at {val_x} {val_y} 0)
			(effects (font (size 1.27 1.27)) (justify left))
		)
		(property "Footprint" ""
			(at {inst_x} {inst_y} 0)
			(effects (font (size 1.27 1.27)) (hide yes))
		)
		(pin "1" (uuid "ffffffff-0000-4000-8000-000000000001"))
		(pin "2" (uuid "ffffffff-0000-4000-8000-000000000002"))
		(instances
			(project "spike"
				(path "/{sch_uuid}"
					(reference "R1")
					(unit 1)
				)
			)
		)
	)
	(sheet_instances
		(path "/"
			(page "1")
		)
	)
)
"#,
        p1x = pin1[0],
        p1y = pin1[1],
        p2x = pin2[0],
        p2y = pin2[1],
        ref_x = inst_x + 2.54,
        ref_y = inst_y - 1.27,
        val_x = inst_x + 2.54,
        val_y = inst_y + 1.27,
    )
}

/// Extract a parenthesis-balanced block starting at the first occurrence of
/// `start_marker`, returning the full `(...)` including the outer parens.
fn extract_balanced_block(text: &str, start_marker: &str) -> Option<String> {
    let start = text.find(start_marker)?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = start;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text[start..=i].to_string());
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn build_schematic(
    r_lib_def: &str,
    inst_x: f64,
    inst_y: f64,
    pin1: [f64; 2],
    pin2: [f64; 2],
) -> String {
    let sch_uuid = "aaaaaaaa-0000-4000-8000-000000000001";
    let r_uuid = "bbbbbbbb-0000-4000-8000-000000000001";
    format!(
        r#"(kicad_sch
	(version 20250114)
	(generator "auto-pcb-spike")
	(generator_version "0.1")
	(uuid "{sch_uuid}")
	(paper "A4")
	(lib_symbols
		{r_lib_def}
	)
	(label "NET1"
		(at {p1x} {p1y} 0)
		(effects (font (size 1.27 1.27)) (justify left bottom))
		(uuid "cccccccc-0000-4000-8000-000000000001")
	)
	(label "NET2"
		(at {p2x} {p2y} 0)
		(effects (font (size 1.27 1.27)) (justify left bottom))
		(uuid "cccccccc-0000-4000-8000-000000000002")
	)
	(symbol
		(lib_id "Device:R")
		(at {inst_x} {inst_y} 0)
		(unit 1)
		(exclude_from_sim no)
		(in_bom yes)
		(on_board yes)
		(dnp no)
		(uuid "{r_uuid}")
		(property "Reference" "R1"
			(at {ref_x} {ref_y} 0)
			(effects (font (size 1.27 1.27)) (justify left))
		)
		(property "Value" "1k"
			(at {val_x} {val_y} 0)
			(effects (font (size 1.27 1.27)) (justify left))
		)
		(property "Footprint" ""
			(at {inst_x} {inst_y} 0)
			(effects (font (size 1.27 1.27)) (hide yes))
		)
		(pin "1" (uuid "dddddddd-0000-4000-8000-000000000001"))
		(pin "2" (uuid "dddddddd-0000-4000-8000-000000000002"))
		(instances
			(project "spike"
				(path "/{sch_uuid}"
					(reference "R1")
					(unit 1)
				)
			)
		)
	)
	(sheet_instances
		(path "/"
			(page "1")
		)
	)
)
"#,
        p1x = pin1[0],
        p1y = pin1[1],
        p2x = pin2[0],
        p2y = pin2[1],
        ref_x = inst_x + 2.54,
        ref_y = inst_y - 1.27,
        val_x = inst_x + 2.54,
        val_y = inst_y + 1.27,
    )
}
