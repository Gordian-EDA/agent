//! `check_schematic`: the symbol-aware lints, the deterministic electrical
//! rules, and KiCAD's own ERC, over the live file.

use anyhow::Result;
use gordian_runtime::AgentRuntime;
use gordian_runtime::tool::compile_report;
use indexmap::IndexMap;
use sch_check::model::{Block, Component, Design, NetAttrs, PinTarget};
use sch_doc::{NetSource, Netlist, SchDoc, placed_pins};
use serde_json::{Value, json};

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

/// ERC findings, errors first and one line per distinct message, so a sheet
/// with forty identical library warnings does not bury its one real error.
fn erc_violations(report: &kicad::ErcReport) -> Vec<String> {
    let mut lines: Vec<String> = report
        .violations
        .iter()
        .map(|v| format!("{}[{}]: {}", v.severity, v.kind, v.description))
        .collect();
    lines.sort_by_key(|line| (!line.starts_with("error"), line.clone()));
    lines.dedup();
    lines.truncate(40);
    lines
}

/// Lint, electrically check, and run KiCAD ERC over the live schematic.
pub fn check_schematic(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (doc, netlist) = crate::session::Edit::read(ctx)?;
    let design = design(&doc, &netlist);
    let mut diagnostics = sch_check::lint::lint(&design, ctx.provider());
    for message in sch_check::erc::erc_checks(&design, ctx.provider()) {
        diagnostics.push(sch_check::Diagnostic::warning("electrical", message));
    }
    let mut report = compile_report(&diagnostics);
    report["extractor_warnings"] = json!(netlist.warnings);
    report["unconnected_pins"] = json!(
        netlist
            .unconnected
            .iter()
            .map(crate::refs::label)
            .collect::<Vec<_>>()
    );

    match ctx.env().erc(ctx.sch_path()) {
        Ok(erc) => {
            let errors = erc.error_count();
            report["erc"] = json!({
                "errors": errors,
                "warnings": erc.warning_count(),
                "violations": erc_violations(&erc),
            });
            report["erc_clean"] = json!(errors == 0);
            if errors > 0 {
                report["ok"] = json!(false);
            }
        }
        Err(error) => {
            report["erc"] = json!({ "error": format!("running ERC: {error}") });
        }
    }
    Ok(report)
}
