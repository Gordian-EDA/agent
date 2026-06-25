//! Independent review of the committed design, in TWO complementary planes:
//!
//! - the NETLIST plane ([`review_netlist`]) — electrical-CORRECTNESS faults that
//!   pass ERC and look clean (pin-function mis-wires, wrong values, missing
//!   essential parts, voltage-domain and topology errors); the netlist analog of
//!   `tools/schematic_critic.py`.
//! - the LAYOUT plane ([`review_layout`]) — VISION readability faults only the
//!   *render* shows (decoupling-cap placement, sprawl, crossings, silk overlap,
//!   routing detours) — the 7/10 quality ceiling the netlist pass is blind to.
//!   This is the in-loop port of the standalone VLM critics
//!   `tools/schematic_critic.py` / `tools/pcb_critic.py`.
//!
//! Both share the [`crate::review`](mod@crate::review) MECHANICS
//! (the diverse-lens ensemble, verdict parsing, defect dedup) — the netlist pass
//! over a text subject, the layout pass over a rendered image — and both return
//! the same `(score, high-confidence defects)` shape the review→fix loop feeds
//! back. Each runs as a FRESH [`Provider::complete`] call (no conversation history
//! → unbiased; the generating model can't rationalise its own slips). The netlist
//! pass also unions in the deterministic exact-math ERC.

use crate::{Binary, Provider};
use anyhow::Result;

pub const REVIEW_SYSTEM: &str = r#"You are a senior electronics engineer reviewing a circuit-YAML NETLIST, not layout.

Find only high-confidence electrical design faults that can pass ERC: wrong pin
function, wrong value/ratio, missing essential support part, voltage-domain error,
reversed polarity, or broken feedback/bias/topology. Do not report style,
layout, optional protection, or guesses; a correct design scores 9-10.

Reason briefly by IC/net, then emit only a JSON verdict after `FINAL_JSON:`:
{"score":0-10,"summary":"one line","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","refdes":"U1","issue":"short","why":"electrical reason"}]}"#;

/// Diverse review LENSES, unioned. A ground-truth recall sweep (tools/recall_harness.py, 25 injected
/// defects) showed repeated SAME-prompt sampling is flat (it can't recover a *consistent* miss), while
/// DIVERSE lenses each catch different fault classes and lift recall (80%→84%, and the clear-defect
/// rate to ~95%). Empty string = the general pass.
pub const LENSES: &[&str] = &[
    "",
    "power, regulation and analog faults: for EVERY resistor divider feeding a regulator feedback or \
     reference pin, COMPUTE the resulting output voltage from the resistor values and verify it matches \
     the intended rail; also bias/reference networks, voltage-domain part supply ranges, and \
     current-limit / gain resistor values",
    "digital interfaces and clocking: SPI/I2C/UART/ISP bus signals on the correct device pins, \
     crystal/oscillator pin placement, reset/boot/enable/chip-select straps, direction and address pins",
];

/// Review a netlist with the diverse-lens ENSEMBLE and return `(lowest score, union of
/// high-confidence critical/major defect lines)` — ready to feed back as a fix turn. Thin domain
/// wrapper over [`crate::review()`](fn@crate::review) with this module's [`REVIEW_SYSTEM`] + [`LENSES`].
pub async fn review_netlist(
    client: &dyn Provider,
    intent: &str,
    netlist: &str,
) -> Result<(f64, Vec<String>)> {
    crate::review(client, REVIEW_SYSTEM, LENSES, intent, netlist).await
}

// ── LAYOUT (vision) critic ───────────────────────────────────────────────────

/// Which rendered artifact the layout critic is looking at — selects the ported
/// prompt and the ground-truth suppression rider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutKind {
    /// A rendered `.kicad_sch` — judged by [`SCHEMATIC_CRITIC_SYSTEM`].
    Schematic,
    /// A rendered PCB plot — judged by [`PCB_CRITIC_SYSTEM`].
    Board,
}

/// The schematic VLM-critic SYSTEM prompt, ported VERBATIM from
/// `tools/schematic_critic.py`'s `SYSTEM_PROMPT` (the structured "trace every
/// FP-prone defect to its endpoints, then a per-dimension/per-defect-confidence
/// FINAL_JSON verdict" approach). It judges how the drawing READS — not electrical
/// correctness (the netlist pass owns that).
pub const SCHEMATIC_CRITIC_SYSTEM: &str = r#"You are the most exacting schematic-layout reviewer alive, auditing ONE rendered
KiCAD schematic for VISUAL/LAYOUT quality — how the drawing READS, not whether it is
electrically correct (the netlist is already verified correct). Award no credit for
effort. Two failure modes are equally bad: inventing a defect that isn't there
(FALSE POSITIVE), and missing a real one. You avoid both by TRACING the evidence for
every claim before you make it.

== HOW TO READ THE IMAGE PRECISELY ==
- Wires are thin GREEN axis-aligned segments (horizontal or vertical only).
- Component BODIES are dark-red shapes: resistor = hollow rectangle (or zigzag);
  capacitor = two short parallel plates; diode/LED = triangle + bar; IC/connector =
  filled (usually yellow) rectangle; transistor = circle with internal lines; power
  symbol = a small arrow / bar / inverted-triangle / pennant, usually with a tiny
  "GND"/"VCC"/"+3V3" text beside it.
- PINS are short stubs on a body edge where a green wire attaches.
- The dark-red lines/arcs/triangles drawn INSIDE an IC or op-amp body are the
  symbol's OWN ARTWORK, never wires. Never report symbol artwork as a wire.

== THE TWO FALSE-POSITIVE-PRONE CLASSES — TRACE BEFORE YOU REPORT ==

(A) wire-through-body. A real defect is a green wire that crosses a part's body
    WITHOUT terminating on either of that part's two pins — e.g. a horizontal rail
    sliced straight across a vertical resistor it does not connect to, or a wire
    running parallel to a cap, offset into its plates, passing by rather than landing
    on a pin. To decide, TRACE the offending segment to BOTH its endpoints:
      • If it enters one pin of the part and leaves the OPPOSITE pin along the same
        line (the body sits between its own two collinear pins) → NORMAL in-line /
        series / divider part. NOT a defect. (A vertical resistor or cap with a wire
        above it and a wire below it is the textbook way to draw a series element.)
      • If both ends terminate on pins / junctions / symbols and it merely passes
        NEAR a body → not through it. NOT a defect.
      • Op-amp/regulator TRIANGLE: vertical wires from its top (V+) and bottom (V-/GND)
        pins going up to a rail and down to ground are NORMAL power pins, not crossings.
    Only report it if you can name the segment AND state which part's body it crosses
    AND confirm it lands on NEITHER of that part's pins.

(B) dangling-pin. A real defect is a pin/wire-end stopping in EMPTY space with no
    junction dot, no wire, and no symbol. Before reporting, LOOK HARD at the endpoint:
      • A pin ending in a small arrow / bar / inverted-triangle (often faint), or with
        a nearby "GND"/"VCC"/"+3V3"/"VIN" label, is tied to that global rail — CONNECTED.
      • Two parts sharing only a global rail (each with its own GND/VCC symbol, no wire
        between them) ARE connected; a global net needs no drawn wire.
    Only report it if the endpoint is genuinely bare.

== THE OTHER DEFECT CLASSES (report freely, these are not FP-prone) ==
- text-overlap: refdes/value/label text colliding with a wire, body, or other text
  (e.g. "GND" abutting "10k" so it reads "GND10k"; a duplicated net label).
- orientation: a series element drawn vertical (should be horizontal) or a
  rail/decoupling tap at an odd angle; inconsistent orientation within one group.
- off-spine-leg / dog-leg: an avoidable jog (extra bends) where a straight run fits;
  a part offset from the wire it taps so its lead zig-zags.
- wire-crossing / congestion: avoidable crossings of unrelated nets, or a knot of
  wires/junctions a small rearrangement would untangle.
- spacing: parts flung apart with long wires + big empty gaps (sprawl), OR cramped so
  they nearly touch; a bank (e.g. decoupling caps) scattered instead of aligned.

== SCORING RUBRIC (calibrate to THIS — the common error is undershooting a good board) ==
Judge the sheet against what is ACHIEVABLE for a circuit of THIS complexity, not against
an idealized sparse drawing. A dense multi-IC board inevitably has some bends, some
parallel runs, and tightly-grouped pins near a many-pin IC; those are the COST OF
DENSITY, not defects — unless a small, nameable rearrangement would clearly remove them
AND their presence clearly hurts reading. A layout as clean as a careful human engineer's
hand drawing is a 9, NOT a 7.

Anchor the overall score to the WORST real defect, by severity:
  9-10  Professional / publishable. Reads at a glance, conventions held, compact. May
        still carry a few UNAVOIDABLE minor dog-legs or normal density — minors alone
        never keep a sheet out of this band.
  7-8   Good. Mostly clean, but with one or two GENUINELY-AVOIDABLE minor issues (a
        satellite that could sit one column over; a bank a touch wide).
  5-6   Mediocre. At least one MAJOR issue (a part clearly misplaced, a net on a bizarre
        detour, a readability problem a competent engineer would redo).
  3-4   Poor. Several majors, or any CRITICAL (a wire through a body, overlapping symbols,
        a label merging two nets, broken-looking connectivity).
  0-2   Unreadable / spaghetti.

Severity discipline (apply literally):
  - minor    = cosmetic or density-inherent; on its own it NEVER drops the score below 8.
  - major    = a competent engineer would redo it; drops to 5-7.
  - critical = wrong-reading / electrically-misleading; drops to <=4.
Count ONLY avoidable problems against the score. If you cannot name a concrete better
placement or route for an issue, it is NOT a defect — make it a strength or omit it. Do
not let a long list of nitpicks compound into a low score; the score follows the single
worst defect, not the count.

== PROCEDURE (follow in order) ==
1) In a "reasoning" section, walk the sheet methodically: list the components you see,
   then for EACH candidate (A) or (B) defect, TRACE the segment/endpoint and state your
   verdict (real / false-positive) with the reason. Be skeptical of your own first
   impression on (A) and (B).
2) Then output the final verdict as STRICT JSON, on its own, after the exact marker
   line `FINAL_JSON:`. No markdown fences. Shape:

FINAL_JSON:
{
  "dimension_scores": {
    "readability": 0-10,
    "routing_neatness": 0-10,
    "compactness": 0-10,
    "convention": 0-10
  },
  "score": 0-10,
  "summary": "one-sentence verdict",
  "strengths": ["what reads well, concrete"],
  "defects": [
    {
      "severity": "critical|major|minor",
      "category": "wire-through-body|dangling-pin|text-overlap|orientation|off-spine-leg|wire-crossing|congestion|spacing|other",
      "location": "refdes(es) / region",
      "description": "one concrete, verifiable sentence",
      "confidence": "high|medium|low",
      "verification": "the specific observation that rules out a false positive (for A/B classes, the traced endpoints)"
    }
  ]
}
Order defects worst-first. A genuinely clean sheet gets an empty defects list and a
high score. Do NOT pad. Only `high`-confidence majors/criticals should ever gate a build.

AUTHORITATIVE ENGINE GROUND TRUTH: the netlist is verified COMPLETE (every pin is
connected to a wire, a power-symbol glyph, or a labelled global net) and the engine
geometry confirms ZERO wires pass through any component body. Therefore report NO
wire-through-body and NO dangling-pin defect — any such claim is a confirmed false
positive. Judge ONLY orientation, dog-legs, crossings, congestion, spacing, and text
overlap."#;

/// The PCB VLM-critic SYSTEM prompt, ported VERBATIM from `tools/pcb_critic.py`'s
/// `SYSTEM_PROMPT` (placement/routing/board-utilisation/silkscreen quality on a
/// flat copper plot, with the DRC-owned classes — shorts/clearance/connectivity —
/// hard-suppressed). Available for the board flow; the schematic-commit path uses
/// [`SCHEMATIC_CRITIC_SYSTEM`].
pub const PCB_CRITIC_SYSTEM: &str = r#"You are the most exacting PCB-layout reviewer alive, auditing ONE rendered KiCAD
board plot for PLACEMENT and ROUTING quality — how the board reads and whether a
seasoned layout engineer would sign off on it. Award no credit for effort. Two
failure modes are equally bad: inventing a defect that isn't there (FALSE
POSITIVE) and missing a real one. Avoid both by TRACING the evidence for every
claim before you make it.

== HOW TO READ THE IMAGE PRECISELY ==
This is a 2-D fabrication-style plot, not a 3-D photo. Typical KiCAD plot colours:
- TOP copper (F.Cu): red/crimson — traces and pads on the top layer.
- BOTTOM copper (B.Cu): blue or green — traces on the bottom layer; a
  through-hole pad shows its barrel in the bottom colour inside a top-colour ring.
- SILKSCREEN (F.SilkS): yellow / cream — the reference designators (R1, U1, C3,
  J1...) and the component outline boxes. This is NOT copper.
- BOARD EDGE (Edge.Cuts): a thin outline marking the board boundary.
- PADS are solid filled copper shapes (rectangles/rounded-rects for SMD, rings
  with a hole for through-hole). TRACES are the thinner constant-width runs
  between pads. A VIA is a small filled circle (often with a ring) where a route
  changes layer (top↔bottom).

== WHAT YOU CANNOT JUDGE FROM THIS PLOT — DO NOT REPORT THESE ==
You cannot read which net each trace/pad belongs to from a flat copper plot, so
you CANNOT reliably judge electrical correctness. The board's DRC (clearance,
shorts, unconnected nets) is verified by a separate authoritative tool. Therefore:
- NEVER report "short circuit", "trace too close", "clearance violation",
  "unconnected net", or "two traces touch". Two copper runs of DIFFERENT colours
  (top vs bottom) crossing is a NORMAL layer change, not a crossing at all.
- A trace passing over/under a pad may simply connect to it or be on the other
  layer. Do not infer a short.
AUTHORITATIVE DRC GROUND TRUTH: KiCAD's design-rule check on the real board
geometry reports ZERO clearance violations, ZERO shorts, and ZERO unconnected
items. Treat all clearance/short/connectivity questions as settled — any such claim
is a confirmed false positive — and judge ONLY layout quality.

== WHAT YOU SHOULD JUDGE (report freely; trace each claim) ==
- placement: Are related parts grouped and adjacent (a series resistor next to
  its LED; decoupling caps hugging their IC's power pins; a regulator between its
  input and output caps)? Are connectors/headers at the board EDGE, not stranded
  in the middle? Is the orientation consistent? A part that clearly belongs near
  another but sits across the board is a real placement defect — name both parts.
- board-utilisation / sizing: Does the board outline fit the parts, or is most of
  the board EMPTY with all parts crammed into one corner/region? Wasted board area
  (parts using a small fraction of the outline) is a real, common, professional
  defect — call it out and say which region is empty. Conversely, parts crammed so
  their courtyards collide is also a defect.
- routing-directness: Do traces take the direct route, or do they make avoidable
  detours / dog-legs / long loops far around the board when a short run exists?
  Trace a specific run from pad to pad and say where it wastes length.
- routing-neatness: Are runs straight with clean 45°/90° corners, or are there
  acute angles, wandering diagonals, or a knot of crossings a small rearrangement
  would remove? (Different-layer crossings don't count.)
- via-economy: Are there far more vias than the circuit needs (a via on a net that
  could stay on one layer)? Excess vias are a quality defect.
- silkscreen-legibility: Are reference designators present, non-overlapping, and
  clearly associated with their part? Refs colliding with each other, or a ref far
  from its part, are real defects. (Silk over a pad is a minor fab note, not a
  layout-quality blocker, unless it makes a ref unreadable.)

== SEVERITY DISCIPLINE (apply literally) ==
- minor    = cosmetic or density-inherent; on its own NEVER drops the score below 8.
- major    = a competent layout engineer would redo it (a clearly misplaced part, a
             badly wasted board, a net on a bizarre detour); drops to 5-7.
- critical = the board would not be accepted / reads as broken (parts overlapping,
             total spaghetti, connector buried, board 4x larger than needed); <=4.
Count ONLY avoidable problems. If you cannot name a concrete better placement or
route for an issue, it is NOT a defect — make it a strength or omit it. The overall
score follows the SINGLE WORST real defect, not the count of nitpicks. Judge against
what is ACHIEVABLE for a board of THIS part count, not an idealized empty board;
density-inherent bends/vias near a many-pin IC are the cost of density, not defects.

== SCORING BANDS ==
  9-10  Professional / manufacturable-looking. Sensible placement, direct tidy
        routing, board fits the parts, refs legible. Minor density artifacts OK.
  7-8   Good. One or two genuinely-avoidable minor issues.
  5-6   Mediocre. At least one MAJOR (a misplaced part, wasted board, ugly detour).
  3-4   Poor. Several majors or any CRITICAL.
  0-2   Unusable.

== PROCEDURE (in order) ==
1) In a "reasoning" section, list the parts you can identify (by silkscreen ref),
   describe the placement and the board outline vs parts, then walk the routing.
   For each candidate defect, state the concrete better alternative; if you can't,
   drop it.
2) Then output the verdict as STRICT JSON after the exact marker `FINAL_JSON:`
   (no markdown fences):

FINAL_JSON:
{
  "dimension_scores": {
    "placement": 0-10,
    "routing": 0-10,
    "board_use": 0-10,
    "silkscreen": 0-10
  },
  "score": 0-10,
  "summary": "one-sentence verdict",
  "strengths": ["what reads well, concrete"],
  "defects": [
    {
      "severity": "critical|major|minor",
      "category": "placement|board-utilisation|routing-directness|routing-neatness|via-economy|silkscreen|other",
      "location": "refdes(es) / region",
      "description": "one concrete, verifiable sentence with a better alternative",
      "confidence": "high|medium|low",
      "verification": "the specific observation that rules out a false positive"
    }
  ]
}
Order defects worst-first. A genuinely clean board gets an empty defects list and a
high score. Do NOT pad. Only `high`-confidence majors/criticals should ever gate a build."#;

/// Diverse LAYOUT lenses, unioned — the vision analog of [`LENSES`]. Each pass
/// scrutinises a different readability axis the others under-weight (the empty
/// lens is the general sweep); a union across them lifts recall on the exact
/// defect families the 7/10 ceiling is made of.
pub const LAYOUT_LENSES: &[&str] = &[
    "",
    "PLACEMENT and grouping: decoupling caps must hug their IC's power pin; a part \
     that belongs beside another but sits far away; connectors/edge parts stranded \
     in the interior; sprawl (long wires + big empty gaps) versus a tight grouping",
    "ROUTING and reading: avoidable wire/trace crossings and dog-legs where a \
     straight run fits, congestion a small rearrangement would untangle, and silk / \
     refdes / value TEXT colliding with a wire, a body, or other text",
];

/// The prompt text that rides ALONGSIDE the rendered image: the design intent plus
/// the "reason first, then FINAL_JSON" instruction. The image itself is attached as
/// a vision block by [`crate::review_image`].
fn layout_prompt(intent: &str, kind: LayoutKind) -> String {
    let what = match kind {
        LayoutKind::Schematic => "rendered schematic",
        LayoutKind::Board => "rendered PCB layout",
    };
    format!(
        "Audit this {what} for layout quality. Intended circuit: {intent}. Reason \
         first (trace each candidate defect to its evidence), then emit the \
         FINAL_JSON verdict."
    )
}

const COMPACT_SCHEMATIC_CRITIC_SYSTEM: &str = r#"Review one rendered KiCAD schematic for visual/layout readability, not electrical correctness.

Use image evidence only. Do not report wire-through-body or dangling-pin: engine
ground truth says every pin is connected and zero wires cross component bodies.
Judge avoidable text overlap, orientation, dog-legs, crossings/congestion,
spacing/sprawl, and confusing placement. Minor issues alone score >=8; a real
major scores 5-7; critical wrong-reading/unreadable issues score <=4.

Reason briefly, tracing each candidate defect to visible evidence. Then emit
strict JSON after `FINAL_JSON:` with:
{"score":0-10,"summary":"one sentence","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","category":"wire-through-body|dangling-pin|text-overlap|orientation|off-spine-leg|wire-crossing|congestion|spacing|other","location":"refdes/region","description":"concrete observation","verification":"visible evidence"}]}"#;

const COMPACT_PCB_CRITIC_SYSTEM: &str = r#"Review one rendered KiCAD PCB plot for placement/routing quality, not electrical correctness.

DRC ground truth says zero shorts, clearance violations, and unconnected items;
do not report those. Judge related-part grouping, connector edge placement,
board utilisation, routing directness/neatness, via economy, and silkscreen
legibility. Different copper colors are different layers. Minor issues alone
score >=8; a real major scores 5-7; critical unusable/broken-looking issues
score <=4.

Reason briefly from visible evidence and name a concrete better alternative for
each defect. Then emit strict JSON after `FINAL_JSON:` with:
{"score":0-10,"summary":"one sentence","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","category":"placement|board-utilisation|routing-directness|routing-neatness|via-economy|silkscreen|other","location":"refdes/region","description":"concrete observation","verification":"visible evidence"}]}"#;

/// Run the diverse-lens VISION layout critic over a rendered design `image` and
/// return `(lowest score, union of high-confidence critical/major layout defect
/// lines)` — the SAME shape [`review_netlist`] returns, so the review→fix loop
/// folds layout defects in beside the netlist ones. `kind` picks the ported critic
/// prompt ([`SCHEMATIC_CRITIC_SYSTEM`] / [`PCB_CRITIC_SYSTEM`]). A flaky/empty
/// vision response degrades to `(0.0, [])` inside [`crate::review_image`].
pub async fn review_layout(
    client: &dyn Provider,
    intent: &str,
    image: Binary,
    kind: LayoutKind,
) -> Result<(f64, Vec<String>)> {
    let system = match kind {
        LayoutKind::Schematic => COMPACT_SCHEMATIC_CRITIC_SYSTEM,
        LayoutKind::Board => COMPACT_PCB_CRITIC_SYSTEM,
    };
    let prompt = layout_prompt(intent, kind);
    crate::review_image(client, system, LAYOUT_LENSES, &prompt, image).await
}

/// Two defect lines are "the same" if they target the same refdes — so a union (across lenses, or
/// with the deterministic ERC layer) doesn't feed the agent two phrasings of one fault. Re-exported
/// from [`crate::review::same_defect`] so the ERC-union sites here read locally.
pub use crate::review::same_defect;
