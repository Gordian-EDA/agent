# Geometry Single-Convention Extraction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Paint down ONE geometry convention across the whole workspace and move every shared 2D helper into the leaf `geom` crate, deleting all duplicates and drift.

**Architecture:** `geom` becomes the single source of truth for the 2D primitives (`Point2`, `Rect`), the segment kernel, rotation, AABB, polyline helpers, tolerance tiers, and grid snapping. Every other crate depends on `geom` and uses its types directly. All internal geometry is **y-down, CCW-positive degrees**; y-up survives only inside the two external-format I/O adapters (`.kicad_sym` symbol load and Specctra), where it is converted at the boundary.

**Tech Stack:** Rust (edition 2024), `serde`/`serde_json`, workspace deps. No new third-party crates.

**Constraints dropped (per product owner):** No back-compat for existing `.gordian/*.json` boards — break freely. The only external formats that still constrain serde field NAMES are KiCAD s-expr (handled by `kicad-sexpr`/`sch-io`, not JSON) and the archived tscircuit `SimpleRouteJson` benchmark *input* corpus, which keeps parsing because unified types retain `rename_all = "camelCase"` field names (`x`,`y`,`minX`,`maxX`,`minY`,`maxY`).

---

## The single convention (target state)

| Axis | One convention |
|---|---|
| Unit | **millimetres (`f64`)** internally everywhere (already uniform — no drift). µm/nm/px appear only inside the `specctra` (`UM_PER_MM=1000`), `kicad-ipc` (nm protobuf), and SVG render (`px_per_mm=10`) adapters. |
| Point | `geom::Point2 { x: f64, y: f64 }` (Copy, serde camelCase) — used EVERYWHERE. No `[f64;2]` points, no `Pt` alias, no `(f64,f64)` point tuples. |
| Rect | `geom::Rect { min_x, min_y, max_x, max_y }` (Copy, serde camelCase) — used EVERYWHERE. No `Bounds`, no `footlib::BBox`, no `[f64;4]` rect aliases. |
| Y axis | y-down internally everywhere. y-up only inside `kicad-symbol` load (flip at boundary) and `specctra` adapter. `raw_definition` stays verbatim y-up. |
| Rotation | one method `Point2::rotate(deg: f64) -> Point2`, CCW-positive, y-down: `x' = x·cosθ + y·sinθ`, `y' = −x·sinθ + y·cosθ`. |
| Angle | `f64` degrees, CCW. One `geom::snap_quadrant(deg: f64) -> f64`. No `i32` rotation fields. |
| Overlap | one boolean `Rect::overlaps` (open, `EPS`-eased). `place::rect_overlap` renamed `rect_axis_penetration` (it returns a metric, not a bool). `segment::segments_intersect` stays a distinct primitive (closed/touching). |
| Tolerances | centralized in `geom::consts`: `EPS = 1e-6` (general), `JOIN_EPS = 1e-12` (path stitching). Try collapsing the placement `1e-9` into `EPS`; keep a named `PLACEMENT_EPS` only if the critic/tests prove it load-bearing. Domain constants (`GRID_MM`, `PLACE_GRID_MM`, `WIRE_INFLATE_MM`, KiCAD rule mins) stay separately named. |

---

## Verified facts (read before coding)

- `geom` crate (`crates/geom`): currently `grid.rs`, `hash.rs`, `ids.rs`, `shape.rs`, `union_find.rs`; deps = `uuid` only. Leaf (no internal deps). `shape.rs` already holds `Dir`, `transform_offset`, `point_on_segment` (on `[f64;2]`). `grid.rs` holds `snap`/`snap_point`/`GRID_MM`.
- `pcb-model` (`crates/pcb-model/src/lib.rs`): `Point2` (:128, methods `dist2`/`dist`), `Rect` (:371, methods `width/height/area/center/contains/overlaps/intersection/boundary_crosses`), `Bounds` (:291, fields ordered `min_x,max_x,min_y,max_y`). `RouteProblem.bounds: Bounds` (:163), `RouteProblem.outline: Option<Vec<Point2>>` (:183), `Trace.path: Vec<Point2>` (:335), `Via.at: Point2`. Segment kernel in `crates/pcb-model/src/geom2d.rs` (`dist`, `point_seg_dist`, `seg_seg_dist`, `cross`, `segments_intersect`, `on_segment`, `point_rect_dist`, `seg_rect_dist`). `pcb-model` deps: `serde`, `uuid`, `rayon`.
- `pcb-model` placement helpers (`crates/pcb-model/src/place.rs`): `courtyard_margin` (:292), `snap_rotation(i32)->i32` (:297), `rotated_courtyard_half` (:303), `rotated_copper_bbox` (:314), `rotate_offset(&Point2,i32)->Point2` (:340), `pad_world` (:355), `rect_overlap` (:368), `compute_hpwl` (:471). `Placement.rotation: i32`, `LockedAt.rotation: i32`.
- Symbol y-up boundary: `crates/kicad-symbol/src/geometry.rs` — `PinGeom { number, name, at: [f64;2], angle: f64, length: f64, unit }` (:52), built in `pin_geom` (:236) verbatim from file (y-up). `approx_size` (:161). `raw_definition` is verbatim sexpr (stays y-up).
- The y-up→y-down bridge: `geom::shape::transform_offset` (`crates/geom/src/shape.rs:28`) does mirror → CCW rotate → final `[rx, -ry]` flip. **Algebraically**, if pin `at.y` is negated at load, `transform_offset` collapses to exactly `mirror-then-geom::rotate` (verified: output `[rx, -ry]` identical).
- Duplicates to delete:
  - `rotate_offset`: `pcb-model/src/place.rs:340` (i32/Point2), `kicad-sexpr/src/pcb.rs:332` (f64/tuple), `gordian-core/src/tools_pcb/engine_svg.rs:367` (i32/Point2 dup).
  - `rotated_aabb_half`: `kicad-sexpr/src/pcb.rs:340`, `kicad-sexpr/src/footlib.rs:309`, `pcb-synth/src/placefp.rs:238` (byte-identical).
  - `half_perimeter`: `grid-astar/src/router.rs:381`, `negotiated-mesh/src/pathing.rs:971`, `negotiated-mesh/src/detail.rs:1043` (byte-identical).
  - `simplify`: `grid-astar/src/router.rs:786`, `negotiated-mesh/src/detail.rs:1080` (identical, incl. 45° collinear merge).
- y-up reasoning sites that must flip with the keystone: `sch-place/src/netclass.rs` `pin_side`, `sch-floorplan/src/floorplan/place/idioms.rs` `orient_angle` (manual `-(dx*s+dy*c)`), `sch-floorplan/src/floorplan/place/infer.rs` edge pin ranking (`-p.at[1]`), `sch-io/src/label.rs` `pin_text_boxes`, `sch-io/src/write/build.rs` `quantize_dir` (`sy = -ry`).
- `pcb-model` dependents: `drc-core`, `drc-lint`, `grid-astar`, `negotiated-mesh`, `pcb-place`, `pcb-synth`, `specctra`, `kicad-sexpr`, `gordian-core`. Schematic stack (`sch-place` deps = `geom` + `kicad-symbol`; plus `sch-io`, `sch-floorplan`, `greedy-place`, `anneal-place`) uses `[f64;2]` today and does NOT depend on `pcb-model`.
- Critics: `tools/schematic_critic.py`, `tools/pcb_critic.py`. Full build/test: `cargo build --workspace`, `cargo test --workspace`.

---

### Task 1: `geom` canonical primitives + kernel

**Files:**
- Create: `crates/geom/src/point.rs`, `crates/geom/src/rect.rs`, `crates/geom/src/segment.rs`, `crates/geom/src/transform.rs`, `crates/geom/src/consts.rs`
- Modify: `crates/geom/src/lib.rs`, `crates/geom/Cargo.toml`, `crates/geom/src/shape.rs`

- [ ] **Step 1: add serde to `geom`**

In `crates/geom/Cargo.toml` `[dependencies]`:

```toml
serde = { workspace = true, features = ["derive"] }
```

- [ ] **Step 2: `consts.rs` — tolerance tiers**

```rust
//! Shared geometric tolerances. One name per *kind* of comparison so callers
//! never hand-roll an epsilon.

/// General geometric slop (mm): collinearity, touching, "are these equal".
pub const EPS: f64 = 1e-6;

/// Path-stitching vertex coincidence (mm): exact-dedup of routed polylines.
pub const JOIN_EPS: f64 = 1e-12;
```

- [ ] **Step 3: `point.rs` — the one point type**

```rust
use serde::{Deserialize, Serialize};

/// A 2-D point in millimetres, y-down. The single point type across the
/// workspace (schematic sheet, PCB board, symbol pins after load).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

impl Point2 {
    #[inline]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
    /// Squared euclidean distance (cheaper than [`Point2::dist`] for compares).
    #[inline]
    pub fn dist2(&self, other: Point2) -> f64 {
        let (dx, dy) = (self.x - other.x, self.y - other.y);
        dx * dx + dy * dy
    }
    /// Euclidean distance (mm).
    #[inline]
    pub fn dist(&self, other: Point2) -> f64 {
        self.dist2(other).sqrt()
    }
    /// Orientation determinant of `(self, a, b)`: >0 ccw, <0 cw, 0 collinear.
    #[inline]
    pub fn orient(self, a: Point2, b: Point2) -> f64 {
        (a.x - self.x) * (b.y - self.y) - (a.y - self.y) * (b.x - self.x)
    }
    /// Rotate about the origin by `deg` (CCW-positive) in y-down space:
    /// `x' = x·cosθ + y·sinθ`, `y' = −x·sinθ + y·cosθ`. KiCAD's footprint/symbol
    /// rotation convention — the single rotation primitive workspace-wide.
    #[inline]
    pub fn rotate(self, deg: f64) -> Point2 {
        let (s, c) = deg.to_radians().sin_cos();
        Point2::new(self.x * c + self.y * s, -self.x * s + self.y * c)
    }
    /// Apply a placed instance's transform to this local offset: optional
    /// x-mirror, then [`Point2::rotate`]. After the keystone, symbol-local
    /// geometry is already y-down, so this is the whole symbol→sheet transform —
    /// no special Y-flip. Translation to the instance position is the caller's job.
    #[inline]
    pub fn transform(self, deg: f64, mirror: bool) -> Point2 {
        let m = if mirror { Point2::new(-self.x, self.y) } else { self };
        m.rotate(deg)
    }
}

impl From<[f64; 2]> for Point2 {
    #[inline]
    fn from(a: [f64; 2]) -> Self {
        Self { x: a[0], y: a[1] }
    }
}
impl From<Point2> for [f64; 2] {
    #[inline]
    fn from(p: Point2) -> Self {
        [p.x, p.y]
    }
}
```

- [ ] **Step 4: `rect.rs` — the one rect type**

Port the `pcb-model::Rect` methods verbatim, add `from_points`, make it `Copy`, and fold the open+EPS overlap policy in:

```rust
use serde::{Deserialize, Serialize};

use crate::consts::EPS;
use crate::point::Point2;
use crate::segment::Segment;

/// An axis-aligned rectangle in mm, y-down, `[min, max]` per axis. The single
/// rect/bounds/bbox type across the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    #[inline]
    pub const fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self { min_x, min_y, max_x, max_y }
    }
    /// Normalized bbox from two arbitrary corner points.
    #[inline]
    pub fn from_points(a: Point2, b: Point2) -> Self {
        Self {
            min_x: a.x.min(b.x),
            min_y: a.y.min(b.y),
            max_x: a.x.max(b.x),
            max_y: a.y.max(b.y),
        }
    }
    /// Tight bounding box of a point set, or `None` if empty. The canonical
    /// "bbox of points" constructor (used by [`crate::Polyline::bbox`] too).
    pub fn bounding(points: &[Point2]) -> Option<Rect> {
        let first = points.first()?;
        let mut r = Rect::new(first.x, first.y, first.x, first.y);
        for p in points {
            r.min_x = r.min_x.min(p.x);
            r.min_y = r.min_y.min(p.y);
            r.max_x = r.max_x.max(p.x);
            r.max_y = r.max_y.max(p.y);
        }
        Some(r)
    }
    #[inline]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    /// Half-perimeter (width + height) — the HPWL term for a bbox.
    #[inline]
    pub fn half_perimeter(&self) -> f64 {
        self.width() + self.height()
    }
    #[inline]
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
    #[inline]
    pub fn area(&self) -> f64 {
        self.width() * self.height()
    }
    #[inline]
    pub fn center(&self) -> Point2 {
        Point2::new((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }
    /// Inflate by `m` on every side (negative shrinks).
    #[inline]
    pub fn inflate(&self, m: f64) -> Rect {
        Rect::new(self.min_x - m, self.min_y - m, self.max_x + m, self.max_y + m)
    }
    /// Is `p` inside or on the boundary?
    #[inline]
    pub fn contains(&self, p: Point2) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }
    /// Do the rects overlap with positive area? Open + `EPS`-eased: a shared
    /// edge (within float dust) is NOT an overlap. This is the single boolean
    /// overlap predicate for the whole workspace.
    #[inline]
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.min_x < other.max_x - EPS
            && self.max_x > other.min_x + EPS
            && self.min_y < other.max_y - EPS
            && self.max_y > other.min_y + EPS
    }
    /// The overlap rectangle, or `None` when they do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let min_x = self.min_x.max(other.min_x);
        let max_x = self.max_x.min(other.max_x);
        let min_y = self.min_y.max(other.min_y);
        let max_y = self.max_y.min(other.max_y);
        (min_x < max_x && min_y < max_y).then_some(Rect { min_x, min_y, max_x, max_y })
    }
    /// Does the boundary of `other` cross the interior of `self`?
    pub fn boundary_crosses(&self, other: &Rect) -> bool {
        if !self.overlaps(other) {
            return false;
        }
        let covers = other.min_x <= self.min_x
            && other.max_x >= self.max_x
            && other.min_y <= self.min_y
            && other.max_y >= self.max_y;
        !covers
    }
    /// Distance from `p` to this rect; 0 inside.
    pub fn dist_to_point(&self, p: Point2) -> f64 {
        let dx = (self.min_x - p.x).max(0.0).max(p.x - self.max_x);
        let dy = (self.min_y - p.y).max(0.0).max(p.y - self.max_y);
        (dx * dx + dy * dy).sqrt()
    }
    /// Min distance to another rect; 0 if they overlap or touch.
    pub fn dist_to_rect(&self, other: &Rect) -> f64 {
        let dx = (self.min_x - other.max_x).max(other.min_x - self.max_x).max(0.0);
        let dy = (self.min_y - other.max_y).max(other.min_y - self.max_y).max(0.0);
        (dx * dx + dy * dy).sqrt()
    }
    /// Min distance to a segment; 0 if it enters/touches. Mirror of
    /// [`Segment::dist_to_rect`].
    #[inline]
    pub fn dist_to_segment(&self, s: Segment) -> f64 {
        s.dist_to_rect(self)
    }
}
```

- [ ] **Step 5: `segment.rs` — a `Segment` struct that OWNS the kernel**

Replace the scattered free functions of `crates/pcb-model/src/geom2d.rs` with a `Segment` struct. Segment-involving distance/intersection are methods on `Segment`; rect-involving distances are methods on `Rect` (Step 4); the orientation determinant is `Point2::orient` (Step 3). One rule: **the distance method lives on the more complex shape** (`Rect` > `Segment` > `Point2`), named `dist_to_<shape>`.

```rust
use crate::consts::EPS;
use crate::point::Point2;
use crate::rect::Rect;

/// A line segment between two points (mm, y-down). Owns segment distance and
/// intersection math so callers say `seg.dist_to_rect(&r)` — no free functions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub a: Point2,
    pub b: Point2,
}

impl Segment {
    #[inline]
    pub const fn new(a: Point2, b: Point2) -> Self {
        Self { a, b }
    }
    #[inline]
    pub fn length(&self) -> f64 {
        self.a.dist(self.b)
    }
    #[inline]
    pub fn midpoint(&self) -> Point2 {
        Point2::new((self.a.x + self.b.x) / 2.0, (self.a.y + self.b.y) / 2.0)
    }

    /// Squared distance to point `p` (a zero-length segment is its point).
    pub fn dist2_to_point(&self, p: Point2) -> f64 {
        let ab = Point2::new(self.b.x - self.a.x, self.b.y - self.a.y);
        let len2 = ab.x * ab.x + ab.y * ab.y;
        if len2 <= f64::EPSILON {
            return p.dist2(self.a);
        }
        let t = (((p.x - self.a.x) * ab.x + (p.y - self.a.y) * ab.y) / len2).clamp(0.0, 1.0);
        p.dist2(Point2::new(self.a.x + t * ab.x, self.a.y + t * ab.y))
    }
    #[inline]
    pub fn dist_to_point(&self, p: Point2) -> f64 {
        self.dist2_to_point(p).sqrt()
    }

    /// Min distance to another segment; 0 if they intersect.
    pub fn dist_to_segment(&self, other: Segment) -> f64 {
        if self.intersects(other) {
            return 0.0;
        }
        self.dist_to_point(other.a)
            .min(self.dist_to_point(other.b))
            .min(other.dist_to_point(self.a))
            .min(other.dist_to_point(self.b))
    }

    /// Does `p` lie on this segment (endpoints included), within `EPS`?
    /// Collinear + within the bounding span.
    pub fn contains_point(&self, p: Point2) -> bool {
        self.a.orient(self.b, p).abs() <= EPS
            && p.x >= self.a.x.min(self.b.x) - EPS
            && p.x <= self.a.x.max(self.b.x) + EPS
            && p.y >= self.a.y.min(self.b.y) - EPS
            && p.y <= self.a.y.max(self.b.y) + EPS
    }

    /// Do the segments intersect (incl. touching / collinear overlap)?
    pub fn intersects(&self, other: Segment) -> bool {
        let (a, b, c, d) = (self.a, self.b, other.a, other.b);
        let d1 = c.orient(d, a);
        let d2 = c.orient(d, b);
        let d3 = a.orient(b, c);
        let d4 = a.orient(b, d);
        if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
            && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
        {
            return true;
        }
        // Collinear / touching fallback.
        other.contains_point(a)
            || other.contains_point(b)
            || self.contains_point(c)
            || self.contains_point(d)
    }

    /// Min distance to an axis-aligned rect; 0 if it enters/touches the rect.
    pub fn dist_to_rect(&self, r: &Rect) -> f64 {
        if r.dist_to_point(self.a) <= EPS || r.dist_to_point(self.b) <= EPS {
            return 0.0;
        }
        let c = [
            Point2::new(r.min_x, r.min_y),
            Point2::new(r.max_x, r.min_y),
            Point2::new(r.max_x, r.max_y),
            Point2::new(r.min_x, r.max_y),
        ];
        let mut best = f64::INFINITY;
        for i in 0..4 {
            best = best.min(self.dist_to_segment(Segment::new(c[i], c[(i + 1) % 4])));
        }
        best
    }
}
```

> `rect.rs` and `segment.rs` reference each other's types — fine within one crate (mutually-referencing modules compile). `Rect::dist_to_point`/`dist_to_rect`/`dist_to_segment` are defined in Step 4.
>
> `Segment::contains_point` subsumes the schematic `shape::point_on_segment` (same collinear + bbox-span test). When `shape` migrates to `Point2` in Task 7, fold `point_on_segment(p, a, b)` callers onto `Segment::new(a, b).contains_point(p)` and delete the free fn.

- [ ] **Step 6: `angle.rs` — the angle helpers with no single-shape owner**

Rotation is `Point2::rotate` and the instance transform is `Point2::transform` (Step 3). `angle.rs` keeps only the two helpers that operate on angles/extents, not on a shape:

```rust
/// Half-extents (hw, hh) of a `w × h` rectangle rotated by `deg`.
#[inline]
pub fn rotated_aabb_half(w: f64, h: f64, deg: f64) -> (f64, f64) {
    let (s, c) = deg.to_radians().sin_cos();
    (
        (w / 2.0 * c).abs() + (h / 2.0 * s).abs(),
        (w / 2.0 * s).abs() + (h / 2.0 * c).abs(),
    )
}

/// Snap `deg` to the nearest quadrant (0/90/180/270), result in `[0, 360)`.
#[inline]
pub fn snap_quadrant(deg: f64) -> f64 {
    (deg / 90.0).round().rem_euclid(4.0) * 90.0
}
```

- [ ] **Step 6b: `polyline.rs` — a `Polyline` newtype owning path ops**

The routed-path/terminal-set helpers become methods on a `Polyline(Vec<Point2>)` newtype (the bbox computation delegates to `Rect::bounding` from Step 4 — no duplicate min/max loop):

```rust
use crate::consts::EPS;
use crate::point::Point2;
use crate::rect::Rect;

/// An ordered polyline (mm, y-down): a routed copper/wire path or a net's
/// terminal set. Owns simplification and bbox queries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polyline(pub Vec<Point2>);

impl Polyline {
    #[inline]
    pub fn new(points: Vec<Point2>) -> Self {
        Self(points)
    }
    #[inline]
    pub fn points(&self) -> &[Point2] {
        &self.0
    }
    #[inline]
    pub fn into_points(self) -> Vec<Point2> {
        self.0
    }
    /// Tight bounding box, or `None` if empty.
    #[inline]
    pub fn bbox(&self) -> Option<Rect> {
        Rect::bounding(&self.0)
    }
    /// Half-perimeter (w + h) of the bbox; 0 if empty. The HPWL/net-order term.
    #[inline]
    pub fn half_perimeter(&self) -> f64 {
        self.bbox().map_or(0.0, |r| r.half_perimeter())
    }
    /// Dedup consecutive coincident points and merge collinear runs (orthogonal
    /// AND 45°). The single polyline simplifier for routed paths.
    pub fn simplify(self) -> Polyline {
        let mut pts: Vec<Point2> = Vec::with_capacity(self.0.len());
        for p in self.0 {
            if pts.last().map_or(true, |q: &Point2| q.dist2(p) > EPS * EPS) {
                pts.push(p);
            }
        }
        if pts.len() < 3 {
            return Polyline(pts);
        }
        let mut out: Vec<Point2> = vec![pts[0]];
        for i in 1..pts.len() - 1 {
            let (a, b, c) = (out[out.len() - 1], pts[i], pts[i + 1]);
            if a.orient(b, c).abs() > EPS {
                out.push(b);
            }
        }
        out.push(pts[pts.len() - 1]);
        Polyline(out)
    }
}
```

> NOTE: confirm `Polyline::simplify` reproduces the existing `grid-astar/src/router.rs:786` behavior including the 45° merge; the `orient`-based collinearity test above merges any collinear triple (orthogonal and 45°). Keep the router's unit test (Task 4) as the oracle.

- [ ] **Step 7: leave `shape.rs`/`grid.rs` untouched (additive task)**

**Leave `shape.rs` and `grid.rs` AS-IS** (`Dir`, `transform_offset`, `point_on_segment`, `snap_point` still on `[f64;2]`). The schematic stack calls these via `sch_place::geom` and must keep compiling — Task 1 is purely **additive** (it only adds the new `Point2`-based types). `shape.rs`/`grid.rs` are migrated to `Point2`, and the free `transform_offset` is deleted in favor of `Point2::transform`, in Task 7 when the whole schematic stack moves together.

- [ ] **Step 8: `lib.rs` — module wiring + flat re-exports**

```rust
pub mod angle;
pub mod consts;
pub mod grid;
pub mod hash;
pub mod ids;
pub mod point;
pub mod polyline;
pub mod rect;
pub mod segment;
pub mod shape;
pub mod union_find;

pub use angle::{rotated_aabb_half, snap_quadrant};
pub use consts::{EPS, JOIN_EPS};
pub use point::Point2;
pub use polyline::Polyline;
pub use rect::Rect;
pub use segment::Segment;
pub use shape::Dir;
```

- [ ] **Step 9: unit tests in `geom`**

Add `#[cfg(test)]` tests in `transform.rs` and `segment.rs`. The rotation test MUST pin the KiCAD convention (ported from `pcb-place/.../tests.rs:785`):

```rust
#[test]
fn rotate_matches_kicad_convention() {
    let p = Point2::new(-2.475, 1.905).rotate(270.0);
    assert!((p.x - -1.905).abs() < 1e-9 && (p.y - -2.475).abs() < 1e-9, "{p:?}");
    let q = Point2::new(-2.475, 1.905).rotate(90.0);
    assert!((q.x - 1.905).abs() < 1e-9 && (q.y - 2.475).abs() < 1e-9, "{q:?}");
    let r = Point2::new(1.0, 2.0).rotate(180.0);
    assert!((r.x - -1.0).abs() < 1e-9 && (r.y - -2.0).abs() < 1e-9, "{r:?}");
}

#[test]
fn snap_quadrant_rounds_to_axes() {
    assert_eq!(snap_quadrant(43.0), 0.0);
    assert_eq!(snap_quadrant(46.0), 90.0);
    assert_eq!(snap_quadrant(-90.0), 270.0);
    assert_eq!(snap_quadrant(360.0), 0.0);
}
```

- [ ] **Step 10: build + test geom**

Run: `cargo test -p geom`
Expected: PASS.

- [ ] **Step 11: commit**

```bash
git add crates/geom
git commit -m "feat(geom): canonical Point2/Rect, segment kernel, rotation, tolerance tiers"
```

---

### Task 2: `pcb-model` adopts `geom` primitives

**Files:**
- Modify: `crates/pcb-model/Cargo.toml`, `crates/pcb-model/src/lib.rs`, `crates/pcb-model/src/place.rs`, `crates/pcb-model/src/route.rs`
- Delete: `crates/pcb-model/src/geom2d.rs`

- [ ] **Step 1: add geom dep** to `crates/pcb-model/Cargo.toml`:

```toml
geom = { path = "../geom" }
```

- [ ] **Step 2: delete the local `Point2`** (`lib.rs:123-148`), the local `Rect` (`lib.rs:371-438`), and the local `Bounds` (`lib.rs:288-296`). Replace with re-exports near the top of `lib.rs`:

```rust
pub use geom::{Point2, Rect};
```

- [ ] **Step 3: replace `Bounds` with `Rect` everywhere in `pcb-model`.** `Bounds` field order was `min_x,max_x,min_y,max_y`; `Rect` is `min_x,min_y,max_x,max_y`. serde stays camelCase so `{minX,maxX,minY,maxY}` JSON still parses. Mechanical rule: `Bounds {` → `Rect {` (named fields, order-independent); `bounds: Bounds` → `bounds: Rect` (`lib.rs:163`, `place.rs` `PlaceProblem.bounds`). Construction sites that used `Bounds { min_x, max_x, min_y, max_y }` keep the same named fields.

- [ ] **Step 4: delete `geom2d.rs`** and its module declaration in `lib.rs`. The old free functions become method calls (mechanical mapping; the compiler enumerates the sites in `pcb-model` and its dependents):

  | Old free fn | New method call |
  |---|---|
  | `point_seg_dist(p, a, b)` | `Segment::new(a, b).dist_to_point(p)` |
  | `seg_seg_dist(a, b, c, d)` | `Segment::new(a, b).dist_to_segment(Segment::new(c, d))` |
  | `segments_intersect(a, b, c, d)` | `Segment::new(a, b).intersects(Segment::new(c, d))` |
  | `cross(o, a, b)` | `o.orient(a, b)` |
  | `point_rect_dist(p, min, max)` | `rect.dist_to_point(p)` (caller builds the `Rect`) |
  | `seg_rect_dist(a, b, min, max)` | `Segment::new(a, b).dist_to_rect(&rect)` |

- [ ] **Step 5: migrate `place.rs` helpers to `geom`.**
  - `rotate_offset(&Point2, i32)` (:340) → delete; callers use `p.rotate(deg as f64)` after snapping. (Quadrant table and `Point2::rotate` agree at 0/90/180/270 — verified.)
  - `snap_rotation(i32)->i32` (:297) → delete; callers use `geom::snap_quadrant(deg)` (returns `f64`). (Angle field migration lands fully in Task 5; for now keep `Placement.rotation: i32` and call `geom::snap_quadrant(r as f64) as i32` at use sites to keep this task compiling.)
  - Keep `rotated_courtyard_half`, `rotated_copper_bbox`, `rect_overlap`, `compute_hpwl`, `pad_world` in `place.rs` (domain-coupled: they take `Part`/`PlaceProblem`), but have them call `geom` internals (`Point2::rotate`, `geom::Rect`). `rect_overlap` is renamed in Task 6.
  - `Point2` is now `Copy`: drop `.clone()` on points and adjust `&Point2` → `Point2` where ergonomic (compiler-guided).

- [ ] **Step 6: build + test**

Run: `cargo test -p pcb-model`
Expected: PASS (including the camelCase round-trip test `lib.rs:~448`; update it if it referenced `Bounds`).

- [ ] **Step 7: build everything that depends on pcb-model** (they only see re-exported `Point2`/`Rect`, so changes are minimal but `Bounds`→`Rect` and `Copy`-ness ripple):

Run: `cargo build -p drc-core -p drc-lint -p grid-astar -p negotiated-mesh -p pcb-place -p pcb-synth -p specctra -p kicad-sexpr -p gordian-core`
Fix each error with the mechanical rules above (the compiler is the worklist). Common fixes: `Bounds` → `Rect`, `&Point2`/`.clone()` → `Copy`, `pcb_model::geom2d::<fn>` → the `Segment`/`Rect`/`Point2` method from the Step 4 table.

- [ ] **Step 8: commit**

```bash
git add -A
git commit -m "refactor(pcb-model): use geom Point2/Rect/segment; drop Bounds and geom2d"
```

---

### Task 3: dedup rotation + `rotated_aabb_half`

**Files:** `crates/kicad-sexpr/src/pcb.rs`, `crates/kicad-sexpr/src/footlib.rs`, `crates/pcb-synth/src/placefp.rs`, `crates/gordian-core/src/tools_pcb/engine_svg.rs`, and their `Cargo.toml` (add `geom = { path = "../geom" }` if absent).

- [ ] **Step 1:** delete the three `rotated_aabb_half` bodies; replace calls with `geom::rotated_aabb_half(w, h, deg)`.
- [ ] **Step 2:** delete `kicad-sexpr/src/pcb.rs:332` `rotate_offset(dx,dy,deg)`; replace `let (rx, ry) = rotate_offset(dx, dy, deg)` with `let r = Point2::new(dx, dy).rotate(deg)` and use `r.x`/`r.y`.
- [ ] **Step 3:** delete `gordian-core/.../engine_svg.rs:367` `rotate_offset`; replace with `off.rotate(rot as f64)`.
- [ ] **Step 4:** delete `kicad-sexpr/src/pcb.rs:197` `snap_quadrant(f64)->i32`; replace with `geom::snap_quadrant(deg)` (now `f64`; cast at the `i32` storage site until Task 5).
- [ ] **Step 5: verify**

Run: `cargo test -p kicad-sexpr -p pcb-synth && cargo build -p gordian-core`
Expected: PASS. The `kicad-sexpr` pad round-trip test (`tests/pcb_roundtrip.rs` `pad_positions_match_hand_math`) MUST still pass — it is the rotation oracle.

- [ ] **Step 6: commit**

```bash
git add -A
git commit -m "refactor: route rotation/AABB/snap_quadrant through geom; delete 3x dupes"
```

---

### Task 4: dedup `half_perimeter` + `simplify`

**Files:** `crates/grid-astar/src/router.rs`, `crates/negotiated-mesh/src/pathing.rs`, `crates/negotiated-mesh/src/detail.rs` (+ Cargo deps already include geom transitively via pcb-model; add direct `geom` dep).

- [ ] **Step 1:** replace the three `half_perimeter(conn)` bodies with a one-line wrapper: `geom::Rect::bounding(&conn.points_to_connect).map_or(0.0, |r| r.half_perimeter())` (keep the `Connection` wrapper local; only the math moves).
- [ ] **Step 2:** delete `negotiated-mesh/src/detail.rs:1080` `simplify` and the body of `grid-astar/src/router.rs:786` `simplify`; route both to `geom::Polyline::new(path).simplify().into_points()`.
- [ ] **Step 3:** delete `negotiated-mesh/src/detail.rs:533` `point_seg_dist2` → `Segment::new(a, b).dist2_to_point(p)`. Delete `negotiated-mesh/src/pipeline.rs` `seg_point_dist` and `gordian-core/.../route.rs` `seg_point_dist` → `Segment::new(a, b).dist_to_point(p)`. Delete `negotiated-mesh/src/crossing.rs` `point_rect_gap` → `rect.dist_to_point(p)`.
- [ ] **Step 4: verify** — the router net-order test (`grid-astar/src/router.rs` `net_order_is_shortest_half_perimeter_first`) and the `simplify` tests are the oracles.

Run: `cargo test -p grid-astar -p negotiated-mesh`
Expected: PASS.

- [ ] **Step 5: commit**

```bash
git add -A
git commit -m "refactor: dedup half_perimeter/simplify/point-seg-dist into geom"
```

---

### Task 5: one angle convention — `f64` degrees everywhere

**Files:** `crates/pcb-model/src/place.rs` (`Placement.rotation`, `LockedAt.rotation`), and every consumer of those fields: `pcb-place/src/**`, `pcb-synth/src/{synth,placefp}.rs`, `kicad-sexpr/src/pcb.rs`, `gordian-core/src/tools_pcb/{place,export,engine_svg,route}.rs`, `specctra/src/lib.rs`.

- [ ] **Step 1:** change `Placement.rotation: i32` → `f64` and `LockedAt.rotation: i32` → `f64` in `crates/pcb-model/src/place.rs`. serde field name unchanged.
- [ ] **Step 2:** remove every `as f64` / `as i32` shim introduced in Tasks 2–3 around rotation. Use `geom::snap_quadrant(deg) -> f64` directly. Footprint synth still rejects non-quadrant angles via `synth.rs` (compare `snap_quadrant(r) == r` within `geom::EPS`).
- [ ] **Step 3:** update any `match rot { 90 => …}` integer matches to compare snapped `f64` (e.g. `match snap_quadrant(rot) as i32`), or refactor to `Point2::rotate`.
- [ ] **Step 4: verify**

Run: `cargo test -p pcb-model -p pcb-place -p pcb-synth -p kicad-sexpr && cargo build -p gordian-core -p specctra`
Expected: PASS. Update the placement determinism JSON snapshot tests (`pcb-place/.../tests.rs:892-897`) — rotation now serializes as `90.0` not `90`; regenerate the expected strings.

- [ ] **Step 5: commit**

```bash
git add -A
git commit -m "refactor: rotations are f64 degrees CCW workspace-wide; one snap_quadrant"
```

---

### Task 6: one overlap predicate; rename the penetration metric

**Files:** `crates/pcb-model/src/place.rs`, `crates/sch-io/src/label.rs`, `crates/sch-io/src/write/mod.rs`, `crates/sch-floorplan/src/floorplan/place/score.rs`, and call sites.

- [ ] **Step 1:** rename `pcb-model::place::rect_overlap` → `rect_axis_penetration` (it returns `(ox, oy)`, a clearance metric — not a boolean). Update `courtyard_overlap`, `part_keepout_overlap`, `is_legal` call sites. Keep its body (delegating to `geom::Rect` math where natural).
- [ ] **Step 2:** delete `sch-io::label::boxes_overlap` (`label.rs:20`), `sch-io::write::mod::boxes_overlap`, and `sch-floorplan::score::rects_overlap` (`score.rs:55`). Replace all callers with `geom::Rect::overlaps`. This resolves the documented strict-vs-EPS mismatch (the `geom` predicate is open+EPS). The schematic now builds `geom::Rect` (Task 7 makes `item_rect`/`label_box`/`wire_box`/`pin_text_boxes` return `Rect`).
- [ ] **Step 3: verify**

Run: `cargo test -p pcb-model -p sch-io -p sch-floorplan`
Expected: PASS. If the schematic readability lint or text solver tests shift due to the strict→EPS unification, inspect with `tools/schematic_critic.py`; only re-baseline if the critic confirms no regression.

- [ ] **Step 4: commit**

```bash
git add -A
git commit -m "refactor: single Rect::overlaps predicate; rename rect_overlap -> rect_axis_penetration"
```

---

### Task 7: schematic `[f64;2]`/`[f64;4]` → `geom::Point2`/`geom::Rect`

This is the largest mechanical sweep. Do it crate-by-crate, compiling after each. The compiler enumerates every site; the rules below are exhaustive for the transformation.

**Files (in dependency order):** `crates/geom/src/shape.rs` & `grid.rs` (signatures), then `crates/kicad-symbol/src/geometry.rs`, `crates/sch-place/src/**`, `crates/sch-io/src/**`, `crates/sch-floorplan/src/**`, `crates/greedy-place/src/lib.rs`, `crates/anneal-place/src/lib.rs`.

**Mechanical rules:**
- `[f64; 2]` used as a point → `Point2`. Element access `p[0]`/`p[1]` → `p.x`/`p.y`. Literal `[a, b]` point → `Point2::new(a, b)`.
- `[f64; 4]` rect / `type BBox = [f64;4]` → `Rect`. `b[0..3]` → `b.min_x/min_y/max_x/max_y`. Builders (`rect_from_corners`, `label_box`, `wire_box`, `item_rect`, `pin_text_boxes`) return `Rect`.
- `Pt`/`Path` aliases (`sch-io/src/wire.rs:14,17`) → `Point2` / `Vec<Point2>`.
- `point_on_segment(p, a, b)` (via `sch_place::geom`) → `Segment::new(a, b).contains_point(p)`.
- `geom::grid::snap_point([f64;2])` → `(Point2)`; `Dir::vec` returns `Point2` (Step 1).

- [ ] **Step 1:** migrate `geom` schematic-facing helpers to `Point2`: `Dir::vec -> Point2`, `grid::snap_point(Point2) -> Point2`. Delete the free `shape::transform_offset` (callers → `Point2::transform`) and `shape::point_on_segment` (callers → `Segment::contains_point`). `cargo build -p geom`. (These were left on `[f64;2]` in Task 1 to keep the schematic compiling; they move now with their callers.) After this, `shape.rs` holds only `Dir`.
- [ ] **Step 2:** `kicad-symbol`: change `PinGeom.at: [f64;2]` → `Point2`; `approx_size() -> [f64;2]` → `Point2` (or keep `(f64,f64)`? no — return `Point2`). Update `geometry.rs` and its tests. `cargo test -p kicad-symbol`. (Y-up still; the flip is Task 8.)
- [ ] **Step 3:** `sch-place` (`item.rs` `Item.at`, `netclass.rs`, `ir.rs`, `place.rs`, `result.rs`). `cargo test -p sch-place`.
- [ ] **Step 4:** `sch-io` (`label.rs`, `wire.rs`, `write/**`). Builders return `Rect`. `cargo test -p sch-io`.
- [ ] **Step 5:** `sch-floorplan` (`floorplan/place/**`, `contract.rs`). `cargo test -p sch-floorplan`.
- [ ] **Step 6:** `greedy-place`, `anneal-place`. `cargo test -p greedy-place -p anneal-place`.
- [ ] **Step 7: full schematic verify** — `cargo test -p sch-place -p sch-io -p sch-floorplan -p greedy-place -p anneal-place` then `tools/schematic_critic.py` on a sample schematic; confirm metrics unchanged (this is a pure representation change — values MUST be identical).
- [ ] **Step 8: commit**

```bash
git add -A
git commit -m "refactor: schematic stack uses geom Point2/Rect; delete [f64;2]/[f64;4]/Pt"
```

---

### Task 8: keystone — normalize symbol geometry to y-down

Now that all points are `Point2` and `Point2::transform` is the single symbol→sheet transform, flip the one y-up pocket at the loader and collapse the special sheet-flip.

**Files:** `crates/kicad-symbol/src/geometry.rs`, `crates/geom/src/transform.rs`, `crates/sch-io/src/write/build.rs` (`quantize_dir`), `crates/sch-io/src/label.rs` (`pin_text_boxes`), `crates/sch-place/src/netclass.rs` (`pin_side`), `crates/sch-floorplan/src/floorplan/place/idioms.rs` (`orient_angle`), `crates/sch-floorplan/src/floorplan/place/infer.rs` (edge ranking).

- [ ] **Step 1: flip at the boundary.** In `kicad-symbol/src/geometry.rs` `pin_geom`, store y-down:

```rust
at: Point2::new(p.at?[0], -p.at?[1]),
angle: -p.angle.unwrap_or(0.0),
```

Update the `PinGeom`/`approx_size` docstrings: pin geometry is now y-down. `raw_definition` is untouched (verbatim y-up sexpr for `(lib_symbols)`).

- [ ] **Step 2: collapse the flip.** In `Point2::transform` (`point.rs`), confirm it is exactly `mirror + rotate` with no residual Y negation (already written that way in Task 1 Step 3 — confirm no `-ry`).
- [ ] **Step 3: fix the y-up-reasoning sites** (each previously assumed `+y = up`):
  - `pin_side` (`netclass.rs`): `+y` now means South, `-y` North. Swap the North/South arms.
  - `orient_angle` (`idioms.rs`): delete the manual `-(dx*s + dy*c)` flip; use `Point2::rotate` on the y-down pin delta.
  - `infer.rs` edge ranking: the `-p.at[1]` / descending-y sorts assumed y-up; flip the sort sign so "top of body" is min-y.
  - `pin_text_boxes` (`label.rs`): the local `[cosθ, sinθ]` body direction is now y-down; drop the per-corner flip and use `Point2::transform`.
  - `quantize_dir` (`build.rs:756-774`): build the outward vector in y-down and drop the `sy = -ry`; or replace the whole body with `Point2::rotate` + `Dir` classification.
- [ ] **Step 4: regenerate the canaries.** These goldens encode old y-up local geometry and WILL change — recompute by hand and update:
  - `kicad-symbol/tests/geometry.rs` pin-`y` sign assertions (now negated).
  - `sch-io/src/label.rs` `pin_name_box_extends_into_body`, `unnamed_pin_has_only_number_box`.
- [ ] **Step 5: assert invariants.** These goldens MUST stay identical (they prove the flip is correct end-to-end). If they change, the keystone math is wrong — fix the code, not the test:
  - `sch-io/.../build.rs` `pin_endpoint_angle0_matches_spike`, `pin_endpoint_rotations`, `pin_endpoint_mirror_negates_local_x`, `pin_outward_directions_quantize_per_rotation`.
- [ ] **Step 6: verify behavior.** `cargo test -p kicad-symbol -p sch-place -p sch-io -p sch-floorplan -p greedy-place -p anneal-place`. Then run `tools/schematic_critic.py` on 2–3 representative schematics before/after (use git stash to compare); placement/routing/readability metrics must be stable. Dispatch a fresh-context review subagent to scan for any remaining y-up assumption.
- [ ] **Step 7: commit**

```bash
git add -A
git commit -m "refactor(keystone): normalize symbol geometry to y-down at load; one rotation everywhere"
```

---

### Task 9: cleanup, dead-code sweep, final verification

**Files:** workspace-wide.

- [ ] **Step 1:** delete now-dead re-export shims and pass-through wrappers (per repo "zero tolerance for drifted code"): `pcb-place/src/placement/geometry.rs` forwards of moved fns; any `pub(crate) use` that only re-exported a relocated helper; `sch-floorplan/src/contract.rs` re-exports that are now identity.
- [ ] **Step 2:** sweep remaining stragglers into `geom`: `negotiated-mesh` `shared_boundary` pair (`mesh.rs` / `crossing.rs`), `rect_center` (`pathing.rs`), `inflate_clamp` core (the rect inflate+clamp, leaving the `problem` lookup local).
- [ ] **Step 3:** grep for leftover drift and fix:

Run: `rg -n "\[f64; ?2\]|\[f64; ?4\]|fn rotate_offset|rotated_aabb_half|fn half_perimeter|type Pt\b|struct Bounds|: i32.*rotation|rotation: i32" crates/`
Expected: only legitimate non-point `[f64;2]`/`[f64;4]` (e.g. raw KiCAD `at` arrays inside `kicad-sexpr`/`kicad-symbol` parse adapters, Specctra) remain. Everything else is gone.

- [ ] **Step 4:** update `geom/src/lib.rs` crate docstring to describe the full surface (primitives, kernel, transform, tolerances, grid, hashing, ids, disjoint-set).
- [ ] **Step 5: full workspace gate.**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: critic gate.** Run `tools/schematic_critic.py` and `tools/pcb_critic.py` on representative designs; confirm no metric regression vs the pre-refactor baseline.

- [ ] **Step 7: commit + push**

```bash
git add -A
git commit -m "refactor(geom): final cleanup; one geometry convention workspace-wide"
git push origin HEAD
```

---

## Self-review

**Spec coverage:** Point unification (Tasks 1,2,7), Rect/Bounds/BBox unification (Tasks 1,2,6,7), y-down keystone (Task 8), one rotation primitive (Tasks 1,3,8), f64 angle + one snap (Tasks 1,3,5), one overlap predicate + renamed penetration metric (Tasks 1,6), tolerance tiers (Task 1), dedup of `rotate_offset`/`rotated_aabb_half`/`half_perimeter`/`simplify`/`point_seg_dist*` (Tasks 3,4), cleanup of drift (Task 9). All seven convention axes covered.

**Type consistency:** `geom::Point2` (Copy struct, `new`/`dist`/`dist2`/`orient`/`rotate`/`transform`), `geom::Rect` (`new`/`from_points`/`bounding`/`width`/`height`/`half_perimeter`/`area`/`center`/`inflate`/`contains`/`overlaps`/`intersection`/`boundary_crosses`/`dist_to_point`/`dist_to_rect`/`dist_to_segment`), `geom::Segment` (`new`/`length`/`midpoint`/`contains_point`/`dist_to_point`/`dist2_to_point`/`dist_to_segment`/`intersects`/`dist_to_rect`), `geom::Polyline` (`new`/`points`/`into_points`/`bbox`/`half_perimeter`/`simplify`), `geom::Dir` (`vec`), free fns `geom::rotated_aabb_half(f64,f64,f64)->(f64,f64)` + `geom::snap_quadrant(f64)->f64` (the only two with no single-shape owner). Rotation is `Point2::rotate`; symbol→sheet is `Point2::transform`. Distance methods live on the more complex shape (`Rect`>`Segment`>`Point2`), named `dist_to_<shape>`; names used consistently across tasks.

**Open verification (not assumptions):**
1. `Polyline::simplify` must reproduce the router's exact 45° merge — Task 1 Step 6b flags porting the original predicate if the `orient`-based form differs; the router test is the oracle (Task 4).
2. Collapsing placement `1e-9` into `EPS=1e-6`: attempt in Task 6/legality paths; if `pcb-place`/`drc` tests or `pcb_critic.py` regress, reintroduce a named `geom::PLACEMENT_EPS = 1e-9` rather than forcing the merge.
3. The keystone (Task 8) is correct iff the `pin_endpoint_*` sheet goldens are byte-identical before/after; treat any change there as a bug in the flip math.
