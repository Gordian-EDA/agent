#!/usr/bin/env python3
"""Measure the ink KiCAD really strokes for schematic text.

`kicad-cli sch export svg` writes every string twice: once as a `<text>` element
carrying its exact advance, and once as a `<g class="stroked-text">` group of
paths — the ink. This reads the ink back, which is what
`sch_model::text`'s constants are fitted to and what the
`sch-doc/tests/fixtures/asdrawn/*.ink-overlaps.tsv` fixtures hold.

  # refit the model: one glyph per cell, then read its advance and reach
  python3 tools/sch_text_ink.py glyphs OUT_DIR

  # regenerate a fixture: every pair of ink boxes a sheet draws overlapping
  python3 tools/sch_text_ink.py overlaps SHEET.kicad_sch > NAME.ink-overlaps.tsv

`KICAD_CLI` (or the `kicad.cliPath` in the agent config) selects the binary.
The stroke font is unchanged between KiCAD 9 and 10; both give these numbers.
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import uuid

NUM = r"-?\d+\.?\d*(?:[eE][-+]?\d+)?"
TEXT = re.compile(
    r'(<g transform="rotate\((' + NUM + r")\s+(" + NUM + r")\s+(" + NUM + r')\)">)'
    r'|(<text x="(' + NUM + r')" y="(' + NUM + r')"\s*\ntextLength="(' + NUM + r')"'
    r' font-size="(' + NUM + r')" lengthAdjust="spacingAndGlyphs"\s*\ntext-anchor="(\w+)"[^>]*>([^<]*)</text>)'
    r'|(<g class="stroked-text"><desc>(.*?)</desc>(.*?)</g>)',
    re.S,
)


def cli() -> str:
    return os.environ.get("KICAD_CLI", "kicad-cli")


def inked(svg: str) -> list[dict]:
    """Every string the SVG strokes, with its ink bounding box."""
    out, pending = [], None
    for match in TEXT.finditer(svg):
        if match.group(5):
            pending = {"text": match.group(11)}
        elif match.group(12) and pending is not None and pending["text"] == match.group(13):
            points = [
                (float(m.group(2)), float(m.group(3)))
                for m in re.finditer(r"([ML])\s*(" + NUM + r")\s+(" + NUM + r")", match.group(14))
            ]
            for circle in re.finditer(
                r'<circle cx="(' + NUM + r')" cy="(' + NUM + r')" r="(' + NUM + r')"', match.group(14)
            ):
                x, y, r = map(float, circle.groups())
                points += [(x - r, y - r), (x + r, y + r)]
            if points:
                pending["ink"] = [
                    min(p[0] for p in points),
                    min(p[1] for p in points),
                    max(p[0] for p in points),
                    max(p[1] for p in points),
                ]
                out.append(pending)
            pending = None
    return out


def render(sheet: str) -> list[dict]:
    with tempfile.TemporaryDirectory() as out:
        subprocess.run(
            [
                cli(),
                "sch",
                "export",
                "svg",
                "--output",
                out,
                # The drawing sheet's border letters are text too, and none of
                # it is the drawing.
                "--exclude-drawing-sheet",
                "--no-background-color",
                sheet,
            ],
            check=True,
            capture_output=True,
        )
        svg = next(f for f in os.listdir(out) if f.endswith(".svg"))
        return inked(open(os.path.join(out, svg)).read())


def dedupe(texts: list[dict]) -> list[dict]:
    """KiCAD plots each sheet twice into one SVG."""
    seen, out = set(), []
    for t in texts:
        key = (t["text"], tuple(round(v, 3) for v in t["ink"]))
        if key not in seen:
            seen.add(key)
            out.append(t)
    return out


def overlaps(sheet: str) -> None:
    """One TSV row per pair of ink boxes that intersect: text, box, text, box."""
    texts = dedupe(render(sheet))
    for i, a in enumerate(texts):
        for b in texts[i + 1 :]:
            if not (a["ink"][0] < b["ink"][2] - 0.01 and b["ink"][0] < a["ink"][2] - 0.01):
                continue
            if not (a["ink"][1] < b["ink"][3] - 0.01 and b["ink"][1] < a["ink"][3] - 0.01):
                continue
            if a["text"] == b["text"] and abs(a["ink"][0] - b["ink"][0]) < 0.01:
                continue  # the same glyph plotted twice
            box = lambda t: ",".join(f"{v:.4f}" for v in t["ink"])
            print(f"{a['text']}\t{box(a)}\t{b['text']}\t{box(b)}")


def glyphs(out_dir: str) -> None:
    """Render one printable glyph per cell and report its advance and reach.

    The advance of a glyph is the exact step a string grows by when it is
    added; the reach is how far its ink climbs above or drops below the line a
    bottom-justified string sits on ([-1.326, -0.326] font sizes).
    """
    printable = [chr(c) for c in range(0x21, 0x7F)]
    os.makedirs(out_dir, exist_ok=True)
    pitch, cols, size = 10.0, 12, 1.27
    body, meta = [], []
    for k, ch in enumerate(printable):
        x, y = 20.0 + (k % cols) * pitch, 20.0 + (k // cols) * pitch
        escaped = ch.replace("\\", "\\\\").replace('"', '\\"')
        body.append(
            f'\t(text "{escaped}"\n\t\t(exclude_from_sim no)\n\t\t(at {x} {y} 0)\n'
            f"\t\t(effects\n\t\t\t(font\n\t\t\t\t(size {size} {size})\n\t\t\t)\n"
            f'\t\t\t(justify left bottom)\n\t\t)\n\t\t(uuid "{uuid.uuid4()}")\n\t)'
        )
        meta.append((ch, x, y))
    rows = (len(printable) + cols - 1) // cols
    w, h = 20.0 + cols * pitch + 40, 20.0 + rows * pitch + 40
    sheet = os.path.join(out_dir, "glyphs.kicad_sch")
    open(sheet, "w").write(
        f'(kicad_sch\n\t(version 20250114)\n\t(generator "sch_text_ink")\n'
        f'\t(generator_version "0.1")\n\t(uuid "{uuid.uuid4()}")\n'
        f'\t(paper "User" {w:g} {h:g})\n\t(lib_symbols\n\t)\n'
        + "\n".join(body)
        + '\n\t(sheet_instances\n\t\t(path "/" (page "1"))\n\t)\n)\n'
    )
    ink = {t["text"]: t["ink"] for t in render(sheet)}
    table = {}
    for ch, x, y in meta:
        box = ink.get(ch)
        if box is None:
            continue
        table[ch] = {
            "ascent_extra": round(max(0.0, -1.326 - (box[1] - y) / size), 3),
            "descent_extra": round(max(0.0, (box[3] - y) / size - (-0.326)), 3),
        }
    print(json.dumps(table, indent=1, sort_keys=True))


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    {"overlaps": overlaps, "glyphs": glyphs}[sys.argv[1]](sys.argv[2])
