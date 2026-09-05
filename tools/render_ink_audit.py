#!/usr/bin/env python3
"""Prove that every glyph KiCAD plots survives rasterization.

A grader that reports a label the sheet really carries sends the model chasing a
ghost, and the eye cannot settle the question. KiCAD's PDF can: `pdftotext -bbox`
gives every string it plotted and where, so each one's own rectangle in the
rendered page can be checked for ink — no OCR, no judgement.

    tools/render_ink_audit.py <dir-of-kicad_sch-or-one-sch> [dpi]

Prints one line per sheet and exits nonzero if any plotted word came out blank or
any `(label ...)` in the source is missing from the plotted text.
"""
import pathlib
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

from PIL import Image

from kicad_cli import configured_kicad_cli

XHTML = {"x": "http://www.w3.org/1999/xhtml"}
# Anything lighter than this is bare paper, not a stroke.
INK = 230


def plotted_words(pdf):
    """Every string on page one as `(text, xmin, ymin, xmax, ymax, page_w, page_h)`."""
    bbox = subprocess.run(
        ["pdftotext", "-bbox", "-f", "1", "-l", "1", str(pdf), "-"],
        capture_output=True, text=True, check=True,
    ).stdout
    page = ET.fromstring(bbox).find(".//x:page", XHTML)
    size = (float(page.get("width")), float(page.get("height")))
    return [
        (word.text or "", *(float(word.get(k)) for k in ("xMin", "yMin", "xMax", "yMax")), *size)
        for word in page.findall(".//x:word", XHTML)
    ]


def blank_words(png, words):
    """The plotted words whose own rectangle holds no ink."""
    image = Image.open(png).convert("L")
    width, height = image.size
    blank = []
    for text, x0, y0, x1, y1, page_w, page_h in words:
        box = (
            max(0, int(x0 * width / page_w) - 1),
            max(0, int(y0 * height / page_h) - 1),
            min(width, int(x1 * width / page_w) + 2),
            min(height, int(y1 * height / page_h) + 2),
        )
        if box[2] <= box[0] or box[3] <= box[1]:
            blank.append(text)
        elif image.crop(box).getextrema()[0] > INK:
            blank.append(text)
    return blank


def audit(sch, out, dpi):
    kicad = configured_kicad_cli()
    pdf = out / f"{sch.stem}.pdf"
    subprocess.run(
        [kicad, "sch", "export", "pdf", "--output", str(pdf),
         "--exclude-drawing-sheet", "--no-background-color", str(sch)],
        capture_output=True, check=True,
    )
    subprocess.run(
        ["pdftoppm", "-r", str(dpi), "-png", "-singlefile", str(pdf), str(out / sch.stem)],
        capture_output=True, check=True,
    )
    words = plotted_words(pdf)
    blank = blank_words(out / f"{sch.stem}.png", words)
    drawn = {text for text, *_ in words}
    absent = sorted(
        label for label in set(re.findall(r'\(label "([^"]+)"', sch.read_text()))
        if label not in drawn
    )
    print(f"{sch.stem:34s} words={len(words):4d} blank={len(blank)} labels_not_plotted={absent}")
    return not blank and not absent


def main():
    target = pathlib.Path(sys.argv[1])
    dpi = int(sys.argv[2]) if len(sys.argv) > 2 else 200
    sheets = sorted(target.glob("*.kicad_sch")) if target.is_dir() else [target]
    out = target if target.is_dir() else target.parent
    out = out / "ink-audit"
    out.mkdir(exist_ok=True)
    if not all([audit(sheet, out, dpi) for sheet in sheets]):
        raise SystemExit("some plotted text did not survive rasterization")


if __name__ == "__main__":
    main()
