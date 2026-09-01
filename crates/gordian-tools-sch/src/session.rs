//! The editing transaction every schematic mutator runs inside.
//!
//! An [`Edit`] opens the project's live `.kicad_sch`, records its connectivity,
//! and hands the caller the document. On [`Edit::commit`] it re-extracts, diffs,
//! and only writes when the change to the net partition is one the call
//! *named*. Anything else is rolled back and reported — a tool may never
//! silently rewire the board while doing something else.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gordian_runtime::AgentRuntime;
use sch_doc::{NetDelta, Netlist, SchDoc, SnapshotId, SymbolSource, connect};
use serde_json::{Value, json};

/// The nets and parts a call declared it was about to touch.
#[derive(Debug, Default, Clone)]
pub(crate) struct Allow {
    nets: BTreeSet<String>,
    refs: BTreeSet<String>,
    /// The call may bring nets into existence it could not name in advance —
    /// a new part's hidden power pin, or a wire that names its own net.
    creating: bool,
}

impl Allow {
    /// Nothing may change: the call claims to be electrically inert.
    pub fn nothing() -> Allow {
        Allow::default()
    }

    pub fn net(mut self, name: impl Into<String>) -> Allow {
        self.nets.insert(name.into());
        self
    }

    pub fn nets<I: Into<String>>(mut self, names: impl IntoIterator<Item = I>) -> Allow {
        self.nets.extend(names.into_iter().map(Into::into));
        self
    }

    pub fn part(mut self, refdes: impl Into<String>) -> Allow {
        self.refs.insert(refdes.into());
        self
    }

    pub fn parts<I: Into<String>>(mut self, refs: impl IntoIterator<Item = I>) -> Allow {
        self.refs.extend(refs.into_iter().map(Into::into));
        self
    }

    /// Permit nets this call could not name in advance to appear.
    pub fn creating(mut self) -> Allow {
        self.creating = true;
        self
    }

    /// The first change the call did not account for, if any.
    fn violation(&self, delta: &NetDelta) -> Option<String> {
        let unnamed = |name: &String| !self.nets.contains(name);
        let mut offenders: BTreeSet<String> = BTreeSet::new();
        if !self.creating {
            offenders.extend(delta.created.iter().filter(|n| unnamed(n)).cloned());
        }
        offenders.extend(delta.removed.iter().filter(|n| unnamed(n)).cloned());
        for (from, to) in &delta.renamed {
            // KiCAD names an unnamed net after its strongest pin, so joining a
            // pin to one re-derives that name. The partition did not change,
            // and no name the design authored did either.
            if is_auto(from) && is_auto(to) {
                continue;
            }
            offenders.extend([from, to].into_iter().filter(|n| unnamed(n)).cloned());
        }
        // On a merge or a split the *authored* names are what matter; the
        // generated name the survivor ends up with is a consequence.
        let unauthored = |name: &String| unnamed(name) && !is_auto(name);
        for (sources, target) in &delta.merged {
            offenders.extend(sources.iter().filter(|n| unnamed(n)).cloned());
            offenders.extend([target].into_iter().filter(|n| unauthored(n)).cloned());
        }
        for (source, targets) in &delta.split {
            offenders.extend([source].into_iter().filter(|n| unnamed(n)).cloned());
            offenders.extend(targets.iter().filter(|n| unauthored(n)).cloned());
        }
        for pin in delta
            .pins_now_connected
            .iter()
            .chain(&delta.pins_now_unconnected)
        {
            if !self.refs.contains(&pin.refdes) {
                offenders.insert(format!("{}.{}", pin.refdes, pin.pin));
            }
        }
        let listed: Vec<String> = offenders.into_iter().collect();
        (!listed.is_empty()).then(|| listed.join(", "))
    }
}

/// Whether a net name was generated rather than authored.
fn is_auto(name: &str) -> bool {
    name.starts_with("Net-(")
}

/// Where rolled-back-able copies of the schematic live, one per committed edit.
fn undo_dir(ctx: &AgentRuntime) -> PathBuf {
    ctx.project_dir().join(".gordian").join("sch-undo")
}

/// An open, guarded edit of the project schematic.
pub(crate) struct Edit {
    path: PathBuf,
    undo_dir: PathBuf,
    original: String,
    before: Netlist,
    rollback: SnapshotId,
    pub doc: SchDoc,
    pub warnings: Vec<String>,
}

impl Edit {
    /// Open the project schematic for editing.
    pub fn open(ctx: &AgentRuntime) -> Result<Edit> {
        let path = ctx.sch_path().to_path_buf();
        let original = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut doc = SchDoc::parse(&original)
            .with_context(|| format!("parsing {}", path.display()))?;
        let before = connect::extract(&doc);
        let rollback = doc.snapshot();
        Ok(Edit {
            path,
            undo_dir: undo_dir(ctx),
            original,
            warnings: before.warnings.clone(),
            before,
            rollback,
            doc,
        })
    }

    /// Read the schematic without intending to change it.
    pub fn read(ctx: &AgentRuntime) -> Result<(SchDoc, Netlist)> {
        let edit = Edit::open(ctx)?;
        Ok((edit.doc, edit.before))
    }

    pub fn before(&self) -> &Netlist {
        &self.before
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// Write the edit if its net delta stays within `allow`, else restore and
    /// report what it would have done.
    pub fn commit(mut self, changed: Value, allow: Allow) -> Result<Value> {
        let after = connect::extract(&self.doc);
        let delta = Netlist::diff(&self.before, &after);
        if let Some(offenders) = allow.violation(&delta) {
            self.doc.restore(self.rollback)?;
            return Ok(json!({
                "error": format!(
                    "refused: the edit would change connectivity the call did not name ({offenders}); \
                     nothing was written"
                ),
                "net_delta": delta_json(&delta),
            }));
        }
        let snapshot = self.stash()?;
        self.doc
            .write(&self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(json!({
            "changed": changed,
            "net_delta": delta_json(&delta),
            "warnings": self.warnings,
            "snapshot": snapshot,
        }))
    }

    /// Save the pre-edit text under a fresh id so `undo` can come back to it.
    fn stash(&self) -> Result<String> {
        std::fs::create_dir_all(&self.undo_dir)
            .with_context(|| format!("creating {}", self.undo_dir.display()))?;
        let next = 1 + std::fs::read_dir(&self.undo_dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| stash_index(&entry.path()))
            .max()
            .unwrap_or(0);
        let id = format!("sch-{next}");
        std::fs::write(self.undo_dir.join(format!("{id}.kicad_sch")), &self.original)?;
        Ok(id)
    }
}

/// The ordinal in a `sch-<n>.kicad_sch` stash filename.
fn stash_index(path: &Path) -> Option<u32> {
    path.file_stem()?.to_str()?.strip_prefix("sch-")?.parse().ok()
}

/// Restore the schematic to a state a previous mutator stashed.
pub fn undo(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(id) = input.get("snapshot").and_then(Value::as_str) else {
        return Ok(json!({ "error": "undo needs the `snapshot` id a mutator returned" }));
    };
    let stash = undo_dir(ctx).join(format!("{id}.kicad_sch"));
    if !stash.is_file() {
        return Ok(json!({ "error": format!("no snapshot `{id}`") }));
    }
    let before = Edit::open(ctx).map(|e| e.before).unwrap_or_default();
    std::fs::copy(&stash, ctx.sch_path())?;
    let after = Edit::open(ctx)?.before;
    Ok(json!({
        "changed": format!("restored the schematic to snapshot {id}"),
        "net_delta": delta_json(&Netlist::diff(&before, &after)),
    }))
}

/// Where new symbol definitions come from.
pub(crate) fn symbol_source(ctx: &AgentRuntime) -> SymbolSource {
    SymbolSource::new(ctx.env().symbol_dir().to_path_buf())
}

/// The net delta as the model reads it: only the parts that are non-empty.
pub(crate) fn delta_json(delta: &NetDelta) -> Value {
    let pins = |list: &[sch_doc::PinRef]| -> Vec<String> {
        list.iter()
            .map(|p| format!("{}.{}", p.refdes, p.pin))
            .collect()
    };
    let mut out = serde_json::Map::new();
    let mut put = |key: &str, value: Value| {
        let empty = value.as_array().is_some_and(|a| a.is_empty());
        if !empty {
            out.insert(key.to_string(), value);
        }
    };
    put("created", json!(delta.created));
    put("removed", json!(delta.removed));
    put("renamed", json!(delta.renamed));
    put("merged", json!(delta.merged));
    put("split", json!(delta.split));
    put("now_connected", json!(pins(&delta.pins_now_connected)));
    put("now_unconnected", json!(pins(&delta.pins_now_unconnected)));
    if out.is_empty() {
        return json!("connectivity unchanged");
    }
    Value::Object(out)
}
