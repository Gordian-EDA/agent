//! Revision-aware schematic changes for the model's edit loop.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use gordian_runtime::revisions::RevisionId;
use sch_doc::{NetDelta, Netlist, SchDoc, SymbolInst, connect};
use serde::Serialize;
use serde_json::{Value, json};

use crate::session::{comparison_revision, delta_json};

#[derive(Serialize)]
struct Pose {
    at: [f64; 2],
    rot: f64,
    mirror: String,
}

#[derive(Serialize)]
struct Moved {
    #[serde(rename = "ref")]
    reference: String,
    from: Pose,
    to: Pose,
}

#[derive(Serialize)]
struct FieldChanged {
    #[serde(rename = "ref")]
    reference: String,
    field: String,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Serialize)]
struct Swapped {
    #[serde(rename = "ref")]
    reference: String,
    from: String,
    to: String,
}

#[derive(Serialize)]
struct CountChanged {
    from: usize,
    to: usize,
    delta: i64,
}

struct Changes {
    added: Vec<String>,
    removed: Vec<String>,
    moved: Vec<Moved>,
    fields_changed: Vec<FieldChanged>,
    swapped: Vec<Swapped>,
    net_delta: NetDelta,
    wires: Option<CountChanged>,
    labels: Option<CountChanged>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.moved.is_empty()
            && self.fields_changed.is_empty()
            && self.swapped.is_empty()
            && self.net_delta.is_empty()
            && self.wires.is_none()
            && self.labels.is_none()
    }
}

fn symbol_key(symbol: &SymbolInst) -> String {
    format!("{}/{}", symbol.refdes(), symbol.unit)
}

fn symbol_table(doc: &SchDoc) -> BTreeMap<String, &SymbolInst> {
    let mut symbols = BTreeMap::new();
    for symbol in doc.symbols() {
        symbols.entry(symbol_key(symbol)).or_insert(symbol);
    }
    symbols
}

fn pose(symbol: &SymbolInst) -> Pose {
    Pose {
        at: [symbol.at.x, symbol.at.y],
        rot: symbol.at.rot,
        mirror: format!("{:?}", symbol.mirror),
    }
}

fn pose_changed(before: &SymbolInst, after: &SymbolInst) -> bool {
    before.at != after.at || before.mirror != after.mirror
}

fn compare(before: &SchDoc, after: &SchDoc) -> Changes {
    let old = symbol_table(before);
    let new = symbol_table(after);
    let old_keys: BTreeSet<&String> = old.keys().collect();
    let new_keys: BTreeSet<&String> = new.keys().collect();
    let added = new_keys
        .difference(&old_keys)
        .map(|key| (*key).clone())
        .collect();
    let removed = old_keys
        .difference(&new_keys)
        .map(|key| (*key).clone())
        .collect();
    let mut moved = Vec::new();
    let mut fields_changed = Vec::new();
    let mut swapped = Vec::new();
    for key in old_keys.intersection(&new_keys) {
        let before_symbol = old[*key];
        let after_symbol = new[*key];
        if before_symbol.lib_id != after_symbol.lib_id {
            swapped.push(Swapped {
                reference: (*key).clone(),
                from: before_symbol.lib_id.clone(),
                to: after_symbol.lib_id.clone(),
            });
        } else if pose_changed(before_symbol, after_symbol) {
            moved.push(Moved {
                reference: (*key).clone(),
                from: pose(before_symbol),
                to: pose(after_symbol),
            });
        }
        let field_names: BTreeSet<&String> = before_symbol
            .fields
            .keys()
            .chain(after_symbol.fields.keys())
            .collect();
        for field in field_names {
            let from = before_symbol
                .fields
                .get(field)
                .map(|value| value.value.clone());
            let to = after_symbol
                .fields
                .get(field)
                .map(|value| value.value.clone());
            if from != to {
                fields_changed.push(FieldChanged {
                    reference: (*key).clone(),
                    field: field.clone(),
                    from,
                    to,
                });
            }
        }
    }
    let before_nets = connect::extract(before);
    let after_nets = connect::extract(after);
    let count = |from: usize, to: usize| {
        (from != to).then_some(CountChanged {
            from,
            to,
            delta: to as i64 - from as i64,
        })
    };
    Changes {
        added,
        removed,
        moved,
        fields_changed,
        swapped,
        net_delta: Netlist::diff(&before_nets, &after_nets),
        wires: count(before.wires().count(), after.wires().count()),
        labels: count(before.labels().count(), after.labels().count()),
    }
}

fn write_list(out: &mut String, title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    writeln!(out, "\n{title}").expect("writing to a string cannot fail");
    for line in lines {
        writeln!(out, "  {line}").expect("writing to a string cannot fail");
    }
}

fn quote_optional(value: &Option<String>) -> String {
    value
        .as_ref()
        .map_or_else(|| "<missing>".to_owned(), |value| format!("{value:?}"))
}

fn compact(revision: RevisionId, changes: &Changes) -> String {
    let mut out = format!("SCHEMATIC DIFF  revision {revision} → live\n");
    if changes.is_empty() {
        out.push_str("No changes.\n");
        return out;
    }
    write_list(&mut out, "ADDED", &changes.added);
    write_list(&mut out, "REMOVED", &changes.removed);
    let moved = changes
        .moved
        .iter()
        .map(|change| {
            format!(
                "{:<12}  @{:.3},{:.3} {:.0}° {} → @{:.3},{:.3} {:.0}° {}",
                change.reference,
                change.from.at[0],
                change.from.at[1],
                change.from.rot,
                change.from.mirror,
                change.to.at[0],
                change.to.at[1],
                change.to.rot,
                change.to.mirror,
            )
        })
        .collect::<Vec<_>>();
    write_list(&mut out, "MOVED", &moved);
    let fields = changes
        .fields_changed
        .iter()
        .map(|change| {
            format!(
                "{:<12}  {:<16}  {} → {}",
                change.reference,
                change.field,
                quote_optional(&change.from),
                quote_optional(&change.to)
            )
        })
        .collect::<Vec<_>>();
    write_list(&mut out, "FIELDS CHANGED", &fields);
    let swapped = changes
        .swapped
        .iter()
        .map(|change| format!("{:<12}  {} → {}", change.reference, change.from, change.to))
        .collect::<Vec<_>>();
    write_list(&mut out, "SWAPPED", &swapped);
    if !changes.net_delta.is_empty() {
        let delta = serde_json::to_string(&delta_json(&changes.net_delta))
            .expect("net delta is serializable");
        write_list(&mut out, "NET DELTA", &[delta]);
    }
    let counts = [
        ("wires", changes.wires.as_ref()),
        ("labels", changes.labels.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, count)| {
        count.map(|count| {
            format!(
                "{name:<12}  {} → {} ({:+})",
                count.from, count.to, count.delta
            )
        })
    })
    .collect::<Vec<_>>();
    write_list(&mut out, "COUNTS CHANGED", &counts);
    out
}

fn detailed(revision: RevisionId, changes: Changes) -> Value {
    json!({
        "revision": revision,
        "added": changes.added,
        "removed": changes.removed,
        "moved": changes.moved,
        "fields_changed": changes.fields_changed,
        "swapped": changes.swapped,
        "net_delta": delta_json(&changes.net_delta),
        "wires": changes.wires,
        "labels": changes.labels,
    })
}

/// Compare the live sheet with an explicit revision or the turn baseline.
pub fn diff_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let revision = match input.get("revision") {
        Some(value) => {
            let id = value
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or_else(|| anyhow!("revision must be a positive integer"))?;
            Some(RevisionId::new(id))
        }
        None => None,
    };
    let baseline = comparison_revision(ctx, revision)?
        .ok_or_else(|| anyhow!("no turn baseline yet; pass an explicit revision"))?;
    let before = baseline
        .path
        .as_deref()
        .map(SchDoc::read)
        .transpose()
        .context("reading comparison revision")?
        .map_or_else(sch_floorplan::live::blank_sheet, Ok)?;
    let after = SchDoc::read(ctx.sch_path()).context("reading live schematic")?;
    let changes = compare(&before, &after);
    if input.get("detail").and_then(Value::as_bool) == Some(true) {
        Ok(detailed(baseline.revision, changes))
    } else {
        Ok(Value::String(compact(baseline.revision, &changes)))
    }
}
