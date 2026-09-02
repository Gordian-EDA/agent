//! Incremental `.kicad_pcb` document edits: the board half of "Update PCB from
//! Schematic".
//!
//! [`patch`](crate::patch) moves and re-routes what a board already has;
//! this module changes *what the board contains* — footprints appear and
//! disappear, pads change net, values change — while every byte the edit does
//! not name stays exactly where it was. That is what makes a board editable
//! rather than reseedable: outline, zones, netclasses, silk and copper all
//! survive because nothing rewrites them.
//!
//! The caller supplies footprint text (the seed writer owns library →
//! `(footprint …)` synthesis); this module owns placement in the document and
//! the net table that footprint text refers to.

use std::collections::BTreeMap;

use geom::Point2;

use crate::patch::{apply_edits, child_nodes, line_start, node_head, quoted_field, root_body};

/// One footprint as the board file has it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardFootprint {
    pub reference: String,
    pub lib_id: String,
    pub value: String,
    pub at: Point2,
    pub rotation: f64,
    /// The copper layer the footprint sits on: `F.Cu` (front) or `B.Cu` (back).
    pub layer: String,
    /// The board file's `(locked yes)`: this part is not the placer's to move.
    pub locked: bool,
    /// Pad number → net name, for pads that carry a net. Pads with no number or
    /// no net are absent; pads that share a number share one entry.
    pub pad_nets: BTreeMap<String, String>,
}

impl BoardFootprint {
    /// Whether the footprint is mounted on the back copper layer.
    pub fn on_back(&self) -> bool {
        self.layer.eq_ignore_ascii_case("B.Cu")
    }
}

/// A `.kicad_pcb` document open for incremental edits.
///
/// Every mutator rescans the text, so the document is always self-consistent
/// and edits never invalidate one another's offsets.
#[derive(Debug, Clone)]
pub struct BoardDoc {
    text: String,
}

impl BoardDoc {
    /// Open a board document. Fails only when the text is not a `kicad_pcb`.
    pub fn parse(text: impl Into<String>) -> Result<Self, String> {
        let text = text.into();
        root_body(&text)?;
        Ok(Self { text })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }

    /// Every footprint in the document, in file order.
    pub fn footprints(&self) -> Vec<BoardFootprint> {
        let Ok((body_start, body_end)) = root_body(&self.text) else {
            return Vec::new();
        };
        child_nodes(&self.text, body_start, body_end)
            .into_iter()
            .filter(|node| node_head(&self.text, node) == "footprint")
            .filter_map(|node| self.read_footprint(node.start, node.end))
            .collect()
    }

    /// Whether the board's Edge.Cuts is the single rectangle the seed writer
    /// emits. A hand-drawn or polygon outline cannot be re-synthesized from the
    /// board file, so anything that would rebuild the document must refuse.
    pub fn outline_is_rectangular(&self) -> bool {
        let Ok((body_start, body_end)) = root_body(&self.text) else {
            return false;
        };
        let mut rects = 0usize;
        for node in child_nodes(&self.text, body_start, body_end) {
            let block = &self.text[node.start..node.end];
            if !block.contains("(layer \"Edge.Cuts\")") {
                continue;
            }
            match node_head(&self.text, &node) {
                "gr_rect" => rects += 1,
                _ => return false,
            }
        }
        rects == 1
    }

    /// References that appear on more than one footprint. A board cannot be
    /// synced part-by-part while two footprints answer to the same name.
    pub fn duplicate_references(&self) -> Vec<String> {
        let mut seen = BTreeMap::<String, usize>::new();
        for fp in self.footprints() {
            *seen.entry(fp.reference).or_default() += 1;
        }
        seen.into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(reference, _)| reference)
            .collect()
    }

    /// Net name → net code from the top-level `(net N "NAME")` table.
    pub fn net_codes(&self) -> BTreeMap<String, i32> {
        crate::patch::parse_net_codes(&self.text).unwrap_or_default()
    }

    /// Declare any of `names` the board does not have yet, and add each new one
    /// to the `Default` netclass so KiCAD gives it the board's own rules.
    /// Returns the full net table afterwards.
    pub fn ensure_nets<'a>(
        &mut self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeMap<String, i32>, String> {
        let mut codes = self.net_codes();
        let mut next = codes.values().copied().max().unwrap_or(0) + 1;
        let missing: Vec<&str> = names
            .into_iter()
            .filter(|name| !name.is_empty() && !codes.contains_key(*name))
            .collect();
        if missing.is_empty() {
            return Ok(codes);
        }
        let mut declarations = String::new();
        for name in &missing {
            declarations.push_str(&format!(
                "\t(net {next} \"{}\")\n",
                kicad::sexpr_escape(name)
            ));
            codes.insert((*name).to_string(), next);
            next += 1;
        }
        let anchor = self.last_top_level("net").ok_or("board has no net table")?;
        let insert_at = line_end(&self.text, anchor.1);
        self.text.insert_str(insert_at, &declarations);
        self.add_to_default_net_class(&missing);
        Ok(codes)
    }

    /// Insert a footprint block, verbatim, before the document's closing paren.
    pub fn insert_footprint(&mut self, block: &str) -> Result<(), String> {
        let (_, body_end) = root_body(&self.text)?;
        let at = line_start(&self.text, body_end);
        self.text.insert_str(at, block);
        Ok(())
    }

    /// Delete a footprint. Returns false when the board has no such reference.
    pub fn remove_footprint(&mut self, reference: &str) -> Result<bool, String> {
        let Some((start, end)) = self.footprint_span(reference)? else {
            return Ok(false);
        };
        self.text = apply_edits(
            &self.text,
            vec![(line_start(&self.text, start), end, String::new())],
        );
        Ok(true)
    }

    /// Point one pad at `net` (code and name must agree with the net table), or
    /// clear it with `None`. Returns false when the pad does not exist.
    /// A footprint may carry the pad number more than once (a thermal tab, a
    /// split pad); they are one electrical node, so they all move together.
    pub fn set_pad_net(
        &mut self,
        reference: &str,
        pad: &str,
        net: Option<(&str, i32)>,
    ) -> Result<bool, String> {
        let Some((fp_start, fp_end)) = self.footprint_span(reference)? else {
            return Ok(false);
        };
        let pads = self.pad_spans(fp_start, fp_end, pad);
        if pads.is_empty() {
            return Ok(false);
        }
        let replacement =
            net.map(|(name, code)| format!("(net {code} \"{}\")", kicad::sexpr_escape(name)));
        let mut edits = Vec::new();
        for (pad_start, pad_end) in pads {
            match (self.child_span(pad_start, pad_end, "net"), &replacement) {
                (Some((start, end)), Some(node)) => edits.push((start, end, node.clone())),
                (Some((start, end)), None) => {
                    edits.push((line_start(&self.text, start), end, String::new()))
                }
                (None, Some(node)) => {
                    let close = line_start(&self.text, pad_end - 1);
                    let indent = child_indent(&self.text, pad_start);
                    edits.push((close, close, format!("{indent}{node}\n")));
                }
                (None, None) => {}
            }
        }
        self.text = apply_edits(&self.text, edits);
        Ok(true)
    }

    /// Rewrite a footprint's `Value` property. Returns false when the board has
    /// no such reference or the footprint carries no Value field.
    pub fn set_value(&mut self, reference: &str, value: &str) -> Result<bool, String> {
        let Some((fp_start, fp_end)) = self.footprint_span(reference)? else {
            return Ok(false);
        };
        let key = "(property \"Value\" \"";
        let body = &self.text[fp_start..fp_end];
        let Some(at) = body.find(key) else {
            return Ok(false);
        };
        let value_start = fp_start + at + key.len();
        let Some(len) = quoted_len(&self.text[value_start..]) else {
            return Ok(false);
        };
        self.text = apply_edits(
            &self.text,
            vec![(
                value_start,
                value_start + len,
                kicad::sexpr_escape(value).to_string(),
            )],
        );
        Ok(true)
    }

    // ── scanning ────────────────────────────────────────────────────────────

    fn read_footprint(&self, start: usize, end: usize) -> Option<BoardFootprint> {
        let block = &self.text[start..end];
        let lib_id = quoted_field(block, "footprint")?.to_string();
        let reference = property(block, "Reference")?;
        let layer = self
            .child_span(start, end, "layer")
            .and_then(|(s, e)| first_quoted(&self.text[s..e]))
            .unwrap_or_else(|| "F.Cu".to_owned());
        let locked = self
            .child_span(start, end, "locked")
            .is_some_and(|(s, e)| self.text[s..e].contains("yes"));
        let (at, rotation) = self
            .child_span(start, end, "at")
            .and_then(|(s, e)| parse_at(&self.text[s..e]))?;
        let mut pad_nets = BTreeMap::new();
        for (pad_start, pad_end) in self.children(start, end, "pad") {
            let pad = &self.text[pad_start..pad_end];
            let (Some(number), Some((_, name))) =
                (first_quoted(pad), self.pad_net(pad_start, pad_end))
            else {
                continue;
            };
            if number.is_empty() || name.is_empty() {
                continue;
            }
            pad_nets.insert(number, name);
        }
        Some(BoardFootprint {
            reference,
            lib_id,
            value: property(block, "Value").unwrap_or_default(),
            at,
            rotation,
            layer,
            locked,
            pad_nets,
        })
    }

    fn pad_net(&self, start: usize, end: usize) -> Option<(i32, String)> {
        let (net_start, net_end) = self.child_span(start, end, "net")?;
        let inner = &self.text[net_start + 1..net_end - 1];
        let code = inner.split_whitespace().nth(1)?.parse().ok()?;
        Some((code, first_quoted(inner)?))
    }

    fn footprint_span(&self, reference: &str) -> Result<Option<(usize, usize)>, String> {
        let (body_start, body_end) = root_body(&self.text)?;
        Ok(child_nodes(&self.text, body_start, body_end)
            .into_iter()
            .filter(|node| node_head(&self.text, node) == "footprint")
            .find(|node| {
                property(&self.text[node.start..node.end], "Reference").as_deref()
                    == Some(reference)
            })
            .map(|node| (node.start, node.end)))
    }

    fn pad_spans(&self, fp_start: usize, fp_end: usize, pad: &str) -> Vec<(usize, usize)> {
        self.children(fp_start, fp_end, "pad")
            .into_iter()
            .filter(|(start, end)| first_quoted(&self.text[*start..*end]).as_deref() == Some(pad))
            .collect()
    }

    /// Depth-1 children of the node spanning `[start, end)` whose head is `head`.
    fn children(&self, start: usize, end: usize, head: &str) -> Vec<(usize, usize)> {
        child_nodes(&self.text, start + 1, end - 1)
            .into_iter()
            .filter(|node| node_head(&self.text, node) == head)
            .map(|node| (node.start, node.end))
            .collect()
    }

    fn child_span(&self, start: usize, end: usize, head: &str) -> Option<(usize, usize)> {
        self.children(start, end, head).into_iter().next()
    }

    fn last_top_level(&self, head: &str) -> Option<(usize, usize)> {
        let (body_start, body_end) = root_body(&self.text).ok()?;
        child_nodes(&self.text, body_start, body_end)
            .into_iter()
            .filter(|node| node_head(&self.text, node) == head)
            .map(|node| (node.start, node.end))
            .next_back()
    }

    /// Give new nets the board's default routing rules, the way the seed writer
    /// would have. Boards without a `Default` netclass need nothing.
    fn add_to_default_net_class(&mut self, nets: &[&str]) {
        let Ok((body_start, body_end)) = root_body(&self.text) else {
            return;
        };
        let Some(class) = child_nodes(&self.text, body_start, body_end)
            .into_iter()
            .filter(|node| node_head(&self.text, node) == "net_class")
            .find(|node| {
                first_quoted(&self.text[node.start..node.end]).as_deref() == Some("Default")
            })
        else {
            return;
        };
        let indent = child_indent(&self.text, class.start);
        let mut added = String::new();
        for net in nets {
            added.push_str(&format!(
                "{indent}(add_net \"{}\")\n",
                kicad::sexpr_escape(net)
            ));
        }
        let close = line_start(&self.text, class.end - 1);
        self.text = apply_edits(&self.text, vec![(close, close, added)]);
    }
}

/// `(property "NAME" "value" …)` inside a block.
fn property(block: &str, name: &str) -> Option<String> {
    let key = format!("(property \"{name}\" \"");
    let rest = block.split_once(&key)?.1;
    let len = quoted_len(rest)?;
    Some(unescape(&rest[..len]))
}

/// Length of the string literal starting at the head of `rest` (which sits just
/// past its opening quote), honouring backslash escapes.
fn quoted_len(rest: &str) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(idx);
        }
    }
    None
}

/// Inverse of [`kicad::sexpr_escape`].
fn unescape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    let mut chars = literal.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The first string literal in a node: `(pad "1" …)` → `1`.
fn first_quoted(node: &str) -> Option<String> {
    let rest = node.split_once('"')?.1;
    Some(unescape(&rest[..quoted_len(rest)?]))
}

fn parse_at(node: &str) -> Option<(Point2, f64)> {
    let mut values = node
        .trim_matches(['(', ')'])
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse::<f64>().ok());
    let point = Point2::new(values.next()?, values.next()?);
    Some((point, values.next().unwrap_or(0.0)))
}

/// The whitespace prefix one level inside the node beginning at `start`.
fn child_indent(text: &str, start: usize) -> String {
    let own = tabs_of(&line_indent(text, start));
    format!("{own}\t")
}

fn line_indent(text: &str, pos: usize) -> String {
    let line = text[..pos].rfind('\n').map_or(0, |nl| nl + 1);
    text[line..pos].to_string()
}

fn tabs_of(indent: &str) -> String {
    indent.strip_suffix('\t').unwrap_or(indent).to_string()
}

fn line_end(text: &str, pos: usize) -> usize {
    text[pos..].find('\n').map_or(text.len(), |nl| pos + nl + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = "(kicad_pcb\n\t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"VIN\")\n\
        \t(net_class \"Default\" \"default board routing rules\"\n\t\t(clearance 0.15)\n\
        \t\t(add_net \"GND\")\n\t\t(add_net \"VIN\")\n\t)\n\
        \t(footprint \"Resistor_SMD:R_0805_2012Metric\"\n\t\t(layer \"F.Cu\")\n\t\t(at 10 20 90)\n\
        \t\t(property \"Reference\" \"R1\"\n\t\t\t(at 0 0 0)\n\t\t)\n\
        \t\t(property \"Value\" \"10k\"\n\t\t\t(at 0 0 0)\n\t\t)\n\
        \t\t(pad \"1\" smd roundrect\n\t\t\t(at -1 0 90)\n\t\t\t(net 2 \"VIN\")\n\t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\t\t\t(at 1 0 90)\n\t\t\t(net 1 \"GND\")\n\t\t)\n\t)\n)\n";

    fn doc() -> BoardDoc {
        BoardDoc::parse(BOARD).unwrap()
    }

    #[test]
    fn reads_footprints_with_pads_and_placement() {
        let parts = doc().footprints();
        assert_eq!(parts.len(), 1);
        let r1 = &parts[0];
        assert_eq!(r1.reference, "R1");
        assert_eq!(r1.lib_id, "Resistor_SMD:R_0805_2012Metric");
        assert_eq!(r1.value, "10k");
        assert_eq!((r1.at.x, r1.at.y, r1.rotation), (10.0, 20.0, 90.0));
        assert_eq!(
            r1.pad_nets,
            BTreeMap::from([("1".into(), "VIN".into()), ("2".into(), "GND".into())])
        );
    }

    #[test]
    fn new_nets_get_codes_and_default_class_membership() {
        let mut doc = doc();
        let codes = doc.ensure_nets(["GND", "SENSE"]).unwrap();
        assert_eq!(codes["GND"], 1);
        assert_eq!(codes["SENSE"], 3);
        assert!(doc.text().contains("(net 3 \"SENSE\")"));
        assert_eq!(doc.text().matches("(add_net \"SENSE\")").count(), 1);
        // Idempotent: a second call declares nothing.
        let again = doc.clone();
        doc.ensure_nets(["SENSE"]).unwrap();
        assert_eq!(doc.text(), again.text());
    }

    #[test]
    fn pad_nets_retarget_and_clear() {
        let mut doc = doc();
        let codes = doc.ensure_nets(["SENSE"]).unwrap();
        assert!(
            doc.set_pad_net("R1", "2", Some(("SENSE", codes["SENSE"])))
                .unwrap()
        );
        assert_eq!(doc.footprints()[0].pad_nets["2"], "SENSE");
        assert!(doc.set_pad_net("R1", "1", None).unwrap());
        assert!(!doc.footprints()[0].pad_nets.contains_key("1"));
        assert!(!doc.set_pad_net("R9", "1", None).unwrap());
        assert!(BoardDoc::parse(doc.into_text()).is_ok());
    }

    #[test]
    fn a_pad_without_a_net_node_gains_one() {
        let bare = BOARD.replace("\n\t\t\t(net 2 \"VIN\")", "");
        let mut doc = BoardDoc::parse(bare).unwrap();
        assert!(doc.set_pad_net("R1", "1", Some(("VIN", 2))).unwrap());
        assert_eq!(doc.footprints()[0].pad_nets["1"], "VIN");
        assert!(BoardDoc::parse(doc.into_text()).is_ok());
    }

    #[test]
    fn value_edits_touch_nothing_else() {
        let mut doc = doc();
        assert!(doc.set_value("R1", "4.7k").unwrap());
        assert_eq!(doc.footprints()[0].value, "4.7k");
        assert_eq!(doc.footprints()[0].at, Point2::new(10.0, 20.0));
        assert_eq!(doc.text().len(), BOARD.len() + 1);
    }

    #[test]
    fn a_footprint_carries_its_side_and_its_lock() {
        let back = BOARD.replace(
            "\t\t(layer \"F.Cu\")",
            "\t\t(locked yes)\n\t\t(layer \"B.Cu\")",
        );
        let fp = BoardDoc::parse(back).unwrap().footprints().remove(0);
        assert!(fp.on_back());
        assert!(fp.locked);
        let front = doc().footprints().remove(0);
        assert!(!front.on_back() && !front.locked);
    }

    #[test]
    fn a_repeated_pad_number_retargets_every_pad_that_carries_it() {
        // A thermal tab sharing pad 2 is one electrical node; both must move.
        let tab = BOARD.replace(
            "\t\t(pad \"2\" smd roundrect\n\t\t\t(at 1 0 90)\n\t\t\t(net 1 \"GND\")\n\t\t)\n",
            "\t\t(pad \"2\" smd roundrect\n\t\t\t(at 1 0 90)\n\t\t\t(net 1 \"GND\")\n\t\t)\n\
             \t\t(pad \"2\" smd roundrect\n\t\t\t(at 1 2 90)\n\t\t\t(net 1 \"GND\")\n\t\t)\n",
        );
        let mut doc = BoardDoc::parse(tab).unwrap();
        let codes = doc.ensure_nets(["SENSE"]).unwrap();
        assert!(
            doc.set_pad_net("R1", "2", Some(("SENSE", codes["SENSE"])))
                .unwrap()
        );
        // Two pads plus the one top-level declaration.
        assert_eq!(doc.text().matches("(net 3 \"SENSE\")").count(), 3);
        // GND keeps only its declaration; no pad points at it any more.
        assert_eq!(doc.text().matches("(net 1 \"GND\")").count(), 1);
    }

    #[test]
    fn a_string_ending_in_an_escaped_backslash_does_not_desync_the_scan() {
        let tricky = BOARD.replace("\"10k\"", "\"10k\\\\\"");
        let parts = BoardDoc::parse(tricky).unwrap().footprints();
        assert_eq!(parts.len(), 1, "the scan must not swallow the document");
        assert_eq!(parts[0].reference, "R1");
    }

    #[test]
    fn a_rectangular_outline_is_recognised_and_a_drawn_one_is_not() {
        let rect = BOARD.replace(
            "(kicad_pcb\n",
            "(kicad_pcb\n\t(gr_rect\n\t\t(start 0 0)\n\t\t(end 10 10)\n\t\t(layer \"Edge.Cuts\")\n\t)\n",
        );
        assert!(BoardDoc::parse(rect).unwrap().outline_is_rectangular());
        let drawn = BOARD.replace(
            "(kicad_pcb\n",
            "(kicad_pcb\n\t(gr_line\n\t\t(start 0 0)\n\t\t(end 10 0)\n\t\t(layer \"Edge.Cuts\")\n\t)\n",
        );
        assert!(!BoardDoc::parse(drawn).unwrap().outline_is_rectangular());
    }

    #[test]
    fn footprints_come_and_go() {
        let mut doc = doc();
        assert!(doc.remove_footprint("R1").unwrap());
        assert!(doc.footprints().is_empty());
        assert!(!doc.remove_footprint("R1").unwrap());
        doc.insert_footprint(
            "\t(footprint \"Capacitor_SMD:C_0603\"\n\t\t(at 1 2)\n\
             \t\t(property \"Reference\" \"C9\"\n\t\t)\n\t)\n",
        )
        .unwrap();
        let parts = doc.footprints();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].reference, "C9");
        assert!(BoardDoc::parse(doc.into_text()).is_ok());
    }
}
