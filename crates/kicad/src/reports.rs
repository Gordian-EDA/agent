use serde::Deserialize;

/// The parsed `kicad-cli sch erc --format json` report.
///
/// Violations are reported per-sheet in the JSON (`sheets[].violations[]`);
/// we flatten them into a single list and expose severity counts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErcReport {
    /// All violations across every sheet, flattened.
    #[serde(default, rename = "sheets", deserialize_with = "flatten_sheets")]
    pub violations: Vec<Violation>,
}

/// A single ERC/DRC violation (one rule firing at one or more locations).
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
    /// Location in the report's native coordinate system.
    #[serde(default)]
    pub pos: Option<ReportPosition>,
}

/// A location emitted by KiCad's JSON reports.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ReportPosition {
    pub x: f64,
    pub y: f64,
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
/// The PCB DRC report is flat: top-level `violations`, `unconnected_items`,
/// and `schematic_parity` arrays. We capture copper/track findings and missing
/// connections, the two arrays the routed-board gate asserts on.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DrcReport {
    /// Copper / track design-rule violations.
    #[serde(default)]
    pub violations: Vec<Violation>,
    /// Missing connections — pads/items that should be on the same net but are
    /// not joined by copper.
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

    /// Number of `error`-severity copper faults, excluding missing connections.
    pub fn copper_error_count(&self) -> usize {
        self.violations
            .iter()
            .filter(|v| v.severity == "error")
            .count()
    }

    fn count_severity(&self, severity: &str) -> usize {
        self.violations
            .iter()
            .chain(self.unconnected_items.iter())
            .filter(|v| v.severity == severity)
            .count()
    }
}

#[derive(Deserialize)]
struct Sheet {
    #[serde(default)]
    violations: Vec<Violation>,
}

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

    const BLANK_JSON: &str = r#"{
        "$schema": "https://schemas.kicad.org/erc.v1.json",
        "kicad_version": "10.0.3",
        "sheets": [ { "path": "/", "uuid_path": "/x", "violations": [] } ]
    }"#;

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
        let position = dangling.items[0].pos.expect("reported position");
        assert_eq!((position.x, position.y), (0.5, 0.5));
    }

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
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.warning_count(), 2);

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
}
