//! Strict walker: yaml::Node -> SurfaceDesign. Unknown keys are errors
//! with did-you-mean suggestions (spec §5.3.6).

use crate::diag::{Diagnostic, Diagnostics, Span};
use crate::surface::*;
use crate::yaml::{self, Node};
use indexmap::IndexMap;

pub fn parse_str(src: &str) -> (Option<SurfaceDesign>, Diagnostics) {
    let mut diags = Diagnostics::default();
    let root = match yaml::load(src) {
        Ok((n, ds)) => {
            diags.extend(ds);
            n
        }
        Err(ds) => return (None, ds),
    };
    let mut p = Parser { diags: &mut diags };
    let design = p.design(&root);
    (design, diags)
}

struct Parser<'a> {
    diags: &'a mut Diagnostics,
}

fn suggest(key: &str, allowed: &[&str]) -> Option<String> {
    allowed
        .iter()
        .map(|a| (strsim::levenshtein(key, a), *a))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, a)| a.to_string())
}

/// Strict `[A-Z]+[0-9]+`: an uppercase-letter prefix followed by a digit
/// suffix, with nothing interleaved (e.g. `U1`, `R10`). Shared by the
/// refdes validator here and the pin-ref detector in `desugar`.
pub fn looks_like_refdes(s: &str) -> bool {
    match s.find(|c: char| c.is_ascii_digit()) {
        Some(i) if i > 0 => {
            let (prefix, suffix) = s.split_at(i);
            prefix.chars().all(|c| c.is_ascii_uppercase())
                && suffix.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

impl Parser<'_> {
    fn err(&mut self, code: &'static str, msg: String, span: Span) {
        self.diags
            .push(Diagnostic::error(code, msg).with_span(span));
    }

    /// Strict map access: every key must be in `allowed`.
    fn check_keys(&mut self, map: &[((String, Span), Node)], allowed: &[&str], ctx: &str) {
        for ((k, kspan), _) in map {
            if !allowed.contains(&k.as_str()) {
                let mut d = Diagnostic::error("unknown-key", format!("unknown key `{k}` in {ctx}"))
                    .with_span(*kspan);
                if let Some(s) = suggest(k, allowed) {
                    d = d.with_suggestion(s);
                }
                self.diags.push(d);
            }
        }
    }

    fn get<'n>(map: &'n [((String, Span), Node)], key: &str) -> Option<&'n Node> {
        map.iter().find(|((k, _), _)| k == key).map(|(_, v)| v)
    }

    fn scalar(&mut self, n: &Node, ctx: &str) -> Option<String> {
        match n {
            Node::Scalar(s, _) => Some(s.clone()),
            _ => {
                self.err(
                    "expected-scalar",
                    format!("expected a scalar for {ctx}"),
                    n.span(),
                );
                None
            }
        }
    }

    /// Strict boolean: only the literals `true`/`false` are accepted;
    /// anything else (e.g. `ture`, `yes`) is a diagnostic, not silent `false`.
    fn bool_field(&mut self, n: &Node, ctx: &str) -> Option<bool> {
        let s = self.scalar(n, ctx)?;
        match s.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            other => {
                self.err(
                    "bad-bool",
                    format!("expected `true` or `false` for {ctx}, found `{other}`"),
                    n.span(),
                );
                None
            }
        }
    }

    fn map_node<'n>(&mut self, n: &'n Node, ctx: &str) -> Option<&'n [((String, Span), Node)]> {
        match n {
            Node::Map(m, _) => Some(m),
            _ => {
                self.err(
                    "expected-map",
                    format!("expected a mapping for {ctx}"),
                    n.span(),
                );
                None
            }
        }
    }

    fn design(&mut self, root: &Node) -> Option<SurfaceDesign> {
        let map = self.map_node(root, "top level")?;
        self.check_keys(
            map,
            &["version", "name", "description", "blocks", "nets", "lint"],
            "top level",
        );
        // version: required, == 1
        match Self::get(map, "version").and_then(|n| match n {
            Node::Scalar(s, _) => Some(s.clone()),
            _ => None,
        }) {
            Some(v) if v == "1" => {}
            _ => self.diags.push(Diagnostic::error(
                "bad-version",
                "`version: 1` is required at top level",
            )),
        }

        let mut d = SurfaceDesign {
            name: Self::get(map, "name").and_then(|n| self.scalar(n, "name")),
            description: Self::get(map, "description").and_then(|n| self.scalar(n, "description")),
            ..Default::default()
        };


        match Self::get(map, "blocks") {
            Some(n) => {
                if let Some(bm) = self.map_node(n, "blocks") {
                    for ((bname, bspan), bnode) in bm {
                        if !bname
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                        {
                            self.err(
                                "bad-block-name",
                                format!("block `{bname}` must be lower_snake"),
                                *bspan,
                            );
                        }
                        if let Some(b) = self.block(bnode) {
                            d.blocks.insert(bname.clone(), b);
                        }
                    }
                }
                if d.blocks.is_empty() {
                    self.diags.push(Diagnostic::error(
                        "no-blocks",
                        "at least one block is required",
                    ));
                }
            }
            None => self
                .diags
                .push(Diagnostic::error("no-blocks", "`blocks:` is required")),
        }

        if let Some(n) = Self::get(map, "nets")
            && let Some(nm) = self.map_node(n, "nets")
        {
            for ((net, nspan), nnode) in nm {
                self.check_net_name(net, *nspan);
                d.nets.insert(net.clone(), self.net_attrs(nnode));
            }
        }

        if let Some(n) = Self::get(map, "lint")
            && let Some(lm) = self.map_node(n, "lint")
        {
            self.check_keys(lm, &["allow"], "lint");
            if let Some(an) = Self::get(lm, "allow") {
                match an {
                    Node::Seq(items, _) => {
                        for it in items {
                            if let Some(s) = self.scalar(it, "lint.allow entry") {
                                d.lint_allow.push(s);
                            }
                        }
                    }
                    _ => self.err(
                        "expected-seq",
                        "`lint.allow` must be a list of lint codes".into(),
                        an.span(),
                    ),
                }
            }
        }

        Some(d)
    }

    /// Parse a per-block `layout:` 2D array — a sequence of rows, each a sequence
    /// of cells. A cell is a refdes (of this block), or `~`/null for a hole.
    fn layout_grid(&mut self, n: &Node) -> Vec<Vec<(Option<String>, Span)>> {
        let Node::Seq(rows, _) = n else {
            self.err(
                "expected-seq",
                "`layout` must be a list of rows (a 2D array)".into(),
                n.span(),
            );
            return Vec::new();
        };
        let mut grid = Vec::new();
        for row in rows {
            let Node::Seq(cells, _) = row else {
                self.err(
                    "expected-seq",
                    "each `layout` row must be a list of cells".into(),
                    row.span(),
                );
                continue;
            };
            let mut out_row = Vec::new();
            for cell in cells {
                match cell {
                    Node::Null(span) => out_row.push((None, *span)),
                    Node::Scalar(s, span) => out_row.push((Some(s.clone()), *span)),
                    other => self.err(
                        "expected-scalar",
                        "a `layout` cell must be a block/refdes name or `~`".into(),
                        other.span(),
                    ),
                }
            }
            grid.push(out_row);
        }
        grid
    }

    fn check_net_name(&mut self, name: &str, span: Span) {
        if name.contains(' ') || name.contains('/') {
            self.err(
                "bad-net-name",
                format!("net `{name}`: spaces forbidden, `/` reserved for hierarchy"),
                span,
            );
        }
        if name.chars().any(|c| c.is_ascii_lowercase()) {
            self.diags.push(
                Diagnostic::warning("net-name-case", format!("net `{name}` is not UPPER_SNAKE"))
                    .with_span(span),
            );
        }
    }

    fn net_attrs(&mut self, n: &Node) -> SurfaceNet {
        let mut out = SurfaceNet {
            span: Some(n.span()),
            ..Default::default()
        };
        if let Some(m) = self.map_node(n, "net attributes") {
            self.check_keys(m, &["class"], "net attributes");
            out.class = Self::get(m, "class").and_then(|v| self.scalar(v, "class"));
        }
        out
    }

    fn block(&mut self, n: &Node) -> Option<SurfaceBlock> {
        let m = self.map_node(n, "block")?;
        self.check_keys(m, &["note", "components", "layout"], "block");
        let mut b = SurfaceBlock {
            span: Some(n.span()),
            ..Default::default()
        };
        b.note = Self::get(m, "note").and_then(|v| self.scalar(v, "note"));
        if let Some(ln) = Self::get(m, "layout") {
            b.layout = self.layout_grid(ln);
        }
        if let Some(cn) = Self::get(m, "components")
            && let Some(cm) = self.map_node(cn, "components")
        {
            for ((refdes, rspan), cnode) in cm {
                if !looks_like_refdes(refdes) {
                    self.err(
                        "bad-refdes",
                        format!("`{refdes}` is not a valid refdes (expected e.g. U1, R10)"),
                        *rspan,
                    );
                }
                if let Some(c) = self.component(cnode) {
                    b.components.insert(refdes.clone(), c);
                }
            }
        }
        Some(b)
    }

    fn pin_map(&mut self, n: &Node, out: &mut IndexMap<String, (String, Span)>) {
        if let Some(m) = self.map_node(n, "pins") {
            for ((pin, pspan), v) in m {
                if let Some(t) = self.scalar(v, "pin target") {
                    if !t.eq_ignore_ascii_case("nc") && !t.contains('.') {
                        self.check_net_name(&t, v.span());
                    }
                    if out.insert(pin.clone(), (t, v.span())).is_some() {
                        self.err(
                            "pin-conflict",
                            format!("pin `{pin}` mapped more than once"),
                            *pspan,
                        );
                    }
                }
            }
        }
    }

    fn component(&mut self, n: &Node) -> Option<SurfaceComponent> {
        let m = self.map_node(n, "component")?;
        self.check_keys(
            m,
            &[
                "part",
                "value",
                "footprint",
                "dnp",
                "props",
                "pins",
                "units",
                "between",
                "positive",
                "negative",
                "decouple",
            ],
            "component",
        );
        let mut c = SurfaceComponent {
            span: Some(n.span()),
            ..Default::default()
        };
        // A present-but-null `part:` is treated as missing (one `missing-part`
        // diagnostic), not an `expected-scalar`: the value is effectively absent.
        match Self::get(m, "part") {
            Some(v) if !matches!(v, Node::Null(_)) => {
                if let Some(p) = self.scalar(v, "part") {
                    c.part = p;
                }
            }
            _ => self.err("missing-part", "`part:` is required".into(), n.span()),
        }
        c.value = Self::get(m, "value").and_then(|v| self.scalar(v, "value"));
        c.footprint = Self::get(m, "footprint").and_then(|v| self.scalar(v, "footprint"));
        if let Some(d) = Self::get(m, "dnp").and_then(|v| self.bool_field(v, "dnp")) {
            c.dnp = d;
        }
        if let Some(pn) = Self::get(m, "props")
            && let Some(pm) = self.map_node(pn, "props")
        {
            for ((k, _), v) in pm {
                if let Some(s) = self.scalar(v, "prop value") {
                    c.props.insert(k.clone(), s);
                }
            }
        }
        if let Some(pn) = Self::get(m, "pins") {
            let mut pins = IndexMap::new();
            self.pin_map(pn, &mut pins);
            c.pins = pins;
        }
        if let Some(un) = Self::get(m, "units")
            && let Some(um) = self.map_node(un, "units")
        {
            for ((uname, _), unode) in um {
                if let Some(uim) = self.map_node(unode, "unit") {
                    self.check_keys(uim, &["pins"], "unit");
                    let mut pins = IndexMap::new();
                    if let Some(pn) = Self::get(uim, "pins") {
                        self.pin_map(pn, &mut pins);
                    }
                    c.units.insert(uname.clone(), pins);
                }
            }
        }
        if let Some(bn) = Self::get(m, "between") {
            match bn {
                Node::Seq(items, _) if items.len() == 2 => {
                    let a = self.scalar(&items[0], "between[0]");
                    let b = self.scalar(&items[1], "between[1]");
                    if let (Some(a), Some(b)) = (a, b) {
                        c.between = Some(((a, items[0].span()), (b, items[1].span())));
                    }
                }
                _ => self.err(
                    "bad-between",
                    "`between` must be a 2-element list".into(),
                    bn.span(),
                ),
            }
        }
        if let Some(v) = Self::get(m, "positive")
            && let Some(s) = self.scalar(v, "positive")
        {
            c.positive = Some((s, v.span()));
        }
        if let Some(v) = Self::get(m, "negative")
            && let Some(s) = self.scalar(v, "negative")
        {
            c.negative = Some((s, v.span()));
        }
        if let Some(dn) = Self::get(m, "decouple")
            && let Some(dm) = self.map_node(dn, "decouple")
        {
            for ((val, vspan), cnt) in dm {
                match self
                    .scalar(cnt, "decouple count")
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    Some(k) if k >= 1 => {
                        c.decouple.insert(val.clone(), k);
                    }
                    _ => self.err(
                        "bad-decouple",
                        format!("decouple count for `{val}` must be a positive integer"),
                        *vspan,
                    ),
                }
            }
        }
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Severity;

    const MINIMAL: &str = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, value: 4.7k, pins: {1: A, 2: GND}}
";

    #[test]
    fn parses_minimal_design() {
        let (d, diags) = parse_str(MINIMAL);
        assert!(!diags.has_errors(), "{:?}", diags);
        let d = d.unwrap();
        let r1 = &d.blocks["main"].components["R1"];
        assert_eq!(r1.part, "R");
        assert_eq!(r1.value.as_deref(), Some("4.7k")); // YAML 1.2: stays a string
        assert_eq!(r1.pins["1"].0, "A");
        assert_eq!(r1.pins["2"].0, "GND");
    }

    #[test]
    fn unknown_key_errors_with_suggestion() {
        let src = "
version: 1
blocks:
  main:
    components:
      U1: {part: X:Y, decuople: {100nF: 2}}
";
        let (_, diags) = parse_str(src);
        let e = diags.0.iter().find(|d| d.code == "unknown-key").unwrap();
        assert_eq!(e.severity, Severity::Error);
        assert_eq!(e.suggestion.as_deref(), Some("decouple"));
        assert!(e.span.is_some());
    }

    #[test]
    fn version_must_be_1() {
        let (_, diags) = parse_str("version: 2\nblocks: {main: {components: {}}}");
        assert!(diags.0.iter().any(|d| d.code == "bad-version"));
        let (_, diags) = parse_str("blocks: {main: {components: {}}}");
        assert!(diags.0.iter().any(|d| d.code == "bad-version"));
    }

    #[test]
    fn parses_sugar_and_full_component_fields() {
        let src = "
version: 1
name: t
blocks:
  rails:
    components:
      PWR1: {part: power:VCC, pins: {1: 3V3}}
      PWR2: {part: power:GND, pins: {1: GND}}
  main:
    note: power section
    layout:
      - [U3, U1]
      - [C1]
    components:
      C1: {part: C, value: 10uF, between: [VBUS, GND], dnp: true,
           footprint: Capacitor_SMD:C_0603_1608Metric, props: {MPN: GRM188}}
      U3:
        part: Amplifier_Operational:LM358
        units:
          A: {pins: {'+': X, '-': Y, OUT: Z}}
        pins: {V+: 3V3, V-: GND}
      U1: {part: M:CPU, decouple: {100nF: 4}, pins: {VDD: 3V3, PA0: nc}}
nets:
  X: {class: analog}
";
        let (d, diags) = parse_str(src);
        assert!(!diags.has_errors(), "{:?}", diags);
        let d = d.unwrap();
        // power symbols parse as ordinary components in their block
        assert_eq!(d.blocks["rails"].components["PWR1"].part, "power:VCC");
        assert_eq!(d.blocks["rails"].components["PWR2"].part, "power:GND");
        let b = &d.blocks["main"];
        // per-block layout grid: 2 rows, names preserved
        assert_eq!(b.layout.len(), 2);
        assert_eq!(b.layout[0][0].0.as_deref(), Some("U3"));
        assert_eq!(b.layout[1][0].0.as_deref(), Some("C1"));
        let c1 = &b.components["C1"];
        assert!(c1.dnp);
        assert_eq!(c1.between.as_ref().unwrap().0.0, "VBUS");
        assert_eq!(c1.props["MPN"], "GRM188");
        let u3 = &b.components["U3"];
        assert_eq!(u3.units["A"]["OUT"].0, "Z");
        assert_eq!(u3.pins["V+"].0, "3V3");
        let u1 = &b.components["U1"];
        assert_eq!(u1.decouple["100nF"], 4);
        assert_eq!(u1.pins["PA0"].0, "nc");
        assert_eq!(d.nets["X"].class.as_deref(), Some("analog"));
    }

    #[test]
    fn bad_refdes_and_net_names_rejected() {
        let src = "
version: 1
blocks:
  main:
    components:
      lowercase1: {part: R, pins: {1: 'MY NET'}}
";
        let (_, diags) = parse_str(src);
        assert!(diags.0.iter().any(|d| d.code == "bad-refdes"));
        assert!(diags.0.iter().any(|d| d.code == "bad-net-name")); // space
    }

    #[test]
    fn empty_part_yields_missing_part_not_literal_tilde() {
        // Empty / null / `~` part must be rejected as missing, never minted
        // into the literal string `"~"`/`"null"`.
        for v in ["", "null", "~"] {
            let src = format!(
                "version: 1\nblocks:\n  main:\n    components:\n      R1: {{part: {v}, value: 1k}}\n"
            );
            let (d, diags) = parse_str(&src);
            assert!(
                diags.0.iter().any(|x| x.code == "missing-part"),
                "part `{v}` should be missing-part, got {diags:?}"
            );
            // And we must not get a confusing extra `expected-scalar` for it.
            assert!(
                !diags.0.iter().any(|x| x.code == "expected-scalar"),
                "part `{v}` should not also emit expected-scalar"
            );
            // The part field never becomes the literal "~"/"null".
            if let Some(d) = d {
                assert_ne!(d.blocks["main"].components["R1"].part, "~");
                assert_ne!(d.blocks["main"].components["R1"].part, "null");
            }
        }
    }

    #[test]
    fn empty_pin_target_is_rejected_not_minted() {
        let src = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, pins: {1: }}
";
        let (d, diags) = parse_str(src);
        // Empty pin target must surface as expected-scalar, not a silent "~".
        assert!(diags.0.iter().any(|x| x.code == "expected-scalar"));
        if let Some(d) = d {
            assert!(!d.blocks["main"].components["R1"].pins.contains_key("1"));
        }
    }

    #[test]
    fn quoted_null_stays_a_string() {
        // A quoted "null" is a genuine string, not YAML null.
        let src = "
version: 1
blocks:
  main:
    components:
      R1: {part: \"null\"}
";
        let (d, diags) = parse_str(src);
        assert!(!diags.has_errors(), "{diags:?}");
        assert_eq!(d.unwrap().blocks["main"].components["R1"].part, "null");
    }

    #[test]
    fn non_map_props_emits_expected_map() {
        let src = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, props: somestring}
";
        let (_, diags) = parse_str(src);
        assert!(diags.0.iter().any(|d| d.code == "expected-map"));
    }

    #[test]
    fn duplicate_component_refdes_in_one_map_errors() {
        let src = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, value: 1k, pins: {1: A, 2: GND}}
      R1: {part: C, value: 2k, pins: {1: A, 2: GND}}
";
        let (_, diags) = parse_str(src);
        assert!(
            diags.0.iter().any(|d| d.code == "duplicate-key"),
            "expected duplicate-key, got {diags:?}"
        );
    }

    #[test]
    fn duplicate_field_key_errors() {
        let (_, diags) = parse_str(
            "version: 1\nblocks: {main: {components: {R1: {part: R, value: 1k, value: 2k}}}}",
        );
        assert!(diags.0.iter().any(|d| d.code == "duplicate-key"));
    }

    #[test]
    fn multiple_documents_error() {
        let src = "version: 1\nblocks: {main: {components: {}}}\n---\nversion: 1\nblocks: {other: {components: {}}}";
        let (_, diags) = parse_str(src);
        assert!(diags.0.iter().any(|d| d.code == "multiple-documents"));
    }

    #[test]
    fn refdes_must_be_letters_then_digits() {
        for bad in ["R1A2", "RA1B2", "R2C3"] {
            let src = format!(
                "version: 1\nblocks: {{main: {{components: {{{bad}: {{part: R, pins: {{1: A, 2: B}}}}}}}}}}"
            );
            let (_, d) = parse_str(&src);
            assert!(
                d.0.iter().any(|x| x.code == "bad-refdes"),
                "{bad} should be rejected"
            );
        }
        for ok in ["R1", "U10", "J2"] {
            let src = format!(
                "version: 1\nblocks: {{main: {{components: {{{ok}: {{part: R, pins: {{1: A, 2: B}}}}}}}}}}"
            );
            let (_, d) = parse_str(&src);
            assert!(
                !d.0.iter().any(|x| x.code == "bad-refdes"),
                "{ok} should be accepted"
            );
        }
    }

    #[test]
    fn lowercase_net_name_warns() {
        let (_, d) = parse_str(
            "version: 1\nblocks: {main: {components: {R1: {part: R, pins: {1: sda, 2: GND}}}}}",
        );
        assert!(
            d.0.iter()
                .any(|x| x.code == "net-name-case" && x.severity == crate::diag::Severity::Warning)
        );
    }
}
