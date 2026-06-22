//! Thin adapter over saphyr: converts its marked tree into our `Node`.
//! The ONLY module allowed to import saphyr. A standalone copy of
//! circuit-lang's loader (kept duplicated so the board DSL is decoupled).
//!
//! We load with scalar *representation* preservation (`early_parse(false)`)
//! so every scalar surfaces as its literal source string — `0.8`, `GND`,
//! `1`, `true` all come out as those exact strings (YAML 1.2 / Norway-problem
//! safe). saphyr's default loader would otherwise resolve scalars to typed
//! `Scalar`s and lose the literal form.

use crate::diag::{Diagnostic, Diagnostics, Span};
use saphyr::{MarkedYaml, Scalar, ScalarStyle, YamlData};
use saphyr_parser::{
    Event, Parser, ScalarStyle as PScalarStyle, Span as PSpan, SpannedEventReceiver,
};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub enum Node {
    /// All scalars are surfaced as their literal string form.
    Scalar(String, Span),
    Seq(Vec<Node>, Span),
    /// Key order preserved; keys are scalars-as-strings with spans.
    Map(Vec<((String, Span), Node)>, Span),
    Null(Span),
}

impl Node {
    pub fn span(&self) -> Span {
        match self {
            Node::Scalar(_, s) | Node::Seq(_, s) | Node::Map(_, s) | Node::Null(s) => *s,
        }
    }
}

pub fn load(src: &str) -> Result<(Node, Diagnostics), Diagnostics> {
    let mut parser = Parser::new_from_str(src);
    let mut loader = saphyr::YamlLoader::<MarkedYaml>::default();
    loader.early_parse(false);
    parser.load(&mut loader, true).map_err(|e| {
        let mut ds = Diagnostics::default();
        ds.push(Diagnostic::error("yaml-syntax", e.to_string()));
        ds
    })?;
    let doc = loader.into_documents().into_iter().next().ok_or_else(|| {
        let mut ds = Diagnostics::default();
        ds.push(Diagnostic::error("yaml-syntax", "empty document"));
        ds
    })?;
    let mut diags = Diagnostics::default();
    scan_structure(src, &mut diags);
    Ok((convert(doc), diags))
}

/// Re-drives the raw event stream to detect duplicate map keys (which the
/// `MarkedYaml` tree silently dedups) and reject multiple YAML documents.
fn scan_structure(src: &str, diags: &mut Diagnostics) {
    let mut recv = StructureScanner::new(diags);
    let mut parser = Parser::new_from_str(src);
    let _ = parser.load(&mut recv, true);
}

struct StructureScanner<'a> {
    diags: &'a mut Diagnostics,
    map_stack: Vec<MapFrame>,
    seq_depth: Vec<usize>,
    doc_count: usize,
    extra_doc_reported: bool,
}

struct MapFrame {
    seen: HashSet<String>,
    expecting_key: bool,
}

impl<'a> StructureScanner<'a> {
    fn new(diags: &'a mut Diagnostics) -> Self {
        Self {
            diags,
            map_stack: Vec::new(),
            seq_depth: Vec::new(),
            doc_count: 0,
            extra_doc_reported: false,
        }
    }

    fn at_key_position(&self) -> bool {
        match (self.map_stack.last(), self.seq_depth.last()) {
            (Some(frame), Some(depth)) => frame.expecting_key && *depth == 0,
            _ => false,
        }
    }

    fn advance_after_node(&mut self) {
        if let Some(depth) = self.seq_depth.last()
            && *depth == 0
            && let Some(frame) = self.map_stack.last_mut()
        {
            frame.expecting_key = !frame.expecting_key;
        }
    }
}

fn pspan_to_span(span: PSpan) -> Span {
    Span {
        line: span.start.line(),
        col: span.start.col(),
    }
}

impl<'input> SpannedEventReceiver<'input> for StructureScanner<'_> {
    fn on_event(&mut self, ev: Event<'input>, span: PSpan) {
        match ev {
            Event::DocumentStart(_) => {
                self.doc_count += 1;
                if self.doc_count > 1 && !self.extra_doc_reported {
                    self.extra_doc_reported = true;
                    self.diags.push(Diagnostic::error(
                        "multiple-documents",
                        "only a single YAML document is supported; extra document(s) ignored",
                    ));
                }
            }
            Event::Scalar(val, style, _, _) => {
                if self.at_key_position() {
                    let key = if matches!(style, PScalarStyle::Plain) && val.is_empty() {
                        "~".to_string()
                    } else {
                        val.into_owned()
                    };
                    if let Some(frame) = self.map_stack.last_mut()
                        && !frame.seen.insert(key.clone())
                    {
                        self.diags.push(
                            Diagnostic::error("duplicate-key", format!("duplicate key `{key}`"))
                                .with_span(pspan_to_span(span)),
                        );
                    }
                }
                self.advance_after_node();
            }
            Event::Alias(_) => {
                self.advance_after_node();
            }
            Event::MappingStart(_, _) => {
                self.map_stack.push(MapFrame {
                    seen: HashSet::new(),
                    expecting_key: true,
                });
                self.seq_depth.push(0);
            }
            Event::MappingEnd => {
                self.map_stack.pop();
                self.seq_depth.pop();
                self.advance_after_node();
            }
            Event::SequenceStart(_, _) => {
                if let Some(depth) = self.seq_depth.last_mut() {
                    *depth += 1;
                }
            }
            Event::SequenceEnd => {
                let mut closed_value = false;
                if let Some(depth) = self.seq_depth.last_mut() {
                    *depth -= 1;
                    if *depth == 0 {
                        closed_value = true;
                    }
                }
                if closed_value {
                    self.advance_after_node();
                }
            }
            _ => {}
        }
    }
}

fn mark_span(node: &MarkedYaml) -> Span {
    Span {
        line: node.span.start.line(),
        col: node.span.start.col() + 1,
    }
}

fn is_null<'a>(data: &YamlData<'a, MarkedYaml<'a>>) -> bool {
    match data {
        YamlData::Representation(s, ScalarStyle::Plain, _) => {
            matches!(s.as_ref(), "" | "~" | "null" | "Null" | "NULL")
        }
        YamlData::Value(Scalar::Null) => true,
        _ => false,
    }
}

fn scalar_string<'a>(data: &YamlData<'a, MarkedYaml<'a>>) -> Option<String> {
    match data {
        YamlData::Representation(s, _, _) => Some(s.to_string()),
        YamlData::Value(s) => match s {
            Scalar::String(s) => Some(s.to_string()),
            Scalar::Integer(i) => Some(i.to_string()),
            Scalar::FloatingPoint(f) => Some(f.into_inner().to_string()),
            Scalar::Boolean(b) => Some(b.to_string()),
            Scalar::Null => None,
        },
        _ => None,
    }
}

fn convert(node: MarkedYaml) -> Node {
    let span = mark_span(&node);
    match node.data {
        YamlData::Mapping(m) => {
            let mut entries = Vec::new();
            for (k, v) in m {
                let kspan = mark_span(&k);
                let key = scalar_string(&k.data).unwrap_or_default();
                entries.push(((key, kspan), convert(v)));
            }
            Node::Map(entries, span)
        }
        YamlData::Sequence(s) => Node::Seq(s.into_iter().map(convert).collect(), span),
        ref d if is_null(d) => Node::Null(span),
        ref d => match scalar_string(d) {
            Some(s) => Node::Scalar(s, span),
            None => Node::Null(span),
        },
    }
}
