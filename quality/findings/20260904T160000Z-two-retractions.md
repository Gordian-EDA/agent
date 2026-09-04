# Two claims of mine the evidence refuted

## 1. The `connect` length cap did not split any net

I capped `connect` at `LONG_SIMPLE_LEN_MM` across sections, then saw
`dataset-current-sense-2995e0dd#0` return `{D12.2, D13.1, U12.3}` — one net on the human
original — as two, with two `pin_not_driven` errors the original does not have. I said the
cap's label fallback was failing to join the two ends, and reverted on that basis.

The fallback never ran. Counting the string `labelled both ends` across every result in
that run gives **zero**: the cap did not fire once on any of the fourteen cases, because
an agent almost never calls `connect` between two blocks more than 76 mm apart. The split
was agent variance — the second attempt at the same case matched the reference exactly.

The revert still stands, on the opposite reasoning: the change was a **no-op**, so the
Arduino improvement it was credited with (5,5 → 7,7) was never its doing either. The long
wires it was aimed at are real, and still need the label pair to land on one net.

## 2. Series parts are not drawn vertically because of their container's axis

The critic calls out orientation on four sheets as major: "series resistors are
predominantly drawn vertically instead of horizontally aligned with the signal flow".

My reading was that `default_pose` stands a two-pin part on end whenever its container is
a column, which conflates two different columns — a series chain, where the signal really
does run downward, and a set of parallel channels merely listed one above another. I made
the axis conditional on whether the child shares a signal net with the sibling above or
below it.

Measured on the 23-fixture corpus, it changed **one** sheet. That sheet, uart-level-
translator, got worse: the anchored critic reads it 7 [7,7,7] before and 6 [4,6,8] after,
and laying the two channels flat put `RXD1` and `TXD1` 2.54 mm apart on one line, which
the layout gate reports as a label collision.

Reverted. The orientation complaint is real and still open; the container's axis is not
where it comes from.

## What the same measurement pass did establish

- Symbol density is **not** the gap. Measuring the symbol bounding box alone, our median
  is 800 mm²/symbol against ~1195 for the human references. Every human reference fits A4.
- The earlier "2.3× less dense" figure was measured over all ink, which included ghost
  block frames — rectangles left behind at coordinates their parts no longer occupy.
  That bug is fixed (17 ghosts across 28 sheets, down to 3) and the median moved
  1051 → 800 mm²/symbol.
- What remains is a sparse **tail**, not a sparse median: every sheet above ~1600
  mm²/symbol scores human-look ≤ 4. The tail is the many-block sheets.
