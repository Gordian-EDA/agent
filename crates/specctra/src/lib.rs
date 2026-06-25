//! Specctra `.ses` import for Freerouting output.
//!
//! This crate intentionally does not parse `.kicad_pcb` files. It only decodes
//! routed geometry from Specctra session files and converts that geometry to the
//! shared [`pcb_model::RouteSolution`] representation when the caller supplies any
//! board-specific defaults explicitly.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use kiutils_sexpr::{Atom, Node, parse_one};
use pcb_model::{LayerRef, Point2, RouteSolution, Trace, Via, ViaSpan};

/// A routed copper polyline recovered from a `.ses`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedWire {
    /// Net name from the `.ses` `net` token.
    pub net: String,
    /// Copper layer name from the `.ses` path, e.g. `F.Cu`, `In1.Cu`, `B.Cu`.
    pub layer: String,
    /// Trace width in mm.
    pub width_mm: f64,
    /// Ordered polyline vertices in mm, KiCAD y-down space.
    pub path: Vec<Point2>,
}

/// A routed via recovered from a `.ses`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedVia {
    /// Net name from the `.ses` `net` token.
    pub net: String,
    /// Via center in mm, KiCAD y-down space.
    pub at: Point2,
    /// Finished via diameter in mm.
    pub diameter_mm: f64,
    /// Drill diameter in mm.
    pub drill_mm: f64,
}

/// Board-specific defaults needed while importing a `.ses`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SesImportOptions {
    /// Finished via diameter in mm, used when the `.ses` does not define the via
    /// padstack diameter.
    pub default_via_diameter: f64,
    /// Drill diameter in mm. Specctra sessions usually do not carry drill sizes.
    pub default_via_drill: f64,
}

impl Default for SesImportOptions {
    fn default() -> Self {
        Self {
            default_via_diameter: 0.6,
            default_via_drill: 0.3,
        }
    }
}

/// The geometry Freerouting produced for a board: routed wires + vias, in mm,
/// KiCAD coordinate space.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RoutedGeometry {
    pub wires: Vec<RoutedWire>,
    pub vias: Vec<RoutedVia>,
}

impl RoutedGeometry {
    /// Convert into an engine [`RouteSolution`].
    ///
    /// Layer names are preserved as `LayerRef` strings. `pcb_model::LayerRef`
    /// resolves both engine vocabulary (`top`, `bottom`, `innerN`) and KiCAD copper
    /// names (`F.Cu`, `B.Cu`, `InN.Cu`) at emission time.
    pub fn to_solution(&self) -> RouteSolution {
        let traces = self
            .wires
            .iter()
            .filter(|w| w.path.len() >= 2)
            .map(|w| Trace {
                connection: w.net.clone(),
                layer: LayerRef(w.layer.clone()),
                width: w.width_mm,
                path: w.path.clone(),
            })
            .collect();
        let vias = self
            .vias
            .iter()
            .map(|v| Via {
                connection: v.net.clone(),
                at: v.at.clone(),
                diameter: v.diameter_mm,
                drill: v.drill_mm,
                span: ViaSpan::Through,
            })
            .collect();
        RouteSolution { traces, vias }
    }
}

/// What can go wrong importing a Specctra session.
#[derive(Debug)]
pub enum SpecctraError {
    /// I/O reading the `.ses`.
    Io(io::Error),
    /// The `.ses` could not be parsed.
    Parse(String),
}

impl std::fmt::Display for SpecctraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecctraError::Io(e) => write!(f, "io error: {e}"),
            SpecctraError::Parse(s) => write!(f, "could not parse .ses: {s}"),
        }
    }
}

impl std::error::Error for SpecctraError {}

impl From<io::Error> for SpecctraError {
    fn from(e: io::Error) -> Self {
        SpecctraError::Io(e)
    }
}

/// Backwards-compatible name for session import errors.
pub type FreerouteError = SpecctraError;

/// Route rules used by callers that still need to write a KiCAD project netclass
/// beside a board. This type is data-only; it does not read board files.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteRules {
    /// Minimum trace width in mm.
    pub trace_width: f64,
    /// Copper clearance in mm.
    pub clearance: f64,
    /// Finished via diameter in mm.
    pub via_diameter: f64,
    /// Via drill diameter in mm.
    pub via_drill: f64,
}

/// Write a KiCAD project file (`<board-stem>.kicad_pro`) beside `board_path` whose
/// **Default** netclass carries these rules.
///
/// This helper only writes JSON; it does not parse or inspect the board file.
pub fn write_net_settings(board_path: &Path, rules: RouteRules) -> io::Result<()> {
    let stem = board_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");
    let pro_path = board_path.with_file_name(format!("{stem}.kicad_pro"));

    let pro = format!(
        r#"{{
  "board": {{
    "design_settings": {{
      "rules": {{
        "min_clearance": 0.0,
        "min_track_width": 0.0,
        "min_via_diameter": 0.0,
        "min_hole_clearance": {hole},
        "min_hole_to_hole": {hole}
      }}
    }}
  }},
  "net_settings": {{
    "classes": [
      {{
        "name": "Default",
        "clearance": {clr},
        "track_width": {tw},
        "via_diameter": {vd},
        "via_drill": {vdr},
        "microvia_diameter": 0.3,
        "microvia_drill": 0.1,
        "diff_pair_gap": 0.25,
        "diff_pair_width": 0.2,
        "priority": 2147483647
      }}
    ],
    "meta": {{ "version": 3 }}
  }},
  "meta": {{ "filename": "{stem}.kicad_pro", "version": 1 }}
}}
"#,
        hole = rules.clearance,
        clr = rules.clearance,
        tw = rules.trace_width,
        vd = rules.via_diameter,
        vdr = rules.via_drill,
    );
    std::fs::write(pro_path, pro)
}

/// Read Freerouting `.ses` output and recover routed copper geometry.
///
/// Coordinates are returned in mm with KiCAD's y-down axis. Widths and via
/// diameters come from the `.ses`; `options` supplies drill and via fallbacks
/// because those are board-specific and are not reliably present in sessions.
pub fn import_ses(
    ses_path: &Path,
    options: SesImportOptions,
) -> Result<RoutedGeometry, SpecctraError> {
    let text = std::fs::read_to_string(ses_path)?;
    import_ses_str(&text, options)
}

/// Parse Freerouting `.ses` text and recover routed copper geometry.
pub fn import_ses_str(
    text: &str,
    options: SesImportOptions,
) -> Result<RoutedGeometry, SpecctraError> {
    let doc = parse_one(text).map_err(|e| SpecctraError::Parse(e.to_string()))?;
    let root = doc
        .nodes
        .first()
        .ok_or_else(|| SpecctraError::Parse("empty .ses".to_owned()))?;

    let mm_div = ses_mm_divisor(root);
    let via_diam = via_padstack_diameters(root, mm_div);
    let mut geo = RoutedGeometry::default();

    if let Some(routes) = find_child(root, "routes")
        && let Some(network_out) = find_child(routes, "network_out")
    {
        for net_node in children_named(network_out, "net") {
            let Some(net_name) = first_atom_string(net_node) else {
                continue;
            };
            for wire in children_named(net_node, "wire") {
                if let Some(w) = parse_wire(wire, &net_name, mm_div) {
                    geo.wires.push(w);
                }
            }
            for via in children_named(net_node, "via") {
                if let Some(v) = parse_via(
                    via,
                    &net_name,
                    &via_diam,
                    options.default_via_diameter,
                    options.default_via_drill,
                    mm_div,
                ) {
                    geo.vias.push(v);
                }
            }
        }
    }

    Ok(geo)
}

/// The divisor from `.ses` file-units to mm, read from `(resolution UNIT N)`.
fn ses_mm_divisor(root: &Node) -> f64 {
    let res = find_child(root, "resolution")
        .or_else(|| find_child(root, "placement").and_then(|p| find_child(p, "resolution")))
        .or_else(|| find_child(root, "routes").and_then(|p| find_child(p, "resolution")));
    let Some(res) = res else { return 10_000.0 };
    let Some(items) = list_items(res) else {
        return 10_000.0;
    };
    let unit = items.get(1).and_then(atom_str).unwrap_or_default();
    let n = items.get(2).and_then(node_f64).unwrap_or(10.0);
    let um_per_unit = match unit.as_str() {
        "um" => 1.0,
        "mm" => 1000.0,
        "inch" => 25_400.0,
        "mil" => 25.4,
        _ => 1.0,
    };
    (n / um_per_unit) * 1000.0
}

fn parse_via(
    node: &Node,
    net: &str,
    via_diam: &BTreeMap<String, f64>,
    default_via_d: f64,
    default_drill: f64,
    mm_div: f64,
) -> Option<RoutedVia> {
    let items = list_items(node)?;
    let padstack = atom_str(items.get(1)?)?;
    let nums: Vec<f64> = items[2..].iter().filter_map(node_f64).collect();
    let (x, y) = (*nums.first()?, *nums.get(1)?);
    let diameter_mm = via_diam.get(&padstack).copied().unwrap_or(default_via_d);
    Some(RoutedVia {
        net: net.to_owned(),
        at: Point2 {
            x: x / mm_div,
            y: -y / mm_div,
        },
        diameter_mm,
        drill_mm: default_drill,
    })
}

fn parse_wire(node: &Node, net: &str, mm_div: f64) -> Option<RoutedWire> {
    let path = find_child(node, "path")?;
    let items = list_items(path)?;
    let layer = atom_str(items.get(1)?)?;
    let width = node_f64(items.get(2)?)?;
    let coords: Vec<f64> = items[3..].iter().filter_map(node_f64).collect();
    if coords.len() < 4 {
        return None;
    }
    let mut pts = Vec::new();
    let mut i = 0;
    while i + 1 < coords.len() {
        pts.push(Point2 {
            x: coords[i] / mm_div,
            y: -coords[i + 1] / mm_div,
        });
        i += 2;
    }
    Some(RoutedWire {
        net: net.to_owned(),
        layer,
        width_mm: width / mm_div,
        path: pts,
    })
}

fn via_padstack_diameters(root: &Node, mm_div: f64) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let Some(library) = find_child(root, "library_out").or_else(|| find_child(root, "library"))
    else {
        return out;
    };
    for ps in children_named(library, "padstack") {
        let Some(id) = first_atom_string(ps) else {
            continue;
        };
        let mut d = 0.0_f64;
        for shape in children_named(ps, "shape") {
            if let Some(circle) = find_child(shape, "circle")
                && let Some(items) = list_items(circle)
                && let Some(dia) = items.get(2).and_then(node_f64)
            {
                d = d.max(dia / mm_div);
            }
        }
        if d > 0.0 {
            out.insert(id, d);
        }
    }
    out
}

fn list_items(node: &Node) -> Option<&[Node]> {
    match node {
        Node::List { items, .. } => Some(items),
        _ => None,
    }
}

fn head(node: &Node) -> Option<String> {
    let items = list_items(node)?;
    atom_str(items.first()?)
}

fn atom_str(node: &Node) -> Option<String> {
    match node {
        Node::Atom { atom, .. } => Some(match atom {
            Atom::Symbol(s) | Atom::Quoted(s) => s.clone(),
        }),
        _ => None,
    }
}

fn first_atom_string(node: &Node) -> Option<String> {
    let items = list_items(node)?;
    atom_str(items.get(1)?)
}

fn node_f64(node: &Node) -> Option<f64> {
    match node {
        Node::Atom {
            atom: Atom::Symbol(s),
            ..
        } => s.parse().ok(),
        _ => None,
    }
}

fn find_child<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    let items = list_items(node)?;
    items.iter().find(|c| head(c).as_deref() == Some(name))
}

fn children_named<'a>(node: &'a Node, name: &str) -> Vec<&'a Node> {
    list_items(node)
        .map(|items| {
            items
                .iter()
                .filter(|c| head(c).as_deref() == Some(name))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wire_negates_y_and_scales() {
        let text = r#"(wire (path F.Cu 2500 10000 -20000 30000 -40000) (net GND))"#;
        let doc = parse_one(text).unwrap();
        let node = doc.nodes.first().unwrap();
        let w = parse_wire(node, "GND", 10_000.0).unwrap();
        assert_eq!(w.layer, "F.Cu");
        assert_eq!(w.width_mm, 0.25);
        assert_eq!(w.path[0], Point2 { x: 1.0, y: 2.0 });
        assert_eq!(w.path[1], Point2 { x: 3.0, y: 4.0 });
    }

    #[test]
    fn ses_resolution_um10_is_10000_per_mm() {
        let text = "(session s (resolution um 10))";
        let doc = parse_one(text).unwrap();
        let root = doc.nodes.first().unwrap();
        assert_eq!(ses_mm_divisor(root), 10_000.0);
    }

    #[test]
    fn ses_resolution_under_routes_is_found() {
        let text = "(session s (routes (resolution um 10)))";
        let doc = parse_one(text).unwrap();
        let root = doc.nodes.first().unwrap();
        assert_eq!(ses_mm_divisor(root), 10_000.0);
    }

    #[test]
    fn ses_resolution_defaults_when_absent() {
        let text = "(session s)";
        let doc = parse_one(text).unwrap();
        let root = doc.nodes.first().unwrap();
        assert_eq!(ses_mm_divisor(root), 10_000.0);
    }

    #[test]
    fn parse_via_negates_y_and_resolves_diameter() {
        let text = "(via via_default 487819 -513009)";
        let doc = parse_one(text).unwrap();
        let node = doc.nodes.first().unwrap();
        let mut diam = BTreeMap::new();
        diam.insert("via_default".to_owned(), 0.5);
        let v = parse_via(node, "GND", &diam, 0.6, 0.3, 10_000.0).unwrap();
        assert_eq!(v.net, "GND");
        assert!((v.at.x - 48.7819).abs() < 1e-6, "{:?}", v.at);
        assert!((v.at.y - 51.3009).abs() < 1e-6, "{:?}", v.at);
        assert_eq!(v.diameter_mm, 0.5);
        assert_eq!(v.drill_mm, 0.3);
    }

    #[test]
    fn import_ses_walks_network_out() {
        let ses = r#"(session s
  (routes
    (resolution um 10)
    (network_out
      (net SA3
        (wire (path F.Cu 1000 100000 -200000 100000 -300000) (net SA3))
      )
      (net GND
        (via via_default 150000 -250000)
      )
    )
  )
)"#;
        let geo = import_ses_str(
            ses,
            SesImportOptions {
                default_via_diameter: 0.6,
                default_via_drill: 0.3,
            },
        )
        .unwrap();
        assert_eq!(geo.wires.len(), 1);
        assert_eq!(geo.wires[0].net, "SA3");
        assert_eq!(geo.wires[0].width_mm, 0.1);
        assert_eq!(geo.wires[0].path[0], Point2 { x: 10.0, y: 20.0 });
        assert_eq!(geo.vias.len(), 1);
        assert_eq!(geo.vias[0].net, "GND");
        assert_eq!(geo.vias[0].at, Point2 { x: 15.0, y: 25.0 });
    }

    #[test]
    fn routed_geometry_converts_to_solution_without_board_parser() {
        let geo = RoutedGeometry {
            wires: vec![RoutedWire {
                net: "N1".into(),
                layer: "F.Cu".into(),
                width_mm: 0.15,
                path: vec![Point2 { x: 1.0, y: 2.0 }, Point2 { x: 3.0, y: 4.0 }],
            }],
            vias: vec![RoutedVia {
                net: "N1".into(),
                at: Point2 { x: 2.0, y: 3.0 },
                diameter_mm: 0.6,
                drill_mm: 0.3,
            }],
        };

        let solution = geo.to_solution();
        assert_eq!(solution.traces[0].layer, LayerRef("F.Cu".into()));
        assert_eq!(solution.vias[0].span, ViaSpan::Through);
    }
}
