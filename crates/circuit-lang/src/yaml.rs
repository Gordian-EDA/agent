//! Thin adapter over saphyr: converts its marked tree into our `Node`.
//! The ONLY module allowed to import saphyr.
//!
//! We load with scalar *representation* preservation (`early_parse(false)`)
//! so every scalar surfaces as its literal source string — `4.7k`, `NO`,
//! `1`, `true` all come out as those exact strings (YAML 1.2 / Norway-problem
//! safe, spec §5.3.7). saphyr's default loader would otherwise resolve
//! scalars to typed `Scalar`s and lose the literal form.

use crate::diag::{Diagnostic, Diagnostics, Span};
use saphyr::{MarkedYaml, Scalar, ScalarStyle, YamlData};
use saphyr_parser::Parser;

#[derive(Debug, Clone)]
pub enum Node {
    /// All scalars are surfaced as their literal string form (pin
    /// numbers, values like `4.7k`, `NO` — spec §5.3.7).
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

pub fn load(src: &str) -> Result<Node, Diagnostics> {
    // Load with representation preservation so scalars keep their literal
    // source string instead of being resolved to typed `Scalar`s.
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
    Ok(convert(doc))
}

fn mark_span(node: &MarkedYaml) -> Span {
    // saphyr markers are 1-based line and 0-based col; `Span` is 1-based.
    Span {
        line: node.span.start.line(),
        col: node.span.start.col() + 1,
    }
}

/// True if `data` is a YAML null per the 1.2 core schema: an *unquoted*
/// (plain-style) scalar that is empty or one of `~`, `null`, `Null`, `NULL`.
/// With `early_parse(false)` an empty/`null`/`~` value surfaces as a plain
/// `Representation` (empty normalizes to `~`), so without this check it would
/// silently become the literal string `"~"`/`"null"` and pass scalar checks.
/// Quoted `"null"`/`'null'` remain genuine strings.
fn is_null<'a>(data: &YamlData<'a, MarkedYaml<'a>>) -> bool {
    match data {
        YamlData::Representation(s, ScalarStyle::Plain, _) => {
            matches!(s.as_ref(), "" | "~" | "null" | "Null" | "NULL")
        }
        YamlData::Value(Scalar::Null) => true,
        _ => false,
    }
}

/// Returns the literal source string of a scalar node, or `None` for
/// collections / null / bad values.
fn scalar_string<'a>(data: &YamlData<'a, MarkedYaml<'a>>) -> Option<String> {
    match data {
        // `early_parse(false)` keeps scalars as their raw representation.
        YamlData::Representation(s, _, _) => Some(s.to_string()),
        // Fallback for any resolved scalar (e.g. should not normally occur
        // with representation preservation, but keeps the literal form).
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
