//! `check_schematic`: the symbol-aware lints, deterministic electrical rules,
//! completeness audit, and KiCad ERC over the live file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use gordian_runtime::AgentRuntime;
use indexmap::IndexMap;
use sch_check::model::{Block, Component, Design, NetAttrs, PinTarget};
use sch_doc::{NetSource, Netlist, PlacedPin, SchDoc, placed_pins};
use serde::Serialize;
use serde_json::{Value, json};

const COMPACT_FINDING_LIMIT: usize = 40;

/// Reduce the live sheet to the kernel model the checkers run on.
///
/// One sheet is one block: the extractor's scope is a file, and the checkers
/// only use blocks to group, never to separate connectivity.
pub(crate) fn design(doc: &SchDoc, netlist: &Netlist) -> Design {
    let pins = placed_pins(doc);
    let mut components: IndexMap<String, Component> = IndexMap::new();
    // The units of a multi-unit part are separate symbols sharing one
    // reference; they are one component, and folding them together is what
    // stops the lints seeing each unit's pins as a design of its own.
    for symbol in doc.symbols() {
        let field = |name: &str| {
            symbol
                .fields
                .get(name)
                .map(|f| f.value.clone())
                .filter(|v| !v.is_empty())
        };
        let component = components
            .entry(symbol.refdes().to_string())
            .or_insert_with(|| Component {
                part: symbol.lib_id.clone(),
                value: field("Value"),
                footprint: field("Footprint"),
                dnp: symbol.dnp,
                ..Component::default()
            });
        for pin in pins.iter().filter(|p| p.owner == symbol.uuid) {
            let target = match crate::refs::net_of(netlist, &pin.refdes, &pin.number) {
                Some(net) => PinTarget::Net(net.to_string()),
                None => PinTarget::NoConnect,
            };
            component.pins.insert(pin.number.clone(), target);
        }
    }
    let nets = netlist
        .nets
        .iter()
        .map(|net| {
            (
                net.name.clone(),
                NetAttrs {
                    power: net.source == NetSource::Power,
                    port: net.source == NetSource::Global,
                    class: None,
                },
            )
        })
        .collect();
    let mut blocks = IndexMap::new();
    blocks.insert(
        "main".to_string(),
        Block {
            note: None,
            components,
            layout: Vec::new(),
        },
    );
    Design {
        name: None,
        description: None,
        blocks,
        nets,
        lint_allow: Default::default(),
    }
}

#[derive(Clone, Serialize)]
struct Finding {
    classification: &'static str,
    severity: String,
    source: &'static str,
    code: String,
    message: String,
    refs: Vec<String>,
    nets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<[f64; 2]>,
    fix: String,
    #[serde(skip_serializing_if = "is_false")]
    advisory: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Finding {
    fn line(&self) -> String {
        let references = if self.refs.is_empty() {
            String::new()
        } else {
            format!(" {}", self.refs.join(", "))
        };
        let nets = if self.nets.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.nets.join(", "))
        };
        format!(
            "{}[{}]{}{}: {} — {}",
            self.severity, self.code, references, nets, self.message, self.fix
        )
    }
}

struct FindingLocator<'a> {
    doc: &'a SchDoc,
    netlist: &'a Netlist,
    pins: Vec<PlacedPin>,
}

impl<'a> FindingLocator<'a> {
    fn new(doc: &'a SchDoc, netlist: &'a Netlist) -> Self {
        Self {
            doc,
            netlist,
            pins: placed_pins(doc),
        }
    }

    fn locate(
        &self,
        text: &str,
        explicit_refs: impl IntoIterator<Item = String>,
        explicit_nets: impl IntoIterator<Item = String>,
    ) -> (Vec<String>, Vec<String>, Option<[f64; 2]>) {
        let mut refs = explicit_refs.into_iter().collect::<BTreeSet<_>>();
        let mut nets = explicit_nets.into_iter().collect::<BTreeSet<_>>();

        for pin in &self.pins {
            let numbered = format!("{}.{}", pin.refdes, pin.number);
            let named = format!("{}.{}", pin.refdes, pin.name);
            let described = contains_refdes(text, &pin.refdes)
                && (contains_name(text, &format!("Pin {}", pin.number))
                    || contains_name(text, &format!("pin {}", pin.number)));
            if contains_name(text, &numbered) || contains_name(text, &named) || described {
                refs.insert(numbered);
            }
        }
        for symbol in self.doc.symbols() {
            if contains_refdes(text, symbol.refdes())
                && !refs
                    .iter()
                    .any(|found| found.starts_with(&format!("{}.", symbol.refdes())))
            {
                refs.insert(symbol.refdes().to_string());
            }
        }
        for net in &self.netlist.nets {
            if contains_name(text, &net.name) {
                nets.insert(net.name.clone());
            }
        }
        for reference in &refs {
            let Some((refdes, pin)) = reference.rsplit_once('.') else {
                continue;
            };
            if let Some(net) = crate::refs::net_of(self.netlist, refdes, pin) {
                nets.insert(net.to_string());
            }
        }
        let at = refs
            .iter()
            .find_map(|reference| self.reference_at(reference));
        (refs.into_iter().collect(), nets.into_iter().collect(), at)
    }

    fn refs_for_uuid(&self, uuid: &str) -> Vec<String> {
        let mut refs = BTreeSet::new();
        for symbol in self.doc.symbols() {
            if symbol.uuid == uuid {
                refs.insert(symbol.refdes().to_string());
            }
            for (pin, pin_uuid) in &symbol.pin_uuids {
                if pin_uuid == uuid {
                    refs.insert(format!("{}.{pin}", symbol.refdes()));
                }
            }
        }
        refs.into_iter().collect()
    }

    fn at_for_uuid(&self, uuid: &str) -> Option<[f64; 2]> {
        for symbol in self.doc.symbols() {
            if symbol.uuid == uuid {
                return Some(round_point(symbol.at.x, symbol.at.y));
            }
            if let Some((pin, _)) = symbol
                .pin_uuids
                .iter()
                .find(|(_, pin_uuid)| pin_uuid.as_str() == uuid)
                && let Some(placed) = self
                    .pins
                    .iter()
                    .find(|placed| placed.owner == symbol.uuid && placed.number == *pin)
            {
                return Some(round_point(placed.at.x, placed.at.y));
            }
        }
        None
    }

    fn reference_at(&self, reference: &str) -> Option<[f64; 2]> {
        if let Some((refdes, pin)) = reference.rsplit_once('.')
            && let Some(found) = self
                .pins
                .iter()
                .find(|found| found.refdes == refdes && found.number == pin)
        {
            return Some(round_point(found.at.x, found.at.y));
        }
        self.doc
            .symbols()
            .find(|symbol| symbol.refdes() == reference)
            .map(|symbol| round_point(symbol.at.x, symbol.at.y))
    }
}

fn round_point(x: f64, y: f64) -> [f64; 2] {
    [(x * 1000.0).round() / 1000.0, (y * 1000.0).round() / 1000.0]
}

fn contains_name(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    text.match_indices(name).any(|(start, _)| {
        let end = start + name.len();
        let boundary = |character: Option<char>| {
            character.is_none_or(|character| !character.is_ascii_alphanumeric())
        };
        boundary(text[..start].chars().next_back()) && boundary(text[end..].chars().next())
    })
}

fn contains_refdes(text: &str, reference: &str) -> bool {
    text.match_indices(reference).any(|(start, _)| {
        let end = start + reference.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        before.is_none_or(|character| !character.is_ascii_alphanumeric())
            && after.is_none_or(|character| !character.is_ascii_alphanumeric() && character != '.')
    })
}

fn strip_subject_prefix(message: String, refs: &[String], nets: &[String]) -> String {
    refs.iter()
        .chain(nets)
        .find_map(|subject| message.strip_prefix(&format!("{subject}: ")))
        .unwrap_or(&message)
        .to_string()
}

fn message_and_fix(message: &str, suggestion: Option<&str>, code: &str) -> (String, String) {
    let message = message.trim().trim_start_matches("- ");
    if let Some((message, fix)) = message.rsplit_once(" — ") {
        return (message.to_string(), fix.to_string());
    }
    let fix = suggestion.map_or_else(|| default_fix(code), str::to_string);
    (message.to_string(), fix)
}

fn default_fix(code: &str) -> String {
    match code {
        "power_pin_not_driven" => "add a PWR_FLAG or a regulator output",
        "pin_not_connected" | "pin_not_driven" | "wire_dangling" => {
            "connect the cited pin or mark it no-connect when intentional"
        }
        "lib_symbol_issues" => "install or remap the named symbol library",
        "footprint_link_issues" => "install or remap the named footprint library",
        "near-name" => "verify the two net names and rename the typo",
        "unreferenced-net" => "connect the declared net or remove the stale declaration",
        "footprint-unknown" => "assign an installed Library:Footprint",
        _ => "inspect the cited objects and correct this finding",
    }
    .to_string()
}

fn diagnostic_finding(
    locator: &FindingLocator<'_>,
    diagnostic: &sch_check::Diagnostic,
    source: &'static str,
) -> Finding {
    let severity = match diagnostic.severity {
        sch_check::Severity::Error => "error",
        sch_check::Severity::Warning => "warning",
    };
    let (message, fix) = message_and_fix(
        &diagnostic.message,
        diagnostic.suggestion.as_deref(),
        diagnostic.code,
    );
    let (refs, nets, at) = locator.locate(&message, [], []);
    let message = strip_subject_prefix(message, &refs, &nets);
    Finding {
        classification: "introduced",
        severity: severity.to_string(),
        source,
        code: diagnostic.code.to_string(),
        message,
        refs,
        nets,
        at,
        fix,
        advisory: diagnostic.severity == sch_check::Severity::Warning,
    }
}

fn erc_finding(locator: &FindingLocator<'_>, violation: &kicad::Violation) -> Finding {
    let mut explicit_refs = BTreeSet::new();
    let mut explicit_at = None;
    let mut item_text = String::new();
    for item in &violation.items {
        item_text.push(' ');
        item_text.push_str(&item.description);
        if let Some(uuid) = &item.uuid {
            explicit_refs.extend(locator.refs_for_uuid(uuid));
            explicit_at = explicit_at.or_else(|| locator.at_for_uuid(uuid));
        }
    }
    let searchable = format!("{}{}", violation.description, item_text);
    let (message, fix) = message_and_fix(&violation.description, None, &violation.kind);
    let (refs, nets, inferred_at) = locator.locate(&searchable, explicit_refs, []);
    let reported_at = violation
        .items
        .iter()
        .find_map(|item| item.pos)
        .map(|position| round_point(position.x, position.y));
    Finding {
        classification: "introduced",
        severity: violation.severity.clone(),
        source: "kicad_erc",
        code: violation.kind.clone(),
        message,
        refs,
        nets,
        at: explicit_at.or(inferred_at).or(reported_at),
        fix,
        advisory: violation.severity != "error",
    }
}

fn pad_clause(prefix: &str, pads: &[String]) -> String {
    if pads.is_empty() {
        String::new()
    } else {
        format!("{prefix}{})", pads.join(", "))
    }
}

fn add_rendered_findings(report: &mut Value, findings: &[Finding], detail: bool) {
    let shown = if detail {
        findings.len()
    } else if findings.len() > COMPACT_FINDING_LIMIT {
        COMPACT_FINDING_LIMIT - 1
    } else {
        findings.len()
    };
    let introduced = findings
        .iter()
        .filter(|finding| finding.classification == "introduced")
        .count();
    let pre_existing = findings.len() - introduced;
    let mut lines = vec![format!(
        "{introduced} introduced, {pre_existing} pre-existing"
    )];
    lines.extend(findings[..shown].iter().map(Finding::line));
    let omitted = findings.len() - shown;
    if omitted > 0 {
        lines.push(format!(
            "+{omitted} more — rerun check_schematic with {{\"detail\":true}}"
        ));
        report["findings_omitted"] = json!(omitted);
    }
    report["findings"] = json!(&findings[..shown]);
    report["diagnostics"] = json!(lines);
    report["text"] = json!(
        report["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

struct Inspection {
    netlist: Netlist,
    gaps: Vec<sch_check::completeness::Gap>,
    findings: Vec<Finding>,
    local_errors: usize,
    local_warnings: usize,
    erc: std::result::Result<kicad::ErcReport, String>,
}

fn inspect_schematic(path: &Path, ctx: &AgentRuntime) -> Result<Inspection> {
    let doc = SchDoc::read(path).with_context(|| format!("reading {}", path.display()))?;
    let netlist = sch_doc::connect::extract(&doc);
    let design = design(&doc, &netlist);
    let locator = FindingLocator::new(&doc, &netlist);
    let gaps = sch_check::completeness::audit(&design, ctx.provider());
    let mut findings = Vec::new();

    let lint = sch_check::lint::lint(&design, ctx.provider());
    findings.extend(
        lint.0
            .iter()
            .map(|diagnostic| diagnostic_finding(&locator, diagnostic, "lint")),
    );
    for defect in sch_check::erc::defects(&design, ctx.provider()) {
        let diagnostic = if defect.blocking {
            sch_check::Diagnostic::error(defect.code, defect.line)
        } else {
            sch_check::Diagnostic::warning(defect.code, defect.line)
        };
        findings.push(diagnostic_finding(
            &locator,
            &diagnostic,
            "deterministic_erc",
        ));
    }
    for problem in gordian_runtime::footprint_compat::unresolvable_footprints(ctx, &design)? {
        let diagnostic = if problem.malformed {
            sch_check::Diagnostic::error("footprint-id", problem.message)
        } else {
            sch_check::Diagnostic::warning("footprint-unknown", problem.message)
        };
        findings.push(diagnostic_finding(&locator, &diagnostic, "footprint"));
    }
    for mismatch in gordian_runtime::footprint_compat::design_pin_mismatches(ctx, &design)? {
        let diagnostic = sch_check::Diagnostic::error(
            "footprint-pins",
            format!(
                "{}: symbol `{}` and footprint `{}` do not agree on pads{}{}{} — swap_symbol to a \
                 part with the footprint's pad numbers, or assign a package that matches the pins",
                mismatch.reference,
                mismatch.symbol,
                mismatch.footprint,
                pad_clause(
                    " (pads with no pin: ",
                    &mismatch.footprint_pads_absent_from_symbol
                ),
                pad_clause(
                    " (pins with no pad: ",
                    &mismatch.symbol_pins_absent_from_footprint
                ),
                mismatch
                    .polarity_mismatch
                    .map(|why| format!(" ({why})"))
                    .unwrap_or_default(),
            ),
        );
        findings.push(diagnostic_finding(&locator, &diagnostic, "footprint"));
    }
    for warning in &netlist.warnings {
        let (message, fix) = message_and_fix(warning, None, "extractor");
        let (refs, nets, at) = locator.locate(&message, [], []);
        findings.push(Finding {
            classification: "introduced",
            severity: "warning".to_string(),
            source: "extractor",
            code: "extractor".to_string(),
            message,
            refs,
            nets,
            at,
            fix,
            advisory: true,
        });
    }
    for gap in &gaps {
        let refs = gap.refdes.iter().cloned();
        let nets = gap.net.iter().cloned();
        let message = format!(
            "{} support circuitry is missing",
            gap.kind.replace('_', " ")
        );
        let (refs, nets, at) = locator.locate(&message, refs, nets);
        findings.push(Finding {
            classification: "introduced",
            severity: "warning".to_string(),
            source: "completeness",
            code: gap.kind.clone(),
            message,
            refs,
            nets,
            at,
            fix: gap.suggestion.clone(),
            advisory: true,
        });
    }

    let local_errors = findings
        .iter()
        .filter(|finding| finding.source != "completeness" && finding.severity == "error")
        .count();
    let local_warnings = findings
        .iter()
        .filter(|finding| finding.source != "completeness" && finding.severity == "warning")
        .count();
    let erc = ctx
        .env()
        .erc(path)
        .map_err(|error| format!("running ERC: {error}"));
    if let Ok(erc) = &erc {
        findings.extend(
            erc.violations
                .iter()
                .map(|violation| erc_finding(&locator, violation)),
        );
    }
    Ok(Inspection {
        netlist,
        gaps,
        findings,
        local_errors,
        local_warnings,
        erc,
    })
}

fn finding_key(finding: &Finding) -> (String, Vec<String>, Vec<String>) {
    (
        finding.code.clone(),
        finding.refs.clone(),
        finding.nets.clone(),
    )
}

fn classify_findings(findings: &mut [Finding], baseline: &[Finding]) {
    let mut available = BTreeMap::new();
    for finding in baseline {
        *available.entry(finding_key(finding)).or_insert(0usize) += 1;
    }
    for finding in findings {
        let count = available.entry(finding_key(finding)).or_default();
        if *count > 0 {
            finding.classification = "pre_existing";
            *count -= 1;
        }
    }
}

/// Lint and run ERC, classifying findings against this turn's first snapshot.
///
/// `ok` and `erc_clean` consider introduced errors only; inherited errors do
/// not block completion of an otherwise clean focused edit.
pub fn check_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let detail = input.get("detail").and_then(Value::as_bool) == Some(true);
    let mut inspection = inspect_schematic(ctx.sch_path(), ctx)?;
    let baseline = ctx.revisions().turn_baseline(ctx.sch_path())?;
    let baseline_revision = baseline.as_ref().map(|baseline| baseline.revision);
    let baseline_inspection = baseline
        .as_ref()
        .and_then(|baseline| baseline.path.as_deref())
        .map(|path| inspect_schematic(path, ctx))
        .transpose()?;
    if let Some(baseline) = &baseline_inspection {
        classify_findings(&mut inspection.findings, &baseline.findings);
    }
    inspection.findings.sort_by_key(|finding| {
        (
            finding.classification != "introduced",
            finding.severity != "error",
        )
    });
    let introduced = inspection
        .findings
        .iter()
        .filter(|finding| finding.classification == "introduced")
        .count();
    let pre_existing = inspection.findings.len() - introduced;
    let introduced_errors = inspection
        .findings
        .iter()
        .filter(|finding| finding.classification == "introduced" && finding.severity == "error")
        .count();
    let introduced_erc_errors = inspection
        .findings
        .iter()
        .filter(|finding| {
            finding.classification == "introduced"
                && finding.source == "kicad_erc"
                && finding.severity == "error"
        })
        .count();
    let mut report = json!({
        "ok": introduced_errors == 0,
        "baseline_revision": baseline_revision,
        "introduced": introduced,
        "pre_existing": pre_existing,
        "detail": detail,
        "checks": {
            "errors": inspection.local_errors,
            "warnings": inspection.local_warnings,
        },
        "extractor_warnings": inspection.netlist.warnings,
        "unconnected_pins": inspection.netlist.unconnected.iter().map(crate::refs::label).collect::<Vec<_>>(),
        "completeness": {
            "warnings": inspection.gaps.len(),
            "gaps": inspection.gaps,
        },
    });

    match &inspection.erc {
        Ok(erc) => {
            let errors = erc.error_count();
            let warnings = erc.warning_count();
            report["errors"] = json!(errors);
            report["warnings"] = json!(warnings);
            report["erc"] = json!({
                "errors": errors,
                "warnings": warnings,
                "findings": erc.violations.len(),
            });
            report["erc_clean"] = json!(introduced_erc_errors == 0);
            if introduced_errors == 0 {
                report["message"] = if pre_existing > 0 {
                    json!(format!(
                        "no introduced errors; {pre_existing} pre-existing finding(s) remain"
                    ))
                } else if inspection.gaps.is_empty() {
                    json!(
                        "schematic is clean and complete by deterministic rules; placement is final"
                    )
                } else {
                    json!("schematic is electrically clean; completeness findings are advisory")
                };
            }
        }
        Err(error) => {
            report["ok"] = json!(false);
            report["erc_clean"] = json!(false);
            report["erc"] = json!({ "error": error });
        }
    }
    report["finding_counts"] = json!({
        "errors": inspection.findings.iter().filter(|finding| finding.severity == "error").count(),
        "warnings": inspection.findings.iter().filter(|finding| finding.severity == "warning").count(),
        "exclusions": inspection.findings.iter().filter(|finding| finding.severity == "exclusion").count(),
        "introduced": introduced,
        "pre_existing": pre_existing,
        "introduced_errors": introduced_errors,
    });
    add_rendered_findings(&mut report, &inspection.findings, detail);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(code: &str, reference: &str, at: [f64; 2]) -> Finding {
        Finding {
            classification: "introduced",
            severity: "warning".to_owned(),
            source: "test",
            code: code.to_owned(),
            message: "warning".to_owned(),
            refs: vec![reference.to_owned()],
            nets: vec!["NET".to_owned()],
            at: Some(at),
            fix: "fix".to_owned(),
            advisory: true,
        }
    }

    #[test]
    fn edit_classifies_one_introduced_and_two_pre_existing_warnings() {
        let baseline = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
        ];
        let mut current = vec![
            warning("first", "R1", [10.0, 10.0]),
            warning("second", "R2", [2.0, 2.0]),
            warning("third", "R3", [3.0, 3.0]),
        ];

        classify_findings(&mut current, &baseline);

        assert_eq!(
            current
                .iter()
                .filter(|finding| finding.classification == "introduced")
                .count(),
            1
        );
        assert_eq!(
            current
                .iter()
                .filter(|finding| finding.classification == "pre_existing")
                .count(),
            2
        );
    }

    #[test]
    fn undo_to_baseline_has_no_introduced_warnings() {
        let baseline = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
        ];
        let mut restored = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
        ];

        classify_findings(&mut restored, &baseline);

        assert!(
            restored
                .iter()
                .all(|finding| finding.classification == "pre_existing")
        );
    }

    #[test]
    fn fresh_sheet_without_baseline_has_only_introduced_warnings() {
        let mut findings = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
            warning("third", "R3", [3.0, 3.0]),
        ];

        classify_findings(&mut findings, &[]);

        assert!(
            findings
                .iter()
                .all(|finding| finding.classification == "introduced")
        );
    }
}
