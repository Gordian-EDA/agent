//! Wrappers around the `kicad-cli` subcommands the agent invokes after
//! writing a schematic: ERC (Electrical Rules Check) and netlist export.
//!
//! `kicad-cli sch erc` writes a JSON report and, with `--exit-code-violations`,
//! returns a nonzero exit code *when violations exist*. That nonzero exit is a
//! normal outcome, not a failure: the JSON report is still written. We therefore
//! key success on whether the report file parses, not on the process exit code,
//! reserving the `Err` path for genuine execution failures (binary missing,
//! schematic failed to load) where no report is produced.
//!
//! `kicad-cli sch export netlist --format kicadxml` writes the authoritative
//! connectivity for a schematic — every component and every net with its pin
//! nodes — which Plan 3's "lift" reads to recover the kernel design from a
//! `.kicad_sch`. We parse the `kicadxml` output (see [`Netlist`]).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;

/// A handle to `kicad-cli` for running schematic-level checks.
pub struct KicadCli {
    cli_path: PathBuf,
}

impl KicadCli {
    /// Build a CLI wrapper from a discovered environment.
    pub fn new(env: &crate::env::KicadEnv) -> Self {
        Self {
            cli_path: env.cli_path.clone(),
        }
    }

    /// Run `kicad-cli sch erc` on `schematic` and parse the JSON report.
    ///
    /// Returns `Ok(report)` whether or not violations were found — a nonzero
    /// "violations exist" exit code is expected and still yields a valid report.
    /// Returns `Err` only on genuine execution failure (e.g. `kicad-cli`
    /// missing, or the schematic failed to load), surfacing stderr.
    pub fn erc(&self, schematic: &Path) -> io::Result<ErcReport> {
        let out = tempfile::Builder::new()
            .prefix("autopcb-erc-")
            .suffix(".json")
            .tempfile()?;

        let output = Command::new(&self.cli_path)
            .args(["sch", "erc", "--format", "json"])
            .arg("--output")
            .arg(out.path())
            .arg("--severity-all")
            .arg("--exit-code-violations")
            .arg(schematic)
            .output()?;

        // The report file is the source of truth: it is written for both the
        // "no violations" (exit 0) and "violations found" (nonzero) cases, but
        // not when the schematic fails to load. So we try to read+parse it and
        // only fall back to an error — surfacing stderr — when that fails.
        let json = std::fs::read_to_string(out.path())?;
        match serde_json::from_str::<ErcReport>(&json) {
            Ok(report) => Ok(report),
            Err(parse_err) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                let detail = if stderr.is_empty() {
                    format!("could not parse ERC report ({parse_err})")
                } else {
                    format!("kicad-cli sch erc failed: {stderr}")
                };
                Err(io::Error::new(io::ErrorKind::InvalidData, detail))
            }
        }
    }

    /// Run `kicad-cli pcb drc` on `pcb` and parse the JSON report.
    ///
    /// Mirrors [`erc`](Self::erc): with `--exit-code-violations` the process
    /// returns nonzero *when violations exist*, which is a normal outcome, not a
    /// failure — the report is still written. We therefore key success on whether
    /// the report file parses, reserving the `Err` path for genuine execution
    /// failures (binary missing, board failed to load) where no report is
    /// produced and stderr carries the cause.
    ///
    /// `--all-track-errors` reports every error per track (not just the first);
    /// the JSON splits findings into separate `violations`, `unconnected_items`,
    /// and `schematic_parity` arrays. We capture the first two — copper DRC and
    /// missing connections — which together are the slice-1 acceptance gate.
    pub fn drc(&self, pcb: &Path) -> io::Result<DrcReport> {
        let out = tempfile::Builder::new()
            .prefix("autopcb-drc-")
            .suffix(".json")
            .tempfile()?;

        let output = Command::new(&self.cli_path)
            .args([
                "pcb",
                "drc",
                "--format",
                "json",
                "--all-track-errors",
                "--exit-code-violations",
            ])
            .arg("--output")
            .arg(out.path())
            .arg(pcb)
            .output()?;

        // As with ERC, the report file is the source of truth: written for both
        // the "no violations" (exit 0) and "violations found" (nonzero) cases,
        // but not when the board fails to load. Parse it; only fall back to an
        // error — surfacing stderr — when that fails.
        let json = std::fs::read_to_string(out.path())?;
        match serde_json::from_str::<DrcReport>(&json) {
            Ok(report) => Ok(report),
            Err(parse_err) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                let detail = if stderr.is_empty() {
                    format!("could not parse DRC report ({parse_err})")
                } else {
                    format!("kicad-cli pcb drc failed: {stderr}")
                };
                Err(io::Error::new(io::ErrorKind::InvalidData, detail))
            }
        }
    }

    /// Run `kicad-cli sch export netlist --format kicadxml` on `schematic` and
    /// parse the result into a [`Netlist`].
    ///
    /// `kicadxml` is chosen over the default `kicadsexpr` because it is a
    /// well-defined XML grammar (`<components>` + `<nets>`) that maps cleanly
    /// onto our structs, and unlike the SPICE/PADS/Allegro formats it loses no
    /// component metadata. A blank schematic yields empty `<components/>` and
    /// `<nets/>`, so `components` and `nets` come back empty.
    ///
    /// Returns `Err` only on genuine execution failure (binary missing, the
    /// schematic failed to load, or unreadable/unparseable output).
    pub fn netlist(&self, schematic: &Path) -> io::Result<Netlist> {
        let out = tempfile::Builder::new()
            .prefix("autopcb-netlist-")
            .suffix(".xml")
            .tempfile()?;

        let output = Command::new(&self.cli_path)
            .args(["sch", "export", "netlist", "--format", "kicadxml"])
            .arg("--output")
            .arg(out.path())
            .arg(schematic)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                "kicad-cli sch export netlist failed".to_string()
            } else {
                format!("kicad-cli sch export netlist failed: {stderr}")
            };
            return Err(io::Error::new(io::ErrorKind::InvalidData, detail));
        }

        let xml = std::fs::read_to_string(out.path())?;
        parse_netlist_xml(&xml)
    }

    /// Run `kicad-cli sch export svg` on `schematic`, writing into `out_dir`.
    ///
    /// KiCAD names the output `<schematic stem>.svg` inside `out_dir`; the
    /// resolved path is returned. If the expected `<stem>.svg` is not present,
    /// falls back to the lexicographically-first `*.svg` in `out_dir` (KiCAD
    /// may emit page-numbered names such as `stem-2.svg` on some versions).
    /// Returns `Err` on execution failure (binary missing, schematic failed to
    /// load) or if no SVG file was produced.
    pub fn export_svg(&self, schematic: &Path, out_dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(out_dir)?;
        let output = Command::new(&self.cli_path)
            .args(["sch", "export", "svg"])
            .arg("--output")
            .arg(out_dir)
            .arg(schematic)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                "kicad-cli sch export svg failed".to_string()
            } else {
                format!("kicad-cli sch export svg failed: {stderr}")
            };
            return Err(io::Error::new(io::ErrorKind::InvalidData, detail));
        }
        let stem = schematic
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "schematic has no stem"))?;
        let svg = out_dir.join(format!("{stem}.svg"));
        if svg.is_file() {
            return Ok(svg);
        }
        // Fallback: collect all *.svg files in out_dir, sort lexicographically
        // (yielding natural KiCAD page order: stem.svg < stem-2.svg < …), and
        // return the first. This is deterministic regardless of filesystem
        // hash-ordering.
        let mut svgs: Vec<PathBuf> = std::fs::read_dir(out_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("svg"))
                    .unwrap_or(false)
            })
            .collect();
        svgs.sort();
        match svgs.into_iter().next() {
            Some(path) => Ok(path),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("expected SVG not produced at {}", svg.display()),
            )),
        }
    }
}

/// A parsed `kicad-cli sch export netlist --format kicadxml` result: the
/// connectivity oracle for lifting a `.kicad_sch` back into a kernel design.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Netlist {
    /// One entry per placed component (`<comp>`), in document order.
    pub components: Vec<NetComp>,
    /// One entry per net (`<net>`), in document order. Includes single-node
    /// nets; unconnected pins do not appear (KiCAD omits them from the netlist).
    pub nets: Vec<Net>,
}

/// A component instance from the netlist (`<comp ref=...>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetComp {
    /// Schematic reference designator, e.g. `"R1"`.
    pub reference: String,
    /// Component value, e.g. `"10k"`.
    pub value: String,
    /// Library id reconstructed from `<libsource lib=".." part=".."/>` as
    /// `"lib:part"`, e.g. `"Device:R"`. Empty if the netlist omits libsource.
    pub lib_id: String,
    /// Field/property metadata: the `<field>` entries (Footprint, Datasheet,
    /// Description, ...) and the `<property>` entries, keyed by name. Footprint
    /// is included whether it arrives as a top-level `<footprint>`, a `<field>`,
    /// or a `<property>`.
    pub properties: HashMap<String, String>,
}

/// A net and the pins it connects (`<net name=...>` with `<node>` children).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Net {
    /// Net name, e.g. `"/VIN"` or `"Net-(C1-Pad2)"`.
    pub name: String,
    /// `(reference, pin)` pairs for every pin on this net.
    pub nodes: Vec<(String, String)>,
}

/// Read an attribute value off a start/empty tag as an owned, unescaped String.
fn attr(tag: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    tag.attributes().flatten().find_map(|a| {
        if a.key.as_ref() == key {
            a.unescape_value().ok().map(|v| v.into_owned())
        } else {
            None
        }
    })
}

/// Parse a `kicadxml` netlist into [`Netlist`].
///
/// Implemented as a small event-driven walk (quick-xml `Reader`) rather than
/// serde-deser so we can keep document order and reconstruct `lib_id` from
/// `<libsource>` without modelling the entire schema. We track which top-level
/// section we are in (`components` vs `nets`) to disambiguate the `<field>`
/// elements that appear in both `<comp>` and `<libpart>`.
fn parse_netlist_xml(xml: &str) -> io::Result<Netlist> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut netlist = Netlist::default();

    // Parser state.
    let mut in_components = false; // inside <components> (not <libparts>)
    let mut in_nets = false;
    let mut comp: Option<NetComp> = None;
    // When inside <comp>, the name of the <field>/<property> whose text we are
    // currently collecting (e.g. a `<field name="Datasheet">..</field>`).
    let mut pending_field: Option<String> = None;
    let mut net: Option<Net> = None;

    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed netlist XML: {e}"),
            )
        })?;

        match event {
            Event::Start(tag) => match tag.name().as_ref() {
                b"components" => in_components = true,
                b"nets" => in_nets = true,
                b"comp" if in_components => {
                    let mut c = NetComp::default();
                    if let Some(r) = attr(&tag, b"ref") {
                        c.reference = r;
                    }
                    comp = Some(c);
                }
                b"value" if comp.is_some() => pending_field = Some("__value".into()),
                b"footprint" if comp.is_some() => pending_field = Some("Footprint".into()),
                b"field" if comp.is_some() => {
                    pending_field = attr(&tag, b"name");
                }
                b"net" if in_nets => {
                    let mut n = Net::default();
                    if let Some(name) = attr(&tag, b"name") {
                        n.name = name;
                    }
                    net = Some(n);
                }
                _ => {}
            },
            Event::Empty(tag) => match tag.name().as_ref() {
                // `<libsource lib=".." part=".."/>` -> lib_id "lib:part".
                b"libsource" if comp.is_some() => {
                    if let Some(c) = comp.as_mut() {
                        let lib = attr(&tag, b"lib").unwrap_or_default();
                        let part = attr(&tag, b"part").unwrap_or_default();
                        c.lib_id = format!("{lib}:{part}");
                    }
                }
                // Self-closing `<field name=".."/>` -> empty value.
                b"field" if comp.is_some() => {
                    if let (Some(c), Some(name)) = (comp.as_mut(), attr(&tag, b"name")) {
                        c.properties.entry(name).or_default();
                    }
                }
                // `<property name=".." value=".."/>`.
                b"property" if comp.is_some() => {
                    if let (Some(c), Some(name)) = (comp.as_mut(), attr(&tag, b"name")) {
                        let value = attr(&tag, b"value").unwrap_or_default();
                        c.properties.insert(name, value);
                    }
                }
                // `<node ref=".." pin=".."/>` inside a net.
                b"node" if in_nets => {
                    if let Some(n) = net.as_mut() {
                        let r = attr(&tag, b"ref").unwrap_or_default();
                        let pin = attr(&tag, b"pin").unwrap_or_default();
                        n.nodes.push((r, pin));
                    }
                }
                _ => {}
            },
            Event::Text(text) if pending_field.is_some() && comp.is_some() => {
                let value = text
                    .unescape()
                    .map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("malformed netlist text: {e}"),
                        )
                    })?
                    .into_owned();
                if let (Some(c), Some(field)) = (comp.as_mut(), pending_field.as_deref()) {
                    if field == "__value" {
                        c.value = value;
                    } else {
                        c.properties.insert(field.to_string(), value);
                    }
                }
            }
            Event::End(tag) => match tag.name().as_ref() {
                b"components" => in_components = false,
                b"nets" => in_nets = false,
                b"comp" => {
                    if let Some(c) = comp.take() {
                        netlist.components.push(c);
                    }
                }
                b"value" | b"footprint" | b"field" => pending_field = None,
                b"net" => {
                    if let Some(n) = net.take() {
                        netlist.nets.push(n);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(netlist)
}

/// The parsed `kicad-cli sch erc --format json` report.
///
/// Violations are reported per-sheet in the JSON (`sheets[].violations[]`);
/// we flatten them into a single list and expose severity counts.
#[derive(Debug, Clone, Deserialize)]
pub struct ErcReport {
    /// All violations across every sheet, flattened.
    #[serde(default, rename = "sheets", deserialize_with = "flatten_sheets")]
    pub violations: Vec<Violation>,
}

/// A single ERC violation (one rule firing at one or more locations).
#[derive(Debug, Clone, Deserialize)]
pub struct Violation {
    /// `"error"`, `"warning"`, or `"exclusion"`.
    pub severity: String,
    /// Machine-readable rule key, e.g. `"wire_dangling"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Human-readable summary of the rule.
    #[serde(default)]
    pub description: String,
    /// Specific locations the rule fired at.
    #[serde(default)]
    pub items: Vec<ViolationItem>,
}

/// One location a violation fired at.
#[derive(Debug, Clone, Deserialize)]
pub struct ViolationItem {
    /// Human-readable description of the offending object.
    #[serde(default)]
    pub description: String,
    /// UUID of the offending object, if any.
    #[serde(default)]
    pub uuid: Option<String>,
}

impl ErcReport {
    /// Number of `error`-severity violations.
    pub fn error_count(&self) -> usize {
        self.count_severity("error")
    }

    /// Number of `warning`-severity violations.
    pub fn warning_count(&self) -> usize {
        self.count_severity("warning")
    }

    fn count_severity(&self, severity: &str) -> usize {
        self.violations
            .iter()
            .filter(|v| v.severity == severity)
            .count()
    }
}

/// The parsed `kicad-cli pcb drc --format json` report.
///
/// Unlike ERC's per-sheet nesting, the PCB DRC report is flat: top-level
/// `violations`, `unconnected_items`, and `schematic_parity` arrays, each a list
/// of [`Violation`]s sharing the same `severity`/`type`/`description`/`items`
/// shape. We capture `violations` (copper/track design-rule findings) and
/// `unconnected_items` (missing connections / airwires) — the two arrays the
/// acceptance gate asserts are empty. `schematic_parity` is left out: it is only
/// populated under `--schematic-parity`, which the routed-board gate does not
/// request (there is no schematic alongside the fixture).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DrcReport {
    /// Copper / track design-rule violations.
    #[serde(default)]
    pub violations: Vec<Violation>,
    /// Missing connections — pads/items that should be on the same net but are
    /// not joined by copper (the routed-board gate requires this to be empty).
    #[serde(default)]
    pub unconnected_items: Vec<Violation>,
}

impl DrcReport {
    /// Number of `error`-severity entries across `violations` + `unconnected_items`.
    pub fn error_count(&self) -> usize {
        self.count_severity("error")
    }

    /// Number of `warning`-severity entries across `violations` + `unconnected_items`.
    pub fn warning_count(&self) -> usize {
        self.count_severity("warning")
    }

    fn count_severity(&self, severity: &str) -> usize {
        self.violations
            .iter()
            .chain(self.unconnected_items.iter())
            .filter(|v| v.severity == severity)
            .count()
    }
}

/// One sheet's worth of violations in the report JSON.
#[derive(Deserialize)]
struct Sheet {
    #[serde(default)]
    violations: Vec<Violation>,
}

/// Deserialize `sheets[]` and flatten every sheet's `violations[]` into one list.
fn flatten_sheets<'de, D>(deserializer: D) -> Result<Vec<Violation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let sheets = Vec::<Sheet>::deserialize(deserializer)?;
    Ok(sheets.into_iter().flat_map(|s| s.violations).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from `kicad-cli sch erc --format json` (10.0.3) on a blank sheet.
    const BLANK_JSON: &str = r#"{
        "$schema": "https://schemas.kicad.org/erc.v1.json",
        "kicad_version": "10.0.3",
        "sheets": [ { "path": "/", "uuid_path": "/x", "violations": [] } ]
    }"#;

    // Captured shape: one error + three warnings across one sheet.
    const VIOLATIONS_JSON: &str = r#"{
        "sheets": [ { "path": "/", "uuid_path": "/x", "violations": [
            { "severity": "error",   "type": "wire_dangling",
              "description": "Wires not connected to anything",
              "items": [ { "description": "Horizontal Wire", "pos": {"x":0.5,"y":0.5},
                           "uuid": "11111111-0000-4000-8000-000000000001" } ] },
            { "severity": "warning", "type": "endpoint_off_grid",
              "description": "Symbol pin or wire end off connection grid", "items": [] },
            { "severity": "warning", "type": "unconnected_wire_endpoint",
              "description": "Unconnected wire endpoint", "items": [] },
            { "severity": "warning", "type": "unconnected_wire_endpoint",
              "description": "Unconnected wire endpoint", "items": [] }
        ] } ]
    }"#;

    #[test]
    fn blank_report_parses_with_no_violations() {
        let report: ErcReport = serde_json::from_str(BLANK_JSON).unwrap();
        assert_eq!(report.error_count(), 0);
        assert_eq!(report.warning_count(), 0);
        assert!(report.violations.is_empty());
    }

    #[test]
    fn violations_are_flattened_and_counted_by_severity() {
        let report: ErcReport = serde_json::from_str(VIOLATIONS_JSON).unwrap();
        assert_eq!(report.violations.len(), 4);
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.warning_count(), 3);

        let dangling = &report.violations[0];
        assert_eq!(dangling.kind, "wire_dangling");
        assert_eq!(dangling.severity, "error");
        assert_eq!(
            dangling.items[0].uuid.as_deref(),
            Some("11111111-0000-4000-8000-000000000001")
        );
    }

    // Captured shape from `kicad-cli pcb drc --format json` (10.0.3): a flat
    // report with separate `violations`, `unconnected_items`, and
    // `schematic_parity` arrays. Here: two warning-level footprint mismatches in
    // `violations`, one error-level missing connection in `unconnected_items`.
    const DRC_JSON: &str = r#"{
        "$schema": "https://schemas.kicad.org/drc.v1.json",
        "kicad_version": "10.0.3",
        "coordinate_units": "mm",
        "source": "two_res.kicad_pcb",
        "violations": [
            { "severity": "warning", "type": "lib_footprint_mismatch",
              "description": "Footprint 'R_0805_2012Metric' does not match copy in library 'Resistor_SMD'",
              "items": [ { "description": "Footprint R1",
                           "uuid": "00000000-0000-0000-0000-000000000010" } ] },
            { "severity": "warning", "type": "lib_footprint_mismatch",
              "description": "Footprint 'R_0805_2012Metric' does not match copy in library 'Resistor_SMD'",
              "items": [ { "description": "Footprint R2" } ] }
        ],
        "unconnected_items": [
            { "severity": "error", "type": "unconnected_items",
              "description": "Missing connection between items",
              "items": [ { "description": "Pad 2 [GND] of R1 on F.Cu",
                           "uuid": "00000000-0000-0000-0000-000000000014" },
                         { "description": "Pad 2 [GND] of R2 on F.Cu" } ] }
        ],
        "schematic_parity": []
    }"#;

    #[test]
    fn drc_report_parses_violations_and_unconnected_items() {
        let report: DrcReport = serde_json::from_str(DRC_JSON).unwrap();
        assert_eq!(report.violations.len(), 2);
        assert_eq!(report.unconnected_items.len(), 1);

        // Counts span both arrays.
        assert_eq!(report.error_count(), 1); // the missing connection
        assert_eq!(report.warning_count(), 2); // the two footprint mismatches

        let unconnected = &report.unconnected_items[0];
        assert_eq!(unconnected.kind, "unconnected_items");
        assert_eq!(unconnected.severity, "error");
        assert_eq!(unconnected.items.len(), 2);
        assert_eq!(
            unconnected.items[0].uuid.as_deref(),
            Some("00000000-0000-0000-0000-000000000014")
        );
    }

    #[test]
    fn drc_report_clean_board_has_no_violations() {
        let clean = r#"{
            "$schema": "https://schemas.kicad.org/drc.v1.json",
            "violations": [],
            "unconnected_items": [],
            "schematic_parity": []
        }"#;
        let report: DrcReport = serde_json::from_str(clean).unwrap();
        assert!(report.violations.is_empty());
        assert!(report.unconnected_items.is_empty());
        assert_eq!(report.error_count(), 0);
        assert_eq!(report.warning_count(), 0);
    }

    // Captured verbatim from `kicad-cli sch export netlist --format kicadxml`
    // (10.0.3) on the rc_pair fixture (an R and a C with their pin-2s wired
    // together and each pin-1 labelled). Trimmed to the structurally relevant
    // elements. This exercises the parser without needing KiCAD on PATH.
    const RC_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<export version="E">
  <design><source>x</source></design>
  <components>
    <comp ref="C1">
      <value>100nF</value>
      <footprint>Capacitor_SMD:C_0603_1608Metric</footprint>
      <fields>
        <field name="Footprint">Capacitor_SMD:C_0603_1608Metric</field>
        <field name="Datasheet"/>
        <field name="Description"/>
      </fields>
      <libsource lib="Device" part="C" description="Unpolarized capacitor"/>
      <property name="Sheetname" value=""/>
      <property name="ki_keywords" value="cap capacitor"/>
      <tstamps>33333333-0000-4000-8000-000000000002</tstamps>
    </comp>
    <comp ref="R1">
      <value>10k</value>
      <footprint>Resistor_SMD:R_0603_1608Metric</footprint>
      <fields>
        <field name="Footprint">Resistor_SMD:R_0603_1608Metric</field>
        <field name="Datasheet"/>
      </fields>
      <libsource lib="Device" part="R" description="Resistor"/>
      <property name="ki_keywords" value="R res resistor"/>
    </comp>
  </components>
  <libparts>
    <libpart lib="Device" part="C">
      <fields>
        <field name="Reference">C</field>
        <field name="Value">C</field>
      </fields>
    </libpart>
  </libparts>
  <nets>
    <net code="1" name="/VIN" class="Default">
      <node ref="R1" pin="1" pintype="passive"/>
    </net>
    <net code="2" name="/VOUT" class="Default">
      <node ref="C1" pin="1" pintype="passive"/>
    </net>
    <net code="3" name="Net-(C1-Pad2)" class="Default">
      <node ref="C1" pin="2" pintype="passive"/>
      <node ref="R1" pin="2" pintype="passive"/>
    </net>
  </nets>
</export>"#;

    #[test]
    fn parses_components_with_lib_id_value_and_properties() {
        let nl = parse_netlist_xml(RC_XML).unwrap();
        assert_eq!(nl.components.len(), 2);

        // Document order is preserved (C1 then R1, as KiCAD emits).
        assert_eq!(nl.components[0].reference, "C1");
        assert_eq!(nl.components[1].reference, "R1");

        let c = &nl.components[0];
        assert_eq!(c.value, "100nF");
        assert_eq!(c.lib_id, "Device:C"); // reconstructed from <libsource>
        assert_eq!(
            c.properties.get("Footprint").map(String::as_str),
            Some("Capacitor_SMD:C_0603_1608Metric")
        );
        // A <property> with a value is captured.
        assert_eq!(
            c.properties.get("ki_keywords").map(String::as_str),
            Some("cap capacitor")
        );
        // A self-closing <field name="Datasheet"/> yields an empty value, not
        // an absent key.
        assert_eq!(c.properties.get("Datasheet").map(String::as_str), Some(""));

        let r = &nl.components[1];
        assert_eq!(r.value, "10k");
        assert_eq!(r.lib_id, "Device:R");
    }

    #[test]
    fn parses_nets_preserving_nodes_and_order() {
        let nl = parse_netlist_xml(RC_XML).unwrap();
        assert_eq!(nl.nets.len(), 3);

        // The labelled single-node nets.
        let vin = nl.nets.iter().find(|n| n.name == "/VIN").unwrap();
        assert_eq!(vin.nodes, vec![("R1".to_string(), "1".to_string())]);

        // The shared 2-node net carries both pin-2 nodes, in document order.
        let shared = nl.nets.iter().find(|n| n.nodes.len() == 2).unwrap();
        assert_eq!(shared.name, "Net-(C1-Pad2)");
        assert_eq!(
            shared.nodes,
            vec![
                ("C1".to_string(), "2".to_string()),
                ("R1".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn blank_netlist_has_no_components_or_nets() {
        let blank = r#"<?xml version="1.0" encoding="UTF-8"?>
<export version="E">
  <design><source>x</source></design>
  <components/>
  <libparts/>
  <libraries/>
  <nets/>
</export>"#;
        let nl = parse_netlist_xml(blank).unwrap();
        assert!(nl.components.is_empty());
        assert!(nl.nets.is_empty());
    }
}
