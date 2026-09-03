#!/usr/bin/env python3
"""Netlist extraction and dataset-case generation for the `schematic` QC suite.

    python3 tools/sch_netlist.py extract SHEET.kicad_sch [-o netlist.json]
    python3 tools/sch_netlist.py cases [--dataset DIR] [--count 8]

`extract` runs `kicad-cli sch export netlist` on a human-drawn sheet and turns it
into the only thing a `dataset-*` case gives the agent: every real part with its
library id and value, and the net each of its pins sits on. Power symbols are not
parts; a pin on the `GND` net is a pin the agent should draw a GND symbol on.

`cases` picks the sheets deterministically — single sheet, every `lib_id`
resolvable in the stock KiCad 10 symbol libraries, 20-60 extracted parts, two per
size band, lowest id first — and writes a complete case directory for each.
"""

import argparse
import collections
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "quality"))
import run as harness  # noqa: E402  (the harness owns rendering and the KiCad CLI)

DATASET = Path("/home/mimi/kicad-scraper/dataset")
BANDS = ((20, 25), (26, 31), (32, 37), (38, 60))
PER_BAND = 2
ANONYMOUS = re.compile(r"^(?:Net-\(|unconnected-\()")

PROMPT = """Draw this circuit as a schematic from the netlist in netlist.json: every part \
with its lib id and value, every pin on exactly the net given. Do not add or remove parts. \
A pin whose net is "nc" stays unconnected. The sheet is titled "{title}". Render the \
schematic when it is done.

netlist.json:
{netlist}"""

RUBRIC = """The delivered sheet must carry the given netlist exactly — the same parts on the
same nets as the human original — pass KiCAD's ERC, and read as well as the human
sheet it was extracted from.
expect: netlist_matches_reference == true
expect: erc_errors == 0
expect: critic_vs_reference >= 8
"""


def symbol_index():
    """Every part name in the stock KiCad 10 symbol libraries, by library."""
    config, _ = harness.platform_config()
    directory = Path(config["kicad"]["symbolDir"])
    index = {}
    for item in directory.iterdir():
        if item.name.endswith(".kicad_symdir"):
            index[item.name[: -len(".kicad_symdir")]] = {
                part.name[: -len(".kicad_sym")]
                for part in item.iterdir()
                if part.name.endswith(".kicad_sym")
            }
        elif item.name.endswith(".kicad_sym"):
            text = item.read_text(encoding="utf-8", errors="replace")
            index.setdefault(item.name[: -len(".kicad_sym")], set()).update(
                re.findall(r'^\t\(symbol "([^"]+)"', text, re.M)
            )
    return index


def netlist_xml(sheet, destination):
    result = subprocess.run(
        [
            harness.kicad_cli(), "sch", "export", "netlist",
            "--format", "kicadxml", "-o", str(destination), str(sheet),
        ],
        text=True, capture_output=True, timeout=300,
    )
    if not Path(destination).is_file():
        raise RuntimeError(
            f"netlist export failed for {sheet}: {(result.stderr or result.stdout).strip()}"
        )
    return ET.parse(destination).getroot()


def net_names(root):
    """`(ref, pin) -> net name`, with KiCAD's auto-names made readable."""
    anonymous = 0
    pins = {}
    for net in root.find("nets") if root.find("nets") is not None else []:
        name = net.get("name") or ""
        nodes = net.findall("node")
        if ANONYMOUS.match(name):
            if len(nodes) < 2:
                name = "nc"
            else:
                anonymous += 1
                name = f"N${anonymous}"
        elif name.startswith("/"):
            name = name.rsplit("/", 1)[-1]
        for node in nodes:
            pins[(node.get("ref"), node.get("pin"))] = name
    return pins


def extract(sheet):
    """The netlist a `dataset-*` case hands the agent."""
    with tempfile.TemporaryDirectory(prefix="sch-netlist-") as temporary:
        root = netlist_xml(sheet, Path(temporary) / "netlist.xml")
    pins = net_names(root)
    title = ""
    design = root.find("design")
    if design is not None:
        block = design.find("./sheet/title_block/title")
        title = (block.text or "").strip() if block is not None else ""
    parts = []
    for comp in root.find("components") if root.find("components") is not None else []:
        ref = comp.get("ref")
        if ref.startswith("#"):
            continue
        source = comp.find("libsource")
        if source is None:
            raise RuntimeError(f"{sheet}: {ref} has no library source")
        numbers = sorted(
            (pin.get("num") for pin in comp.findall("./units/unit/pins/pin")),
            key=lambda n: (len(n), n),
        )
        parts.append(
            {
                "ref": ref,
                "lib_id": f"{source.get('lib')}:{source.get('part')}",
                "value": (comp.findtext("value") or "").strip(),
                "pins": {number: pins.get((ref, number), "nc") for number in numbers},
            }
        )
    parts.sort(key=lambda part: (re.sub(r"\d+$", "", part["ref"]), part["ref"]))
    return {"title": title or Path(sheet).stem, "parts": parts}


# --- dataset case generation ------------------------------------------------


def library_only(sheet, index):
    ids = set(re.findall(r'\(lib_id "([^"]+)"', sheet.read_text(encoding="utf-8", errors="replace")))
    for lib_id in ids:
        library, _, part = lib_id.partition(":")
        if not part or part not in index.get(library, ()):
            return False
    return bool(ids)


def candidates(dataset, index):
    """Single-sheet, stock-library-only sheets, sized by their extracted netlist."""
    found = []
    for meta_path in sorted(dataset.glob("*.json")):
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
        sheet = meta_path.with_suffix(".kicad_sch")
        if meta["metrics"]["sheets"] or not sheet.is_file():
            continue
        if not library_only(sheet, index):
            continue
        netlist = extract(sheet)
        if not BANDS[0][0] <= len(netlist["parts"]) <= BANDS[-1][1]:
            continue
        found.append((meta["id"], len(netlist["parts"]), meta["description"], sheet))
    return found


def selection(dataset, per_band=PER_BAND):
    pool = candidates(dataset, symbol_index())
    chosen = []
    for low, high in BANDS:
        band = sorted(item for item in pool if low <= item[1] <= high)
        chosen.extend(band[:per_band])
    return chosen


def slug(description):
    words = re.findall(r"[A-Za-z0-9]+", description.lower())[:2]
    return "-".join(words) or "sheet"


def write_case(case, sheet, identifier, description):
    netlist = extract(sheet)
    inputs = case / "input"
    inputs.mkdir(parents=True, exist_ok=True)
    (inputs / "netlist.json").write_text(json.dumps(netlist, indent=1) + "\n", encoding="utf-8")
    (inputs / "reference.kicad_sch").write_bytes(sheet.read_bytes())
    harness.clean_render("schematic", inputs / "reference.kicad_sch", inputs / "reference.png")
    for stray in inputs.glob("reference.svg"):
        stray.unlink()
    (case / "prompt.txt").write_text(
        PROMPT.format(title=netlist["title"], netlist=json.dumps(netlist["parts"], indent=1)) + "\n",
        encoding="utf-8",
    )
    (case / "rubric.txt").write_text(RUBRIC, encoding="utf-8")
    (case / "source.json").write_text(
        json.dumps(
            {
                "dataset_id": identifier,
                "description": description,
                "source": str(sheet),
                "part_count": len(netlist["parts"]),
                "net_count": len({net for part in netlist["parts"] for net in part["pins"].values()}
                                 - {"nc"}),
            },
            indent=1,
        )
        + "\n",
        encoding="utf-8",
    )
    return len(netlist["parts"])


def build_cases(dataset, out, per_band):
    for identifier, _, description, sheet in selection(dataset, per_band):
        case = out / f"dataset-{slug(description)}-{identifier[:8]}"
        parts = write_case(case, sheet, identifier, description)
        print(f"{case.name}: {parts} parts — {description[:60]}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    one = sub.add_parser("extract")
    one.add_argument("sheet", type=Path)
    one.add_argument("-o", "--output", type=Path)
    many = sub.add_parser("cases")
    many.add_argument("--dataset", type=Path, default=DATASET)
    many.add_argument("--out", type=Path, default=ROOT / "quality" / "cases")
    many.add_argument("--per-band", type=int, default=PER_BAND)
    args = parser.parse_args()

    if args.mode == "extract":
        text = json.dumps(extract(args.sheet), indent=1) + "\n"
        if args.output:
            args.output.write_text(text, encoding="utf-8")
        else:
            sys.stdout.write(text)
        return
    build_cases(args.dataset, args.out, args.per_band)


if __name__ == "__main__":
    main()
