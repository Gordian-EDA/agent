//! Wrappers around the `kicad-cli` subcommands the agent invokes after
//! writing a schematic. Currently: ERC (Electrical Rules Check).
//!
//! `kicad-cli sch erc` writes a JSON report and, with `--exit-code-violations`,
//! returns a nonzero exit code *when violations exist*. That nonzero exit is a
//! normal outcome, not a failure: the JSON report is still written. We therefore
//! key success on whether the report file parses, not on the process exit code,
//! reserving the `Err` path for genuine execution failures (binary missing,
//! schematic failed to load) where no report is produced.

use std::io;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// A handle to `kicad-cli` for running schematic-level checks.
pub struct KicadCli {
    cli_path: std::path::PathBuf,
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
}
