#!/usr/bin/env python3
"""Score rendered corpus fixtures with the anchored critic, and report the mean.

The critic reads about two points apart on one unchanged sheet, so a single
fixture moving by a point says nothing. This grades a set of fixtures, prints each
one, and prints the mean — which is what a placement change has to move.

    tools/corpus_critic.py <dir-of-kicad_sch> [fixture ...]

Renders each `.kicad_sch` through kicad-cli, grades it against
`quality/anchor/schematic-9.png`, and writes `<dir>/scores.json`.
"""
import concurrent.futures as futures
import json
import pathlib
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
ANCHOR = ROOT / "quality/anchor/schematic-9.png"
KICAD = ROOT / ".local/kicad-10.0.4/AppDir/usr/bin/kicad-cli"
# Seven, matching `quality/run.py`: the critic reads one unchanged sheet 1-3 points apart,
# and the mean of seven takes that spread to about 0.3 — the resolution a packing change
# needs to be visible at all.
SAMPLES = 7


def gateway():
    llm = tomllib.loads((pathlib.Path.home() / ".config/gordian/config.toml").read_text())["llm"]
    return llm["endpoint"].rstrip("/"), llm["apiKey"], llm["model"]


def render(sch: pathlib.Path) -> pathlib.Path | None:
    """`sch` as a PNG beside it, via KiCAD's PDF export and poppler.

    ImageMagick's SVG reader gives up on a big sheet with "vector graphics nested
    too deeply", which silently dropped the five largest fixtures — exactly the ones
    a packing change moves most. KiCAD's PDF carries the same drawing through the
    renderer it is written for.
    """
    png = sch.with_suffix(".png")
    if png.exists():
        return png
    pdf = sch.with_suffix(".pdf")
    subprocess.run([str(KICAD), "sch", "export", "pdf", "-o", str(pdf),
                    "--exclude-drawing-sheet", "--no-background-color", str(sch)],
                   capture_output=True)
    if not pdf.exists():
        return None
    subprocess.run(["pdftoppm", "-r", "130", "-png", "-singlefile", str(pdf),
                    str(sch.with_suffix(""))], capture_output=True)
    return png if png.exists() else None


def score(png: pathlib.Path, base, key, model) -> dict:
    out = subprocess.run(
        [sys.executable, str(ROOT / "tools/schematic_critic.py"), str(png),
         "--anchor", str(ANCHOR), "--circuit", png.stem,
         "--samples", str(SAMPLES), "--json-only"],
        capture_output=True, text=True,
        env={**__import__("os").environ, "OPENAI_BASE_URL": base,
             "OPENAI_API_KEY": key, "CRITIC_MODEL": model},
    )
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return {"score": None, "error": (out.stderr or out.stdout).strip()[:200]}


def main():
    directory = pathlib.Path(sys.argv[1])
    only = set(sys.argv[2:])
    sheets = sorted(p for p in directory.glob("*.kicad_sch")
                    if not only or p.stem in only)
    base, key, model = gateway()

    def one(sheet):
        png = render(sheet)
        if png is None:
            return sheet.stem, {"score": None, "error": "render failed"}
        return sheet.stem, score(png, base, key, model)

    with futures.ThreadPoolExecutor(max_workers=6) as pool:
        results = dict(pool.map(one, sheets))

    # The MEAN of the samples, not the rounded score: rounding throws away most of what
    # seven samples bought.
    scored = [v.get("mean", v.get("score")) for v in results.values()
              if isinstance(v.get("mean", v.get("score")), (int, float))]
    for name, verdict in sorted(results.items()):
        print(f"{name}: {verdict.get('mean', verdict.get('score'))} "
              f"{verdict.get('samples', verdict.get('error', ''))}")
    if scored:
        print(f"\nmean {sum(scored) / len(scored):.2f} over {len(scored)} sheets")
    (directory / "scores.json").write_text(json.dumps(results, indent=1))


if __name__ == "__main__":
    main()
