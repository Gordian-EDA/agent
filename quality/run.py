#!/usr/bin/env python3
"""Run Gordian quality cases: deterministic facts first, one VLM judge second.

A case has `rubric.txt`, `input/`, and `prompt.txt`. One prompt is one agent run,
carried to completion: the harness never asks the agent to continue. The
artifacts it leaves behind are measured, not guessed at. Every
schematic case records what actually happened to the file — which symbols moved,
which fields were dropped, how the net partition changed, what KiCAD's own ERC
says — and a rubric may assert on any of it:

    expect: erc_errors == 0
    expect: unchanged_symbols_moved == []
    expect: len(symbols_added) >= 2

Those checks are evaluated here, not by the model. A failed check caps the score
at 3 whatever the judge thought.
"""

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
import hashlib
import html
import json
import math
import mimetypes
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import threading
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
CASES = Path(__file__).resolve().parent / "cases"
REFERENCES = Path(__file__).resolve().parent / "references"
# The human sheet every schematic critic score is calibrated against: equal to it
# is a 9, better a 10. `dataset-*` cases anchor on their own human original instead.
ANCHOR_SCHEMATIC = Path(__file__).resolve().parent / "anchor" / "schematic-9.png"
# Case inputs that describe the answer; the agent never sees them.
REFERENCE_INPUTS = {"reference.kicad_sch", "reference.png", "reference.svg"}
CRITIC_SAMPLES = 7
KICAD_DEMOS = Path(
    "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/demos"
)
VALIDATED_KICAD_CLIS = set()
CAPPED_SCORE = 3
CLEAN_RENDER_DENSITY = "200"
CLEAN_RENDER_SIZE = "1600x900"
PCB_CLEAN_LAYERS = {
    "front": "F.Cu,F.SilkS,F.Mask,Edge.Cuts",
    "back": "B.Cu,B.SilkS,B.Mask,Edge.Cuts",
}
FINDING_TAGS = (
    "tool-contract",
    "prompt",
    "engine",
    "harness",
    "judge",
    "self-diagnosis",
    "variance",
)


def platform_config():
    """The same platform configuration used by Gordian itself."""
    config_root = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    config_path = config_root / "gordian" / "config.toml"
    try:
        return tomllib.loads(config_path.read_text(encoding="utf-8")), config_path
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise RuntimeError(f"cannot read Gordian configuration from {config_path}: {error}") from error


def kicad_cli():
    """Configured KiCad 10 command-line executable."""
    cli = os.environ.get("KICAD_CLI")
    if not cli:
        config, config_path = platform_config()
        cli = config.get("kicad", {}).get("cliPath")
        if not cli:
            raise RuntimeError(f"set kicad.cliPath to KiCad 10 in {config_path}")
    cli = str(Path(cli).expanduser())
    if cli in VALIDATED_KICAD_CLIS:
        return cli
    try:
        version = subprocess.run(
            [cli, "--version"], text=True, capture_output=True, timeout=15,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError(f"cannot run configured KiCad CLI {cli}: {error}") from error
    if version.returncode or not version.stdout.strip().startswith("10."):
        detail = (version.stdout or version.stderr).strip()
        raise RuntimeError(f"configured KiCad CLI must be version 10, got {detail!r}")
    VALIDATED_KICAD_CLIS.add(cli)
    return cli


def command(args, *, timeout=600, check=True, env=None, input_text=None):
    result = subprocess.run(
        args, cwd=ROOT, text=True, capture_output=True, timeout=timeout, env=env,
        input=input_text,
    )
    if check and result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(map(str, args))}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return result


BUILT = {}
BUILD_LOCK = threading.Lock()
# Two cases may want the same cached demo render at the same time.
REFERENCE_LOCK = threading.Lock()


def cargo_target_dir():
    """Cargo target directory resolved the same way as subprocess builds."""
    configured = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    return configured if configured.is_absolute() else ROOT / configured


def facts_binary(name):
    """Path to one `quality-facts` binary, built once per run.

    Going through `cargo run` would take the workspace target-dir lock, so a
    concurrent build elsewhere on the machine could stall a case for as long as
    that build ran, and land in the timings.
    """
    override = os.environ.get(f"{name.upper()}_BIN")
    if override:
        return override
    with BUILD_LOCK:
        if name not in BUILT:
            command(
                ["cargo", "build", "--release", "-p", "quality-facts"], timeout=1800
            )
            BUILT[name] = str(cargo_target_dir() / "release" / name)
        return BUILT[name]


def facts_json(name, *args):
    """Run a facts binary and read its JSON, or report why it could not."""
    result = command(
        [facts_binary(name), *(str(a) for a in args)], check=False
    )
    if result.returncode:
        return {"error": (result.stderr or result.stdout).strip()}
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        return {"error": f"{name} returned no JSON: {error}"}


def prepare_project(case, project):
    """Copy the case's starting files into the run directory.

    A case's `input/` is the project as the agent finds it: plain KiCad files.
    Reference answers are held back — the agent never sees them.
    """
    source = case / "input"
    if not source.is_dir():
        return
    for item in source.iterdir():
        if item.name in REFERENCE_INPUTS:
            continue
        target = project / item.name
        if item.is_dir():
            shutil.copytree(item, target)
        else:
            shutil.copy2(item, target)


# --- deterministic board facts ---------------------------------------------


def pcb_facts(project):
    """Where every footprint sits, read from the `.kicad_pcb` itself."""
    return facts_json("pcb_facts", project)


def board_facts(project, before_project):
    """The board before against after: what moved, what changed identity, and
    whether the outline survived. A fact is recorded only when both sides were
    actually measured, so a rubric line about them fails rather than passing on
    a run that never produced a board."""
    after = pcb_facts(project)
    if not after.get("board"):
        return {"pcb_facts_error": after.get("error")}
    before = pcb_facts(before_project)
    facts = {
        "pcb_facts_error": None,
        "board_part_count": len(after["parts"]),
        "board_nets": after["nets"],
    }
    if not before.get("board"):
        return facts
    poses = {part["reference"]: part for part in before["parts"]}
    now = {part["reference"]: part for part in after["parts"]}
    reidentified = {
        reference
        for reference, part in now.items()
        if reference in poses and poses[reference]["lib_id"] != part["lib_id"]
    }
    facts.update(
        {
            "board_parts_added": sorted(set(now) - set(poses)),
            "board_parts_removed": sorted(set(poses) - set(now)),
            "board_lib_ids_changed": sorted(reidentified),
            # A part that changed package legitimately moves with it; "moved"
            # is about the parts nobody asked to touch.
            "board_parts_moved": sorted(
                reference
                for reference, part in now.items()
                if reference in poses
                and reference not in reidentified
                and poses[reference]["pose"] != part["pose"]
            ),
            "board_outline_changed": before["outline"] != after["outline"],
        }
    )
    return facts


# DRC findings that mean copper is wrong, as opposed to a silkscreen or
# courtyard complaint. `--drc-clean` is handed to the PCB critic on this.
COPPER_VIOLATIONS = {
    "clearance", "copper_edge_clearance", "copper_sliver", "shorting_items",
    "track_dangling", "via_dangling", "starved_thermal", "hole_clearance",
    "hole_near_hole", "track_width", "annular_width", "drill_out_of_range",
}


def unconnected_pair(finding):
    """One unrouted connection, named by the two things DRC could not join."""
    ends = [
        item.get("description", "?")
        for item in finding.get("items", [])
        if isinstance(item, dict)
    ]
    return " <-> ".join(ends) if ends else finding.get("description", "unconnected item")


def board_quality_facts(board, drc):
    """DRC diagnostics and cheap copper metrics, measured from the board and
    KiCad's own DRC report."""
    facts = {}
    if not isinstance(drc, dict) or "error" in drc:
        facts["board_check_error"] = (
            drc.get("error") if isinstance(drc, dict) else None
        ) or "DRC not run"
    else:
        findings = violations(drc)
        unconnected = [unconnected_pair(item) for item in drc.get("unconnected_items", [])]
        facts.update(
            {
                "board_check_error": None,
                "drc_blocking_findings": sum(
                    f.get("severity") == "error" for f in findings
                ),
                "drc_reported_findings": len(findings),
                "drc_copper_violations": sum(
                    f.get("severity") == "error"
                    and f.get("type") in COPPER_VIOLATIONS
                    for f in findings
                )
                + len(unconnected),
                "unrouted": unconnected,
            }
        )

    measured = pcb_facts(board)
    if not measured.get("board"):
        facts["board_metrics_error"] = measured.get("error", "board not read")
    else:
        facts.update(
            {
                "board_metrics_error": None,
                "via_count": measured["via_count"],
                "total_track_length": measured["total_track_length"],
            }
        )
    return facts


# --- deterministic schematic facts -----------------------------------------


def sch_facts(*args):
    return facts_json("sch_facts", *args)


def kicad_partition(schematic, out_path):
    """KiCAD's own net partition: sorted `REF.PIN` groups, power symbols and
    single-pin nets dropped so it is comparable to the extractor's, plus the
    named nets exactly as KiCAD sees them. The named form is what settles a
    connectivity question for the judge, which otherwise has only a render —
    where a wire passing behind a symbol reads as a short that is not there."""
    result = command(
        [
            kicad_cli(), "sch", "export", "netlist",
            "--format", "kicadxml", "-o", str(out_path), str(schematic),
        ],
        check=False,
    )
    if not out_path.exists():
        return None, None, (result.stderr or result.stdout).strip()
    try:
        nets = ET.parse(out_path).getroot().find("nets")
    except ET.ParseError as error:
        return None, None, f"malformed netlist export: {error}"
    named = {
        net.get("name"): sorted(
            f"{n.get('ref')}.{n.get('pin')}" for n in net.findall("node")
        )
        for net in (nets if nets is not None else [])
    }
    return normalize_partition(named.values()), named, None


def normalize_partition(groups):
    kept = [sorted(p for p in group if not p.startswith("#")) for group in groups]
    return sorted(group for group in kept if len(group) >= 2)


def duplicate_parts(facts):
    """Parts drawn twice: the same symbol and value on the same set of nets,
    where at least one net is a signal. Two 100 nF caps between the rails are a
    bank on purpose; two reset buttons on NRST/GND are one button drawn twice.
    Named after the copies, so the count is the number of parts to remove."""
    net_of = {}
    for i, group in enumerate(facts.get("partition", [])):
        for pin in group:
            net_of[pin] = i
    rails = {
        i
        for i, group in enumerate(facts.get("partition", []))
        if any(pin.startswith("#PWR") for pin in group)
    }
    seen = {}
    copies = []
    for symbol in facts.get("symbols", []):
        ref = symbol["ref"]
        if ref.startswith("#") or symbol.get("dnp"):
            continue
        pins = sorted(
            net_of[p] for p in net_of if p.rsplit(".", 1)[0] == ref
        )
        nets = tuple(sorted(set(pins)))
        if len(nets) < 2 or all(n in rails for n in nets):
            continue
        key = (symbol["lib_id"], symbol.get("fields", {}).get("Value", ""), nets)
        if key in seen and seen[key] != ref:
            copies.append(ref)
        seen.setdefault(key, ref)
    return sorted(set(copies))


def symbol_table(facts):
    table = {}
    for symbol in facts.get("symbols", []):
        table.setdefault(symbol["key"], symbol)
    return table


def pose(symbol):
    return (symbol["x"], symbol["y"], symbol["rot"], symbol["mirror"])


def compare_symbols(before, after):
    """What the edit did to the symbols that were already there.

    A part the case asked to swap is expected to change, so
    `unchanged_symbols_moved` and `fields_lost` look only at the parts whose
    `lib_id` stayed put: replacing one component never reads as trampling the
    rest, and trampling the rest is still caught.
    """
    old, new = symbol_table(before), symbol_table(after)
    shared = sorted(set(old) & set(new))
    swapped = set(k for k in shared if old[k]["lib_id"] != new[k]["lib_id"])
    kept = [k for k in shared if k not in swapped]
    return {
        "symbols_added": sorted(set(new) - set(old)),
        "symbols_removed": sorted(set(old) - set(new)),
        "unchanged_symbols_moved": [k for k in kept if pose(old[k]) != pose(new[k])],
        "symbols_reidentified": [k for k in shared if old[k]["uuid"] != new[k]["uuid"]],
        "duplicate_symbol_keys": len(after.get("symbols", [])) - len(new),
        "fields_lost": [
            [k, name]
            for k in kept
            for name in old[k]["fields"]
            if name not in new[k]["fields"]
        ],
        "fields_changed": [
            [k, name, value, new[k]["fields"][name]]
            for k in shared
            for name, value in old[k]["fields"].items()
            if name in new[k]["fields"] and new[k]["fields"][name] != value
        ],
        "lib_ids_changed": [[k, old[k]["lib_id"], new[k]["lib_id"]] for k in sorted(swapped)],
    }


def flat_net_delta(delta):
    return {f"net_delta_{key}": value for key, value in delta.items()}


def paths_broken(before_partition, kicad_nets, added_refs):
    """Pins that shared a net before and no longer reach each other.

    Inserting a series part splits a net in two, and that split is what the
    edit was asked for — `net_delta_split` cannot tell it from a severed signal
    path. Reachability can: every added part bridges its own pins, so a correct
    insertion leaves the old net's pins in one component of the after-netlist
    once those bridges are laid in. A pair still apart was cut.
    """
    if kicad_nets is None:
        return []
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    def union(a, b):
        parent[find(a)] = find(b)

    on_net = {}
    for net, nodes in kicad_nets.items():
        for node in nodes:
            union(node, net)
            on_net.setdefault(node.split(".")[0], set()).add(net)
    for ref in added_refs:
        nets = sorted(on_net.get(ref, ()))
        for net in nets[1:]:
            union(nets[0], net)

    broken = []
    for group in before_partition:
        live = [pin for pin in group if pin in parent]
        for pin in live[1:]:
            if find(pin) != find(live[0]):
                broken.append([live[0], pin])
    return sorted(broken)


def schematic_facts(project, before_project, artifacts):
    """Everything measurable about the schematic, before against after.

    A fact is recorded only when it was actually measured. Where the extractor
    or `kicad-cli` failed, the facts they would have produced stay *absent*
    rather than defaulting to zero, so a rubric line about them fails instead of
    passing on a broken run.
    """
    schematic = first_schematic(project)
    if schematic is None:
        return {"schematic_created": False}, {}

    after = sch_facts(project)
    before = sch_facts(before_project)
    facts = {"schematic_created": True, "sch_facts_error": after.get("error")}
    detail = {"sch_after": after}
    if "error" in after:
        return facts, detail

    kicad, kicad_nets, kicad_error = kicad_partition(schematic, artifacts / "netlist.xml")
    extracted = normalize_partition(after["partition"])
    facts.update(
        {
            "kicad_nets": kicad_nets,
            "sch_errors": after["errors"],
            "symbol_count": after["symbol_count"],
            "part_count": after["part_count"],
            "extractor_warnings": after["extractor_warnings"],
            "unconnected_pins": after["unconnected_pins"],
            "duplicate_parts": duplicate_parts(after),
            # An empty partition matches an empty partition; that is agreement
            # about nothing, not evidence the extractor tracks KiCAD.
            "partition_matches_kicad": bool(kicad) and kicad == extracted,
            "kicad_netlist_error": kicad_error,
            "net_count": len(extracted),
            "kicad_net_count": len(kicad) if kicad is not None else None,
        }
    )
    if "error" not in before and before["symbols"]:
        facts.update(compare_symbols(before, after))
        facts["unconnected_pins_added"] = sorted(
            set(after["unconnected_pins"]) - set(before["unconnected_pins"])
        )
        facts.update(flat_net_delta(sch_facts("--diff", before_project, project)))
        facts["paths_broken"] = paths_broken(
            normalize_partition(before["partition"]),
            kicad_nets,
            {key.split("/")[0] for key in facts["symbols_added"]},
        )
        detail["sch_before"] = before
    return facts, detail


def pin_partition(named_nets):
    """Every pin grouped by the net it sits on, power symbols dropped.

    Single-pin groups are kept: a sheet whose signals are mostly one pin and a
    label would otherwise compare as almost nothing, and shorting two of those
    pins together is exactly the mistake this has to catch.
    """
    groups = [sorted(p for p in group if not p.startswith("#")) for group in named_nets]
    return sorted(group for group in groups if group)


def offered_pins(netlist_xml):
    """Per reference designator, the pin numbers its symbol actually has.

    Read from the delivered netlist's own `libparts`, which is the INSTALLED
    library's view: KiCAD 10 renamed a USB-C receptacle's shield pin from `S1` to
    `SH` and dropped a flash chip's ninth pin, so a netlist extracted from a sheet
    drawn years earlier names pins that no longer exist to be placed.
    """
    try:
        root = ET.parse(netlist_xml).getroot()
    except (ET.ParseError, OSError):
        return {}
    parts = {}
    for part in root.iter("libpart"):
        key = (part.get("lib"), part.get("part"))
        parts[key] = {
            pin.get("num") for pin in part.iter("pin") if pin.get("num") is not None
        }
    offered = {}
    for comp in root.iter("comp"):
        source = comp.find("libsource")
        if source is None:
            continue
        pins = parts.get((source.get("lib"), source.get("part")))
        if pins:
            offered[comp.get("ref")] = pins
    return offered


def without_absent_pins(groups, offered):
    """`groups` with every pin the installed symbol does not offer removed, and
    the removed pins listed."""
    absent = sorted(
        pin
        for group in groups
        for pin in group
        if (ref := pin.split(".")[0]) in offered and pin.split(".", 1)[1] not in offered[ref]
    )
    if not absent:
        return groups, []
    gone = set(absent)
    kept = [[pin for pin in group if pin not in gone] for group in groups]
    return sorted(group for group in kept if group), absent


def repeated_pins(groups):
    """Pins the partition places on more than one net — impossible on a real sheet,
    and the signature of one reference designator worn by several parts."""
    seen, twice = set(), set()
    for group in groups:
        for pin in group:
            (twice if pin in seen else seen).add(pin)
    return sorted(twice)


def reference_netlist_facts(case, facts, artifacts):
    """Is the delivered netlist the human original's netlist, pin set for pin set?

    Net *names* are the agent's to choose; what must survive is which pins sit
    together. A reference that will not export leaves the verdict unmeasured, so
    the case fails rather than passing on a netlist nobody read.
    """
    reference = case / "input" / "reference.kicad_sch"
    if not reference.is_file():
        return {}
    _, named, error = kicad_partition(reference, artifacts / "reference-netlist.xml")
    if error or named is None:
        return {"reference_netlist_error": error or "reference netlist not exported"}
    expected = pin_partition(named.values())
    delivered = pin_partition((facts.get("kicad_nets") or {}).values())
    doubled = repeated_pins(expected)
    if doubled:
        # The human sheet annotates several parts with ONE reference designator — the
        # RS485 board draws three `R1`s, one per channel — so its own export puts that
        # designator's pins on six different nets. No sheet can reproduce that and stay
        # a sheet. The case is unmeasurable, and saying so beats failing a faithful
        # reproduction against ground truth that contradicts itself.
        return {
            "reference_netlist_error": (
                "the reference sheet reuses a reference designator, so its netlist puts "
                f"one pin on several nets: {', '.join(doubled[:4])}"
            ),
        }
    # Compare only the pins BOTH libraries agree exist. KiCAD 10 renamed a USB-C
    # receptacle's shield from `S1` to `SH`: the reference names a pin we cannot place,
    # and we place one it never names, for the one piece of metal.
    expected, absent = without_absent_pins(
        expected, offered_pins(artifacts / "netlist.xml")
    )
    delivered, renamed = without_absent_pins(
        delivered, offered_pins(artifacts / "reference-netlist.xml")
    )
    missing = [group for group in expected if group not in delivered]
    extra = [group for group in delivered if group not in expected]
    expected_refs = {pin.split(".")[0] for group in expected for pin in group}
    delivered_refs = {pin.split(".")[0] for group in delivered for pin in group}
    return {
        "reference_netlist_error": None,
        "netlist_matches_reference": bool(expected) and not missing and not extra,
        "reference_pins_absent_from_library": sorted(set(absent) | set(renamed)),
        "reference_net_count": len(expected),
        "reference_nets_missing": missing,
        "reference_nets_extra": extra,
        "reference_parts_missing": sorted(expected_refs - delivered_refs),
        "reference_parts_extra": sorted(delivered_refs - expected_refs),
    }


def by_type(report):
    """Error violations of `report`, counted per type."""
    counts = {}
    for finding in violations(report):
        if finding.get("severity") == "error":
            kind = finding.get("type", "unknown")
            counts[kind] = counts.get(kind, 0) + 1
    return counts


def reference_erc_facts(case, erc, artifacts):
    """ERC errors this sheet has that the human original does not.

    A `dataset-*` sheet is one page of a larger design, and the netlist it must
    reproduce exactly is that page's. Its inputs are driven from other pages, so
    KiCAD reports `pin_not_driven` on the human original itself — 10 of them on the
    DDR sheet, 6 on the RS485 board. Requiring zero asks a faithful reproduction to
    beat the sheet it reproduces, which it cannot do without changing the netlist
    it was told not to change.

    What it must not do is introduce errors of its own, so the comparison is per
    type: a class the original already carries is excused up to the original's
    count, and everything else is ours.
    """
    reference = case / "input" / "reference.kicad_sch"
    if not reference.is_file():
        return {}
    report = run_check("sch", reference, artifacts / "reference-erc.json")
    if not isinstance(report, dict) or "error" in report:
        return {"reference_erc_error": "reference ERC not run"}
    theirs, ours = by_type(report), by_type(erc)
    beyond = {
        kind: count - theirs.get(kind, 0)
        for kind, count in ours.items()
        if count > theirs.get(kind, 0)
    }
    return {
        "reference_erc_error": None,
        "reference_erc_errors": sum(theirs.values()),
        "erc_errors_beyond_reference": sum(beyond.values()),
        "erc_types_beyond_reference": beyond,
    }


def first_schematic(project):
    """The project's schematic, chosen deterministically so the ERC and the
    netlist export can never end up measuring different sheets."""
    found = sorted(project.glob("*.kicad_sch"))
    return found[0] if found else None


def run_check(kind, design, report_path):
    if design is None:
        return None
    result = command(
        [
            kicad_cli(), kind, "erc" if kind == "sch" else "drc",
            "--format", "json", "--severity-all", "--output", str(report_path),
            str(design),
        ],
        check=False,
    )
    if not report_path.exists():
        return {"error": (result.stderr or result.stdout).strip()}
    return json.loads(report_path.read_text(encoding="utf-8"))


def violations(report):
    if not isinstance(report, dict):
        return []
    found = list(report.get("violations", []))
    for sheet in report.get("sheets", []):
        found.extend(sheet.get("violations", []))
    return found


def severity_counts(report, prefix):
    """Error and warning counts, but only when there was a report to count.
    A design that was never checked leaves the counts out, so
    `expect: erc_errors == 0` cannot pass on a run that never got that far."""
    if not isinstance(report, dict) or "error" in report:
        reason = report.get("error") if isinstance(report, dict) else None
        return {f"{prefix}_check_error": reason or "not run"}
    findings = violations(report)
    return {
        f"{prefix}_errors": sum(v.get("severity") == "error" for v in findings),
        f"{prefix}_warnings": sum(v.get("severity") == "warning" for v in findings),
        f"{prefix}_check_error": None,
    }


def transcript_facts(artifacts):
    """What the agent actually did, read from its own event stream.

    A board that ends DRC-clean can still have been reached through a dozen
    refused calls and three outline guesses. A rubric can only say so if the
    calls themselves are measured, so every `tool ->` and every refused
    `tool <-` becomes a fact."""
    path = artifacts / "agent.stderr.txt"
    text = path.read_text(encoding="utf-8", errors="replace") if path.exists() else ""
    return {
        "tool_calls": re.findall(r"^\s*tool -> (\S+)", text, re.M),
        "refused_tools": re.findall(
            r"^\s*tool <- (\S+)(?: \([^\n]*\))?: (?:error|refused)", text, re.M
        ),
    }


def deterministic_facts(case, project, before_project, artifacts, agent_result):
    schematic = first_schematic(project)
    board = next(iter(sorted(project.glob("*.kicad_pcb"))), None)
    erc = run_check("sch", schematic, artifacts / "erc.json")
    drc = run_check("pcb", board, artifacts / "drc.json")
    board_quality = (
        board_quality_facts(board, drc) if board is not None else {}
    )
    unconnected = drc.get("unconnected_items", []) if isinstance(drc, dict) else []
    fab = (
        sorted(path.name for path in (project / "fab").glob("*"))
        if (project / "fab").is_dir()
        else []
    )
    sch, detail = schematic_facts(project, before_project, artifacts)
    pcb = board_facts(project, before_project) if board is not None else {}
    facts = {
        "agent_exit": agent_result.returncode,
        "pcb_created": board is not None,
        **pcb,
        **board_quality,
        **severity_counts(erc, "erc"),
        **severity_counts(drc, "drc"),
        "unconnected_items": len(unconnected),
        "fab_files": fab,
        **transcript_facts(artifacts),
        **sch,
    }
    if sch.get("schematic_created"):
        facts.update(reference_netlist_facts(case, facts, artifacts))
        facts.update(reference_erc_facts(case, erc, artifacts))
    return facts, detail


VISUAL_FACTS = ("body_overlaps", "text_collisions", "wires_through_bodies")


def schematic_visual_facts(project, before_project):
    """Measured drawing defects after the run, and the input sheet's counts.

    Edit rubrics compare the live list length with the input count, so the
    harness owns that comparison rather than asking the agent for turn state.
    Where the measurement failed the facts stay absent, so a rubric line about
    them fails instead of passing on a sheet nobody read.
    """
    after = sch_facts("--visual", project)
    if "error" in after:
        return {"schematic_visual_error": after["error"]}
    before = sch_facts("--visual", before_project)
    measured = {}
    for name in VISUAL_FACTS:
        if not isinstance(after.get(name), list):
            continue
        measured[name] = after[name]
        if isinstance(before.get(name), list):
            measured[f"before_{name}"] = len(before[name])
    missing = [name for name in VISUAL_FACTS if name not in measured]
    return {
        "schematic_visual_error": (
            f"sch_facts --visual omitted: {', '.join(missing)}" if missing else None
        ),
        "sheet_extent": after.get("sheet_extent"),
        **measured,
    }


# --- rubric checks ----------------------------------------------------------


IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
CHECK = re.compile(
    rf"^(?:len\((?P<counted>{IDENT})\)|(?P<name>{IDENT}))"
    r"\s*(?P<op>==|!=|<=|>=|<|>)\s*(?P<value>.+)$"
)
OPS = {
    "==": lambda a, b: a == b,
    "!=": lambda a, b: a != b,
    "<": lambda a, b: a < b,
    "<=": lambda a, b: a <= b,
    ">": lambda a, b: a > b,
    ">=": lambda a, b: a >= b,
}


def parse_rubric(text):
    """Split a rubric into the prose the judge reads and the `expect:` lines the
    runner evaluates itself."""
    prose, checks = [], []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.lower().startswith("expect:"):
            checks.append(stripped[len("expect:"):].strip())
        else:
            prose.append(line)
    return "\n".join(prose).strip(), checks


def evaluate_check(expression, facts):
    match = CHECK.match(expression)
    if not match:
        return False, "unparseable check"
    counted = match.group("counted")
    name = counted or match.group("name")
    if name not in facts:
        return False, f"not measured: {name}"
    actual = facts[name]
    if counted:
        if not isinstance(actual, (list, dict)):
            return False, f"len() needs a collection, {name} is {actual!r}"
        actual = len(actual)
    expected_text = match.group("value")
    if re.fullmatch(IDENT, expected_text) and expected_text in facts:
        expected = facts[expected_text]
    else:
        try:
            expected = json.loads(expected_text)
        except json.JSONDecodeError:
            return False, f"expected value is not JSON or a measured fact: {expected_text}"
    try:
        ok = OPS[match.group("op")](actual, expected)
    except TypeError:
        return False, f"cannot compare {actual!r} with {expected!r}"
    return ok, "" if ok else f"actual {json.dumps(actual)}"


def evaluate_checks(checks, facts):
    passed, failed = [], []
    for expression in checks:
        ok, reason = evaluate_check(expression, facts)
        (passed if ok else failed).append(
            expression if ok else f"{expression} — {reason}"
        )
    return {"pass": passed, "fail": failed}


# --- judge ------------------------------------------------------------------


def image_part(path):
    mime = mimetypes.guess_type(path)[0] or "image/png"
    encoded = base64.b64encode(Path(path).read_bytes()).decode()
    return {"type": "image_url", "image_url": {"url": f"data:{mime};base64,{encoded}"}}


def extract_object(text, key="score"):
    """The last JSON object in `text` that carries `key` — a model's verdict."""
    decoder = json.JSONDecoder()
    objects = []
    for match in re.finditer(r"\{", text):
        try:
            value, _ = decoder.raw_decode(text[match.start():])
            if isinstance(value, dict):
                objects.append(value)
        except json.JSONDecodeError:
            pass
    verdicts = [value for value in objects if key in value]
    if verdicts:
        return verdicts[-1]
    # A grader that drops one quote inside its own JSON — `"...cramped.":""]` — used
    # to cost the whole grade, and two of 28 human-look scores in one suite went to
    # nothing that way. The score and the lists are still legible; read them loosely.
    score = re.search(rf'"{re.escape(key)}"\s*:\s*(\d+(?:\.\d+)?)', text)
    if score is None:
        raise ValueError(f"model returned no object with {key!r}: {text!r}")
    loose = {key: int(float(score.group(1)))}
    for field in ("worst_three", "what_a_human_would_change", "issues"):
        block = re.search(rf'"{field}"\s*:\s*\[(.*?)\]', text, re.S)
        if block:
            loose[field] = re.findall(r'"((?:[^"\\]|\\.){8,}?)"', block.group(1))[:3]
    return loose


def llm_config():
    config, config_path = platform_config()
    try:
        llm = config["llm"]
    except KeyError as error:
        raise RuntimeError(
            f"cannot read judge configuration from {config_path}: {error}"
        ) from error
    base = str(llm.get("endpoint", "")).rstrip("/")
    key = str(llm.get("apiKey", ""))
    model = str(llm.get("model", ""))
    if not base or not key or not model:
        raise RuntimeError(f"set llm.endpoint, llm.apiKey, and llm.model in {config_path}")
    return base, key, model


def demo_instance_count(path, kind):
    """Cheap size proxy for choosing a similarly dense human demo."""
    text = path.read_text(encoding="utf-8", errors="replace")
    pattern = r"^\s*\(symbol\s*$" if kind == "schematic" else r"^\s*\(footprint\s+"
    return len(re.findall(pattern, text, re.M))


def closest_demos(kind, target_count):
    """KiCad demos ordered from the closest part count outwards."""
    suffix = ".kicad_sch" if kind == "schematic" else ".kicad_pcb"
    candidates = []
    if KICAD_DEMOS.is_dir():
        for path in KICAD_DEMOS.rglob(f"*{suffix}"):
            count = demo_instance_count(path, kind)
            if count:
                candidates.append(
                    (abs(count - target_count), path.stat().st_size, count, str(path), path)
                )
    if not candidates:
        raise RuntimeError(f"no KiCad demo {kind} references found under {KICAD_DEMOS}")
    return [(path, count) for _, _, count, _, path in sorted(candidates)]


def reference_render(kind, target_count):
    """Render and cache the closest usable KiCad 10 demo."""
    errors = []
    for source, count in closest_demos(kind, target_count):
        try:
            return render_reference_candidate(kind, source, count)
        except Exception as error:
            errors.append(f"{source}: {error}")
    detail = "; ".join(errors)
    raise RuntimeError(f"no renderable KiCad demo {kind} reference: {detail}")


def render_reference_candidate(kind, source, count):
    """Render one demo, raising so reference_render can try the next closest."""
    digest = hashlib.sha256(str(source).encode()).hexdigest()[:10]
    stem = re.sub(r"[^A-Za-z0-9_.-]+", "-", source.stem).strip("-")
    REFERENCES.mkdir(parents=True, exist_ok=True)
    png = REFERENCES / f"{kind}-{count}-{stem}-{digest}-clean-v3.png"
    with REFERENCE_LOCK:
        if not png.is_file():
            clean_render(kind, source, png)
    return {"path": str(png), "source": str(source), "part_count": count}


def human_look_judge(kind, rendered, target_count, llm):
    """Compare one final render with a closest-size human-authored KiCad demo."""
    path = rendered.get("path")
    if not path:
        return {
            "score": None,
            "worst_three": [],
            "what_a_human_would_change": [],
            "error": rendered.get("error", f"no {kind} render"),
        }
    reference = reference_render(kind, target_count)
    base, key, model = llm
    text = f"""Compare the FIRST image, an agent-created KiCad {kind}, with the SECOND
image, a human-authored KiCad demo chosen only because it has a comparable part
count. Judge visual organization and drafting/layout craft, not whether the two
circuits implement the same function. Be strict but size-aware. A score of 8 means
the agent result looks like competent human engineering work; 10 means exemplary.

Return only JSON with this exact shape:
{{"score": 0, "worst_three": [], "what_a_human_would_change": []}}

Score must be an integer from 1 to 10. Both arrays must contain short, concrete,
actionable strings, with at most three entries each. Do not report electrical,
ERC, DRC, or connectivity claims from pixels."""
    body = {
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a senior electronics drafter comparing visual workmanship.",
            },
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": text},
                    {"type": "text", "text": f"Agent {kind} under review"},
                    image_part(path),
                    {"type": "text", "text": f"Human demo reference ({reference['part_count']} parts)"},
                    image_part(reference["path"]),
                ],
            },
        ],
        "max_tokens": 3000,
    }
    request = urllib.request.Request(
        f"{base}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=240) as response:
        payload = json.loads(response.read())
    verdict = extract_object(
        payload["choices"][0]["message"].get("content") or ""
    )
    score = verdict.get("score")
    # A judge with nothing to add answers `null`, which is an empty list.
    worst = verdict.get("worst_three") or []
    changes = verdict.get("what_a_human_would_change") or []
    if not isinstance(score, int) or isinstance(score, bool) or not 1 <= score <= 10:
        raise ValueError(f"invalid human-look score: {score!r}")
    if not isinstance(worst, list) or not all(isinstance(item, str) for item in worst):
        raise ValueError(f"invalid human-look worst_three: {worst!r}")
    if not isinstance(changes, list) or not all(isinstance(item, str) for item in changes):
        raise ValueError(f"invalid human-look changes: {changes!r}")
    return {
        "score": score,
        "worst_three": worst[:3],
        "what_a_human_would_change": changes[:3],
        "reference": reference,
    }


def judge(prompt, rubric, facts, checks, renders, llm):
    base, key, model = llm
    text = f"""Review Gordian's result for the user request below.

REQUEST:
{prompt}

CASE RUBRIC:
{rubric}

AUTHORITATIVE FACTS (measured from the files, not opinions):
{json.dumps(facts, indent=2)}

MACHINE CHECKS ALREADY EVALUATED:
{json.dumps(checks, indent=2)}

`unchanged_symbols_moved` lists parts that were already in the input and whose
position or rotation changed; on an edit case that is a regression even when the
result looks fine. `fields_lost` lists properties dropped from a surviving part.
`net_delta_*` is the change to the net partition.

`kicad_nets` is KiCAD's own netlist of the delivered schematic: every net and
every pin on it. It, not the render, decides what is connected to what. Never
claim a short, a missing connection, an isolated node or a wrong topology that
`kicad_nets` contradicts — two wires crossing, a label sitting over a symbol, or
a part drawn far from its net all look wrong and are not. Judge the render for
what only it can show: readability, overlap, clipping, layout and orientation.

The images are labeled by filename as before/after schematic or PCB renders.
Return only JSON with this exact shape:
{{"score": 0, "issues": []}}

Score must be an integer from 0 to 10. Issues must be short, concrete, actionable
strings. Do not repeat an issue already disproved by the authoritative facts.
An empty issues array means no actionable issue was found."""
    content = [{"type": "text", "text": text}]
    for phase in ("before", "after"):
        for kind in ("schematic", "pcb"):
            item = renders.get(phase, {}).get(kind, {})
            if item.get("path"):
                content.append({"type": "text", "text": Path(item["path"]).name})
                content.append(image_part(item["path"]))

    body = {
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a strict senior electronics and PCB design reviewer.",
            },
            {"role": "user", "content": content},
        ],
        "max_tokens": 4000,
    }
    request = urllib.request.Request(
        f"{base}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    # A reasoning model occasionally spends the whole budget thinking and
    # answers with nothing; one retry costs less than losing a 15-minute run.
    for attempt in range(2):
        try:
            with urllib.request.urlopen(request, timeout=240) as response:
                payload = json.loads(response.read())
        except urllib.error.HTTPError as error:
            raise RuntimeError(error.read().decode(errors="replace")) from error
        answer = payload["choices"][0]["message"].get("content") or ""
        if answer.strip():
            break
    verdict = extract_object(answer)
    score = verdict.get("score")
    issues = verdict.get("issues")
    if not isinstance(score, int) or not 0 <= score <= 10:
        raise ValueError(f"invalid judge score: {score!r}")
    if not isinstance(issues, list) or not all(isinstance(i, str) for i in issues):
        raise ValueError(f"invalid judge issues: {issues!r}")
    return {"score": score, "issues": issues}


def self_diagnose(prompt, transcript, llm):
    """Ask the agent's own model what it struggled with, given its transcript.

    Cheap and pointed: tool gaps, confusing results and missing information
    surface here long before they show up as a score."""
    base, key, model = llm
    trimmed = transcript[-24000:]
    text = f"""You are the Gordian agent reviewing your own run. The user asked:

{prompt}

Your transcript (tool calls, their results, your messages):

{trimmed}

What did you struggle with in the current toolset during this task? List concrete
tool gaps, tool results that were confusing or insufficient, information you needed
and could not get, refusals you did not understand, and what would have made the
task faster or the result better. Return only JSON:
{{"struggles": ["..."], "wishes": ["..."]}}
At most six entries each, one short specific sentence naming the tool or the missing
capability."""
    body = {
        "model": model,
        "messages": [{"role": "user", "content": text}],
        "max_tokens": 6000,
    }
    request = urllib.request.Request(
        f"{base}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=240) as response:
        payload = json.loads(response.read())
    answer = extract_object(
        payload["choices"][0]["message"].get("content") or "", key="struggles"
    )
    struggles = answer.get("struggles") or []
    wishes = answer.get("wishes") or []
    return {
        "struggles": [str(item) for item in struggles],
        "wishes": [str(item) for item in wishes],
    }


def schematic_anchor(case):
    """The human sheet this case's critic score is calibrated against: the case's
    own original where there is one, else the suite-wide reference-9 sheet."""
    own = case / "input" / "reference.png"
    return (own, True) if own.is_file() else (ANCHOR_SCHEMATIC, False)


def critic(kind, rendered, prompt, facts, llm, anchor=None, same_circuit=False):
    """Run one dedicated visual critic with the judge's gateway credentials."""
    path = rendered.get("path")
    if not path:
        if rendered.get("error"):
            return {"score": None, "issues": [], "error": rendered["error"]}
        return {
            "score": None,
            "issues": [],
            "skipped": f"no {kind} render",
        }
    base, key, model = llm
    script = ROOT / "tools" / f"{kind}_critic.py"
    args = [
        sys.executable,
        str(script),
        path,
        "--circuit",
        prompt,
        "--model",
        model,
        "--json-only",
    ]
    if kind == "schematic" and anchor is not None:
        args.extend(["--anchor", str(anchor), "--samples", str(CRITIC_SAMPLES)])
        if same_circuit:
            args.append("--anchor-same-circuit")
    if kind == "pcb":
        args.extend(
            [
                "--layers-note",
                "Two panels are shown at the same scale: front copper/silkscreen/mask "
                "on the left, and the mirrored back copper/silkscreen/mask on the right; "
                "both include Edge.Cuts.",
            ]
        )
    if (
        kind == "schematic"
        and facts.get("wires_through_bodies") == []
        and facts.get("unconnected_pins") == []
        and facts.get("kicad_netlist_error") is None
    ):
        args.append("--engine-clean")
    if (
        kind == "pcb"
        and facts.get("drc_copper_violations") == 0
        and facts.get("unrouted") == []
    ):
        args.append("--drc-clean")
    result = command(
        args,
        # Seven sequential reads of a large sheet on a loaded gateway pass 300 s; when
        # the sample count went 3 -> 7 this did not follow it, and six renders of one
        # suite came back unscored with "timed out after 300 seconds". Scale with the
        # reads, not a constant.
        timeout=150 * CRITIC_SAMPLES,
        check=False,
        env={
            **os.environ,
            "OPENAI_BASE_URL": base,
            "OPENAI_API_KEY": key,
            "CRITIC_MODEL": model,
        },
    )
    try:
        verdict = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        message = (result.stderr or result.stdout).strip()
        raise RuntimeError(
            f"{kind} critic returned no JSON (exit {result.returncode}): {message}"
        ) from error
    score = verdict.get("score")
    issues = verdict.get("defects", [])
    samples = verdict.get("samples")
    if (
        isinstance(score, bool)
        or not isinstance(score, (int, float))
        or not math.isfinite(score)
        or not 0 <= score <= 10
        or not isinstance(issues, list)
        or not all(isinstance(issue, dict) for issue in issues)
    ):
        raise ValueError(f"invalid {kind} critic verdict: {verdict!r}")
    result = {"score": score, "issues": issues}
    if samples:
        result["samples"] = samples
    if kind == "schematic" and anchor is not None:
        result["anchor"] = str(anchor)
        result["anchor_same_circuit"] = same_circuit
    return result


# --- render + run -----------------------------------------------------------


def frame_render(source, png, size=CLEAN_RENDER_SIZE):
    """Trim, pad and scale one rasterized page into a judge PNG."""
    command(
        [
            "magick", "-density", CLEAN_RENDER_DENSITY, str(source),
            "-trim", "+repage", "-bordercolor", "white", "-border", "24",
            "-background", "white", "-alpha", "remove", "-alpha", "off",
            "-resize", size, str(png),
        ],
        timeout=180,
    )
    if not png.is_file():
        raise RuntimeError(f"page conversion produced no PNG: {png}")


def rasterize_schematic_pdf(source, png, temporary, size=CLEAN_RENDER_SIZE):
    """Export one schematic as PDF and rasterize it with poppler.

    KiCad's PDF is the drawing rendered by the renderer it is written for.
    ImageMagick's SVG reader is not an equivalent: it gives up on a dense sheet
    with `vector graphics nested too deeply`, which silently dropped the largest
    fixtures — exactly the ones a layout change moves most.
    """
    pdf = temporary / "schematic.pdf"
    result = command(
        [
            kicad_cli(), "sch", "export", "pdf", "--output", str(pdf),
            "--exclude-drawing-sheet", "--no-background-color", str(source),
        ],
        timeout=180,
        check=False,
    )
    if result.returncode or not pdf.is_file():
        detail = (result.stderr or result.stdout).strip()
        raise RuntimeError(f"KiCad schematic PDF export failed: {detail}")
    pages = temporary / "page"
    command(
        ["pdftoppm", "-r", CLEAN_RENDER_DENSITY, "-png", str(pdf), str(pages)],
        timeout=180,
    )
    rendered = sorted(temporary.glob("page*.png"))
    if not rendered:
        raise RuntimeError(f"PDF rasterization produced no page: {source}")
    frame_render(rendered[0], png, size)
    return pdf


def clean_render(kind, source, destination):
    """Export an unannotated KiCad 10 page and convert it to a judge PNG.

    Candidates and human-demo references both use this function so their crop,
    density, and maximum dimensions match. PCB renders show the front and a
    mirrored back view side by side, using the copper, silkscreen, mask, and
    board-edge layers a fabrication preview exposes.
    """
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    stem = destination.with_suffix("")
    with tempfile.TemporaryDirectory(prefix="gordian-quality-clean-render-") as temporary:
        temporary = Path(temporary)
        if kind == "schematic":
            pdf = rasterize_schematic_pdf(source, destination, temporary)
            kept = stem.with_suffix(".pdf")
            shutil.copy2(pdf, kept)
            return {"path": str(destination), "pdf_path": str(kept)}

        if kind != "pcb":
            raise ValueError(f"unknown render kind: {kind}")
        side_pngs = []
        svg_paths = []
        for side, layers in PCB_CLEAN_LAYERS.items():
            exported = temporary / f"{side}.svg"
            args = [
                kicad_cli(), "pcb", "export", "svg", "--output", str(exported),
                "--layers", layers, "--mode-single", "--page-size-mode", "2",
                "--fit-page-to-board", "--exclude-drawing-sheet", "--check-zones",
            ]
            if side == "back":
                args.append("--mirror")
            args.append(str(source))
            result = command(args, timeout=180, check=False)
            if result.returncode or not exported.is_file():
                detail = (result.stderr or result.stdout).strip()
                raise RuntimeError(f"KiCad PCB {side} SVG export failed: {detail}")
            kept_svg = stem.parent / f"{stem.name}-{side}.svg"
            shutil.copy2(exported, kept_svg)
            side_png = temporary / f"{side}.png"
            frame_render(kept_svg, side_png, "776x900")
            side_pngs.append(side_png)
            svg_paths.append(str(kept_svg))
        command(
            [
                "magick", str(side_pngs[0]), str(side_pngs[1]),
                "-gravity", "center", "-background", "white", "+append",
                str(destination),
            ],
            timeout=180,
        )
        if not destination.is_file():
            raise RuntimeError(f"PCB composition produced no PNG: {destination}")
        return {"path": str(destination), "svg_paths": svg_paths}


def capture_clean_render(project, kind, destination):
    source = (
        first_schematic(project)
        if kind == "schematic"
        else next(iter(sorted(project.glob("*.kicad_pcb"))), None)
    )
    if source is None:
        return {"error": f"no {kind} source"}
    try:
        return clean_render(kind, source, destination)
    except Exception as error:
        return {"error": str(error)}


def render_project(project, artifacts, prefix):
    """The judge's view of the project: KiCad's own page, exported the same way
    for a candidate and for a human reference."""
    rendered = {}
    for kind, pattern in (("schematic", "*.kicad_sch"), ("pcb", "*.kicad_pcb")):
        if next(project.glob(pattern), None):
            rendered[kind] = capture_clean_render(
                project, kind, artifacts / f"{prefix}-{kind}-clean.png"
            )
    return rendered


def agent_command(project, prompt):
    """The command one case runs.

    `GORDIAN_AGENT_CMD` replaces the whole invocation, so the harness itself can
    be exercised against a stand-in that writes a sheet and exits: the words are
    split like a shell command line and `{project}` / `{prompt}` are filled in.
    `GORDIAN_BIN` names a prebuilt agent binary and keeps the real argument list.
    """
    template = os.environ.get("GORDIAN_AGENT_CMD")
    if template:
        return [
            word.replace("{project}", str(project)).replace("{prompt}", prompt)
            for word in shlex.split(template)
        ]
    configured = os.environ.get("GORDIAN_BIN")
    if configured:
        return [configured, "agent", "--project", str(project), prompt]
    return [
        "cargo", "run", "--release", "--quiet", "-p", "gordian", "--",
        "agent", "--project", str(project), prompt,
    ]


ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
TOOL_STARTED = re.compile(
    r"^tool -> (?P<tool>[A-Za-z0-9_]+)(?:\s+(?P<args>\{.*\}))?\s*$"
)
TOOL_FINISHED = re.compile(
    r"^tool <- (?P<tool>[A-Za-z0-9_]+)"
    r"(?: \(elapsed (?P<elapsed>[0-9.]+)s\))?"
    r":\s?(?P<summary>.*)$"
)
USAGE_REQUESTS = re.compile(r"^usage: provider_requests=(\d+)\b")
USAGE_REQUEST = re.compile(
    r"^usage: request #(?P<request>\d+) in=(?P<input>\d+) out=(?P<output>\d+) "
    r"cached=(?P<cached>\d+)(?: cache_write=(?P<cache_write>\d+))? "
    r"latency=(?P<latency>[0-9.]+)s$"
)
TOTAL_REQUESTS = re.compile(r"\bprovider requests:\s*(\d+)\b")
# The agent's `review_schematic` summary line: its own anchored critic's modal
# score, so a run's self-review trajectory can be plotted against the harness's.
REVIEW_SCORE = re.compile(r"^review ([0-9.]+)/10\b")
USER_CAP = re.compile(r"MaxRequestsReached\s*\{\s*requests:\s*(\d+)\s*\}")
REFUSAL = re.compile(r"\brefus(?:e|ed|al|ing)\b", re.IGNORECASE)
TURN_STARTED = re.compile(r"^turn (?P<turn>\d+): (?P<prompt>.*)$")
TURN_DONE = re.compile(r"^turn (?P<turn>\d+) done:")
ASSISTANT_TEXT = re.compile(r"^assistant:\s*(.+)$", re.M)


def parse_agent_stderr(stderr):
    """Turn the headless agent's compact event transcript into durable facts."""
    text = ANSI.sub("", stderr)
    calls = []
    attempted = []
    usage_counts = []
    total_counts = []
    requests = []
    transcript = []
    recording = False
    for line in text.splitlines():
        stripped = line.strip()
        if TURN_STARTED.match(stripped):
            recording = True
            transcript.append(stripped)
            continue
        if recording:
            transcript.append(stripped)
            if TURN_DONE.match(stripped):
                recording = False
        if match := TOOL_STARTED.match(stripped):
            name = match.group("tool")
            attempted.append(name)
            calls.append({
                "tool": name,
                "status": None,
                "summary": "",
                "args": match.group("args") or "",
            })
            continue
        if match := TOOL_FINISHED.match(stripped):
            name = match.group("tool")
            summary = match.group("summary")
            target = next(
                (
                    call
                    for call in reversed(calls)
                    if call["tool"] == name and call["status"] is None
                ),
                None,
            )
            if target is None:
                attempted.append(name)
                target = {"tool": name, "status": None, "summary": ""}
                calls.append(target)
            target["status"] = (
                "error"
                if summary.lower().startswith(("error:", "refused:"))
                else "ok"
            )
            target["summary"] = summary
            target["elapsed_ms"] = (
                round(float(match.group("elapsed")) * 1000)
                if match.group("elapsed") else None
            )
            if " | refusal=" in summary:
                target["refusal_reason"] = summary.split(" | refusal=", 1)[1]
            continue
        if match := USAGE_REQUEST.match(stripped):
            requests.append({
                "request": int(match.group("request")),
                "input_tokens": int(match.group("input")),
                "output_tokens": int(match.group("output")),
                "cached_tokens": int(match.group("cached")),
                "cache_write_tokens": int(match.group("cache_write") or 0),
                "latency_ms": round(float(match.group("latency")) * 1000),
            })
        if match := USAGE_REQUESTS.match(stripped):
            usage_counts.append(int(match.group(1)))
        if match := TOTAL_REQUESTS.search(stripped):
            total_counts.append(int(match.group(1)))

    for call in calls:
        if call["status"] is None:
            call["status"] = "error"
            call["summary"] = "no completion recorded"

    refusals, errors = [], []
    for call in calls:
        if call["status"] != "error":
            continue
        signal = {
            "tool": call["tool"],
            "message": call.get("refusal_reason") or call["summary"],
        }
        (refusals if REFUSAL.search(call["summary"]) else errors).append(signal)

    repeated = []
    index = 0
    while index < len(attempted):
        end = index + 1
        while end < len(attempted) and attempted[end] == attempted[index]:
            end += 1
        if end - index >= 3:
            repeated.append({"tool": attempted[index], "count": end - index})
        index = end

    review_scores = [
        float(match.group(1))
        for call in calls
        if call["tool"] == "review_schematic"
        for match in [REVIEW_SCORE.match(call["summary"])]
        if match
    ]

    cap = USER_CAP.search(text)
    assistant_messages = ASSISTANT_TEXT.findall(text)
    final_assistant = assistant_messages[-1].strip() if assistant_messages else ""
    return {
        "tool_calls": calls,
        "requests": requests,
        "request_count": (
            total_counts[-1] if total_counts
            else len(requests) if requests
            else sum(usage_counts)
        ),
        "transcript": transcript,
        "refusals": refusals,
        "errors": errors,
        "repeated_calls": repeated,
        "review_scores": review_scores,
        "review_final": review_scores[-1] if review_scores else None,
        "user_cap_hit": int(cap.group(1)) if cap else None,
        "final_assistant": final_assistant,
    }


def phase_health(project, artifacts, number):
    schematic = first_schematic(project)
    board = next(iter(sorted(project.glob("*.kicad_pcb"))), None)
    erc = run_check("sch", schematic, artifacts / f"phase-{number}-erc.json")
    drc = run_check("pcb", board, artifacts / f"phase-{number}-drc.json")
    health = {
        "schematic_created": schematic is not None,
        "pcb_created": board is not None,
        **severity_counts(erc, "erc"),
        **severity_counts(drc, "drc"),
    }
    if isinstance(drc, dict) and "error" not in drc:
        health["unconnected_items"] = len(drc.get("unconnected_items", []))
    return health


def write_gallery(artifacts, phases):
    cards = []
    for phase in phases:
        caption = (
            f"{phase['label']} · {phase['tool_call_count']} tool calls · "
            f"ERC {phase['health'].get('erc_errors', '?')} errors · "
            f"DRC {phase['health'].get('drc_errors', '?')} errors"
        )
        images = []
        for kind in ("schematic", "pcb"):
            rendered = phase.get("renders", {}).get(kind, {})
            if rendered.get("path"):
                relative = Path(rendered["path"]).relative_to(artifacts)
                figure = (
                    f'<figure><img src="{html.escape(str(relative))}" '
                    f'alt="{html.escape(phase["label"])} {kind}"></figure>'
                )
            else:
                figure = f'<figure class="missing"><div>No {kind} render</div></figure>'
            images.append(
                f'<div class="kind"><h3>{html.escape(kind.title())}</h3>{figure}</div>'
            )
        cards.append(
            f'<section><h2>{html.escape(caption)}</h2>'
            + "".join(images)
            + "</section>"
        )
    document = """<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Gordian phase gallery</title>
<style>
body{font:15px system-ui,sans-serif;margin:24px;background:#17191d;color:#eee}
section{margin:0 0 32px}h1,h2,h3{font-weight:600}h2{font-size:16px;color:#bbb}
.kind{margin-top:20px}.kind h3{font-size:15px;text-transform:capitalize}
figure{margin:0;background:#fff;padding:8px;color:#222}
img{display:block;width:100%;height:auto}.missing div{display:grid;min-height:220px;place-items:center;color:#777}
</style></head><body><h1>Design phases</h1>""" + "".join(cards) + "</body></html>\n"
    (artifacts / "gallery.html").write_text(document, encoding="utf-8")


def run_agent(project, prompt, timeout, env):
    """Run one agent case to completion. `timeout` is None unless the operator
    set `QUALITY_TIMEOUT`: the loop has no budget of its own and the harness
    imposes none either."""
    args = agent_command(project, prompt)
    try:
        return command(args, timeout=timeout, check=False, env=env)
    except subprocess.TimeoutExpired as error:
        stdout = error.stdout or ""
        stderr = error.stderr or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode(errors="replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode(errors="replace")
        stderr += f"\nquality harness: agent timed out after {timeout}s\n"
        return subprocess.CompletedProcess(args, 124, stdout, stderr)


def run_case(case, output_root, attempt=None):
    started = time.time()
    prompt = (case / "prompt.txt").read_text(encoding="utf-8").strip()
    rubric, checks = parse_rubric((case / "rubric.txt").read_text(encoding="utf-8"))
    run_dir = output_root / (case.name if attempt is None else f"{case.name}#{attempt}")
    if run_dir.exists():
        shutil.rmtree(run_dir)
    artifacts = run_dir / "artifacts"
    project = run_dir / "project"
    before_project = run_dir / "before-project"
    artifacts.mkdir(parents=True)
    project.mkdir()
    result_path = run_dir / "result.json"
    started_utc = datetime.fromtimestamp(started, timezone.utc).isoformat()
    result_path.write_text(
        json.dumps(
            {"case": case.name, "started_utc": started_utc, "_started_epoch": started},
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    prepare_project(case, project)
    shutil.copytree(project, before_project)
    renders = {"before": render_project(project, artifacts, "before")}

    agent_started = time.time()
    result = run_agent(
        project,
        prompt,
        int(os.environ["QUALITY_TIMEOUT"]) if os.environ.get("QUALITY_TIMEOUT") else None,
        {**os.environ, "GORDIAN_THREAD_ID": f"quality-{case.name}-{int(started)}"},
    )
    agent_seconds = round(time.time() - agent_started, 1)
    (artifacts / "agent.stdout.txt").write_text(result.stdout, encoding="utf-8")
    (artifacts / "agent.stderr.txt").write_text(result.stderr, encoding="utf-8")
    parsed = parse_agent_stderr(result.stderr)
    phases = [{
        "label": "After the run",
        "tool_call_count": len(parsed["tool_calls"]),
        "health": phase_health(project, artifacts, 1),
        "renders": render_project(project, artifacts, "phase-1"),
    }]
    turn_facts = {
        "agent_exit": result.returncode,
        "request_count": parsed["request_count"],
        "requests": parsed["requests"],
        "tool_call_details": parsed["tool_calls"],
        "refusals": parsed["refusals"],
        "errors": parsed["errors"],
        "repeated_calls": parsed["repeated_calls"],
        "review_scores": parsed["review_scores"],
        "review_final": parsed["review_final"],
        "user_cap_hit": parsed["user_cap_hit"],
        "final_assistant": parsed["final_assistant"],
        "transcript": parsed["transcript"],
    }
    # failure costs the verdicts and not the whole run.
    result_path.write_text(
        json.dumps(
            {
                "case": case.name,
                "started_utc": started_utc,
                "_started_epoch": started,
                "agent_seconds": agent_seconds,
                "elapsed_seconds": round(time.time() - started, 1),
                **turn_facts,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    facts, detail = deterministic_facts(case, project, before_project, artifacts, result)
    renders["after"] = phases[-1]["renders"]
    facts.update(
        schematic_visual_facts(project, before_project)
        if first_schematic(project)
        else {}
    )
    facts.update(turn_facts)
    facts["elapsed_seconds"] = round(time.time() - started, 1)
    report = {
        "case": case.name,
        "started_utc": started_utc,
        "agent_seconds": agent_seconds,
        "elapsed_seconds": round(time.time() - started, 1),
        **facts,
        **detail,
    }
    # The measured half is written before the judge is asked, so a gateway
    # failure costs the verdict and not the whole run.
    result_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    try:
        llm = llm_config()
    except Exception as error:
        llm = None

    anchor, same_circuit = schematic_anchor(case)
    for kind in ("schematic", "pcb"):
        key = f"critic_{kind}"
        rendered = renders.get("after", {}).get(kind, {})
        arguments = (kind, rendered, prompt, facts, llm)
        keywords = {"anchor": anchor, "same_circuit": same_circuit} if kind == "schematic" else {}
        if not rendered.get("path"):
            report[key] = critic(*arguments, **keywords)
        elif llm is None:
            report[key] = {
                "score": None,
                "issues": [],
                "error": "judge credentials unavailable",
            }
        else:
            try:
                report[key] = critic(*arguments, **keywords)
            except Exception as error:
                report[key] = {"score": None, "issues": [], "error": str(error)}

    human_look = {}
    for kind, count_name in (
        ("schematic", "symbol_count"),
        ("pcb", "board_part_count"),
    ):
        rendered = renders.get("after", {}).get(kind, {})
        if not rendered.get("path"):
            human_look[kind] = human_look_judge(
                kind, rendered, int(facts.get(count_name, 0)), llm
            )
        elif llm is None:
            human_look[kind] = {
                "score": None,
                "worst_three": [],
                "what_a_human_would_change": [],
                "error": "judge credentials unavailable",
            }
        else:
            try:
                human_look[kind] = human_look_judge(
                    kind, rendered, int(facts.get(count_name, 0)), llm
                )
            except Exception as error:
                human_look[kind] = {
                    "score": None,
                    "worst_three": [],
                    "what_a_human_would_change": [],
                    "error": str(error),
                }
    facts["human_look"] = human_look
    facts["critic_anchor"] = str(anchor)
    facts["schematic_critic_score"] = report["critic_schematic"].get("score")
    if same_circuit:
        facts["critic_vs_reference"] = facts["schematic_critic_score"]
    facts["pcb_critic_score"] = report["critic_pcb"].get("score")
    facts["human_look_schematic_score"] = human_look["schematic"].get("score")
    facts["human_look_pcb_score"] = human_look["pcb"].get("score")
    for kind in ("schematic", "pcb"):
        reference = human_look[kind].get("reference", {})
        facts[f"human_look_{kind}_reference"] = reference.get("source")
    report.update(facts)
    outcome = evaluate_checks(checks, facts)
    report["checks"] = outcome

    if llm is None:
        report["judge"] = {"score": None, "issues": [], "error": "judge credentials unavailable"}
    else:
        try:
            report["judge"] = judge(prompt, rubric, facts, outcome, renders, llm)
        except Exception as error:
            report["judge"] = {"score": None, "issues": [], "error": str(error)}

    if llm is None:
        report["self_diagnosis"] = {"error": "judge credentials unavailable"}
    else:
        try:
            report["self_diagnosis"] = self_diagnose(
                prompt,
                (artifacts / "agent.stderr.txt").read_text(
                    encoding="utf-8", errors="replace"
                ),
                llm,
            )
        except Exception as error:
            report["self_diagnosis"] = {"error": str(error)}

    report["judge_score"] = report["judge"]["score"]
    if report["judge_score"] is not None:
        report["score"] = (
            min(report["judge_score"], CAPPED_SCORE)
            if outcome["fail"]
            else report["judge_score"]
        )
    report["elapsed_seconds"] = round(time.time() - started, 1)
    result_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    write_gallery(artifacts, phases)
    write_case_findings(run_dir, report)
    return report


# --- findings ---------------------------------------------------------------


def critic_issue_text(issue):
    if not isinstance(issue, dict):
        return str(issue)
    description = issue.get("description") or json.dumps(issue, sort_keys=True)
    context = "/".join(
        str(issue[name])
        for name in ("severity", "category", "location")
        if issue.get(name)
    )
    return f"{context}: {description}" if context else description


def findings_for(report):
    findings = []
    if report.get("error"):
        findings.append(("harness", f"runner error: {report['error']}"))
    board_expected = any(
        key.startswith(("board_parts", "board_outline", "drc_errors", "drc_warnings"))
        for key in report
    )
    for name in (
        "sch_facts_error",
        "kicad_netlist_error",
        "pcb_facts_error",
        "board_check_error",
        "board_metrics_error",
        "schematic_visual_error",
        "erc_check_error",
        "drc_check_error",
    ):
        if name in ("drc_check_error", "board_check_error", "board_metrics_error", "pcb_facts_error") and not board_expected:
            continue
        if report.get(name):
            findings.append(("harness", f"{name}: {report[name]}"))
    checks = report.get("checks", {})
    failed = checks.get("fail", [])
    for failure in failed:
        tag = "harness" if any(
            marker in failure for marker in ("unparseable check", "not measured:")
        ) else "engine"
        findings.append((tag, f"failed check: {failure}"))

    all_checks_pass = not failed
    judge = report.get("judge", {})
    for issue in judge.get("issues", []):
        findings.append(
            ("judge" if all_checks_pass else "engine", f"judge: {issue}")
        )
    if judge.get("error"):
        findings.append(("harness", f"judge unavailable: {judge['error']}"))

    diagnosis = report.get("self_diagnosis", {})
    for struggle in diagnosis.get("struggles", []):
        findings.append(("self-diagnosis", f"struggled: {struggle}"))
    for wish in diagnosis.get("wishes", []):
        findings.append(("self-diagnosis", f"wished: {wish}"))
    if diagnosis.get("error"):
        findings.append(("harness", f"self-diagnosis unavailable: {diagnosis['error']}"))

    for kind in ("schematic", "pcb"):
        result = report.get(f"critic_{kind}", {})
        for issue in result.get("issues", []):
            findings.append(
                (
                    "judge" if all_checks_pass else "engine",
                    f"{kind} critic: {critic_issue_text(issue)}",
                )
            )
        if result.get("error"):
            findings.append(("harness", f"{kind} critic unavailable: {result['error']}"))

        human = report.get("human_look", {}).get(kind, {})
        for issue in human.get("worst_three", []):
            findings.append(("judge", f"{kind} human-look: {issue}"))
        if human.get("error") and not human.get("error", "").startswith(f"no {kind} render"):
            findings.append(("harness", f"{kind} human-look unavailable: {human['error']}"))

    for signal_name, label in (("errors", "error"), ("refusals", "refusal")):
        for signal in report.get(signal_name, []):
            findings.append(
                (
                    "tool-contract",
                    f"tool `{signal['tool']}` {label}: {signal['message']}",
                )
            )
    for smell in report.get("repeated_calls", []):
        findings.append(
            (
                "prompt",
                f"loop smell: tool `{smell['tool']}` called {smell['count']} times in a row",
            )
        )

    cap = report.get("user_cap_hit")
    cost_tag = "prompt" if cap or report.get("repeated_calls") else "variance"
    cost = (
        f"cost: {report.get('elapsed_seconds', 0):.1f}s elapsed, "
        f"{report.get('agent_seconds', 0):.1f}s agent, "
        f"{report.get('request_count', 0)} provider requests"
    )
    if cap:
        cost += f"; stopped at the user-set cap of {cap} requests"
    findings.append((cost_tag, cost))
    if report.get("requests"):
        latencies = ", ".join(
            f"#{request['request']}={request['latency_ms']}ms"
            for request in report["requests"]
        )
        findings.append(("variance", f"provider latency: {latencies}"))
    return findings


def write_case_findings(run_dir, report):
    lines = [
        f"# Findings: {report['case']}",
        "",
        "[Phase gallery](artifacts/gallery.html)",
        "",
    ]
    lines.extend(f"- [{tag}] {text}" for tag, text in findings_for(report))
    human_look = report.get("human_look", {})
    lines.extend(["", "## Human look", ""])
    for kind in ("schematic", "pcb"):
        verdict = human_look.get(kind, {})
        lines.extend([
            f"### {kind.title()}",
            "",
            f"Score: {verdict.get('score', '-')} / 10",
            "",
        ])
        reference = report.get(f"human_look_{kind}_reference")
        if reference:
            lines.append(f"Reference (`human_look_{kind}_reference`): `{reference}`")
            lines.append("")
        for issue in verdict.get("worst_three", []):
            lines.append(f"- Worst: {issue}")
        for change in verdict.get("what_a_human_would_change", []):
            lines.append(f"- Human change: {change}")
        if verdict.get("error"):
            lines.append(f"- Unavailable: {verdict['error']}")
    transcript = report.get("transcript")
    if transcript:
        lines.extend(["", "## Agent transcript", "", "````text", *transcript, "````"])
    (run_dir / "findings.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def recover_failed_report(output_root, case, error):
    """Keep expensive partial evidence when an unexpected harness step fails."""
    run_dir = output_root / case
    run_dir.mkdir(parents=True, exist_ok=True)
    result_path = run_dir / "result.json"
    try:
        report = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        report = {"case": case}
    started = report.pop("_started_epoch", None)
    if isinstance(started, (int, float)):
        report["elapsed_seconds"] = round(time.time() - started, 1)
    report.setdefault("agent_seconds", 0.0)
    report.setdefault("elapsed_seconds", 0.0)
    report.setdefault("checks", {"pass": [], "fail": []})
    stderr_path = run_dir / "artifacts" / "agent.stderr.txt"
    if stderr_path.is_file():
        report.update(parse_agent_stderr(stderr_path.read_text(encoding="utf-8")))
    else:
        report.setdefault("request_count", 0)
    report["error"] = str(error)
    result_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    write_case_findings(run_dir, report)
    return report


def aggregate_findings(reports, output_root, suite, questions):
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    destination = ROOT / "quality" / "findings" / f"{timestamp}-{suite}.md"
    destination.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        f"# Quality findings: {suite}",
        "",
        f"Generated: {timestamp}",
        f"Run output: {output_root}",
        "",
        "Questions:",
    ]
    lines.extend(f"- {question}" for question in questions)
    if not questions:
        lines.append("- (none provided)")

    grouped = {tag: [] for tag in FINDING_TAGS}
    for report in reports:
        for tag, finding in findings_for(report):
            grouped.setdefault(tag, []).append((report["case"], finding))
    for tag in FINDING_TAGS:
        if not grouped[tag]:
            continue
        lines.extend(["", f"## [{tag}]", ""])
        for case, finding in grouped[tag]:
            lines.append(f"- `{case}`: {finding}")
    destination.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return destination


# --- scoreboard -------------------------------------------------------------


COLUMNS = [
    "case", "score", "sch critic", "pcb critic", "human look", "checks", "erc e/w",
    "moved", "lost", "added", "pcb moved", "agent s", "elapsed s",
]


def count(report, name):
    return str(len(report[name])) if name in report else "-"


def critic_score(report, kind):
    score = report.get(f"critic_{kind}", {}).get("score")
    return "-" if score is None else str(score)


def human_look_score(report):
    verdicts = report.get("human_look", {})
    values = []
    for kind in ("schematic", "pcb"):
        score = verdicts.get(kind, {}).get("score")
        values.append("-" if score is None else str(score))
    return "/".join(values)


def row(report):
    if "error" in report:
        values = [report["case"]] + ["-"] * (len(COLUMNS) - 2)
        return values + [report["error"][:60]]
    checks = report["checks"]
    total = len(checks["pass"]) + len(checks["fail"])
    return [
        report["case"],
        str(report.get("score", "-")),
        critic_score(report, "schematic"),
        critic_score(report, "pcb"),
        human_look_score(report),
        f"{len(checks['pass'])}/{total}" if total else "-",
        f"{report.get('erc_errors', '?')}/{report.get('erc_warnings', '?')}",
        count(report, "unchanged_symbols_moved"),
        count(report, "fields_lost"),
        count(report, "symbols_added"),
        count(report, "board_parts_moved"),
        f"{report.get('agent_seconds', 0):.0f}s",
        f"{report['elapsed_seconds']:.0f}s",
    ]


SCHEMATIC_COLUMNS = [
    "case", "parts", "netlist", "erc e/w", "critic", "review", "human look", "agent s",
]


def review_trajectory(report):
    """The agent's own `review_schematic` scores, in the order it took them."""
    scores = report.get("review_scores") or []
    return "-" if not scores else "→".join(f"{score:g}" for score in scores)


def schematic_row(report):
    """The benchmark view: did the circuit come out right, and does it read well."""
    if "error" in report:
        return [report["case"]] + ["-"] * (len(SCHEMATIC_COLUMNS) - 2) + [report["error"][:60]]
    match = report.get("netlist_matches_reference")
    critic = report.get("critic_schematic", {})
    score = critic.get("score")
    samples = critic.get("samples")
    attempts = report.get("attempts")
    spread = (
        ""
        if not attempts or len({a["critic"] for a in attempts}) <= 1
        else " of " + ",".join("-" if a["critic"] is None else f"{a['critic']:g}" for a in attempts)
    )
    return [
        report["case"],
        str(report.get("part_count", "-")),
        "-" if match is None else ("yes" if match else "NO"),
        f"{report.get('erc_errors', '?')}/{report.get('erc_warnings', '?')}",
        "-" if score is None else (
            f"{score:g}"
            + (f" {samples}" if samples and len(set(samples)) > 1 else "")
            + spread
        ),
        review_trajectory(report),
        human_look_score(report).split("/")[0],
        f"{report.get('agent_seconds', 0):.0f}s",
    ]


def scoreboard(reports, columns=COLUMNS, row_of=row):
    rows = [columns] + [row_of(report) for report in reports]
    widths = [max(len(r[i]) for r in rows) for i in range(len(columns))]
    lines = ["| " + " | ".join(c.ljust(w) for c, w in zip(rows[0], widths)) + " |"]
    lines.append("| " + " | ".join("-" * w for w in widths) + " |")
    for r in rows[1:]:
        lines.append("| " + " | ".join(c.ljust(w) for c, w in zip(r, widths)) + " |")
    return "\n".join(lines)


def suite_view(suite):
    """Columns and row builder for a suite's scoreboard."""
    if suite == "schematic":
        return SCHEMATIC_COLUMNS, schematic_row
    return COLUMNS, row


SUITES = {
    "campaign": lambda name: name.startswith("campaign-"),
    # The schematic benchmark: human sheets redrawn from their netlist, and
    # generic circuit prompts.
    "schematic": lambda name: name.startswith(("dataset-", "prompt-")),
    "live-edit": lambda name: name.startswith("sch-"),
    "pcb": lambda name: not name.startswith(("campaign-", "dataset-", "prompt-", "sch-")),
    "all": lambda name: True,
}


def futures_report(futures, name):
    """The finished report for one case, in the order the suite lists it."""
    return next(future.result() for future, queued in futures.items() if queued == name)


def typical(reports):
    """The middle report of a case's repeated runs, by how many checks it passed
    and then by critic score.

    An agent run is not deterministic: the same H-bridge scored 4, 6 and 7 on three
    identical tries. Reporting the best would flatter the engine and reporting the
    last would be arbitrary, so the median run is the one that stands for the case.
    With an even number of runs the LOWER middle is taken, for the same reason.
    Every run's scores ride along in `attempts`.
    """
    def rank(report):
        checks = report.get("checks") or {}
        critic = (report.get("critic_schematic") or {}).get("score")
        return (-len(checks.get("fail") or []), critic if critic is not None else -1)

    # An attempt that never produced a measurement — a provider that dropped the
    # connection, a crash — is not a WORSE run, it is no run. Ranked as the worst it
    # becomes the median of two and hides a real result: one keyboard-interface attempt
    # passed every check at critic 9 while the other died mid-response, and the case was
    # reported as unmeasured.
    measured = [r for r in reports if (r.get("critic_schematic") or {}).get("score") is not None]
    judged = measured or reports
    # The LOWER middle: with an even number of runs there is no middle one, and taking
    # the upper of the two reports the better attempt, which flatters the engine exactly
    # where the spread is widest.
    ordered = sorted(judged, key=rank)
    middle = ordered[(len(ordered) - 1) // 2]
    middle["attempts_unmeasured"] = len(reports) - len(measured)
    middle["attempts"] = [
        {
            "critic": (r.get("critic_schematic") or {}).get("score"),
            "failed": (r.get("checks") or {}).get("fail") or [],
        }
        for r in reports
    ]
    return middle


def select(available, args):
    if args.cases:
        return args.cases
    return sorted(name for name in available if SUITES[args.suite](name))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cases", nargs="*", help="case names; defaults to the suite")
    parser.add_argument("--output", type=Path, default=Path("quality/runs"))
    parser.add_argument(
        "--suite",
        choices=sorted(SUITES),
        default="all",
        help="schematic runs dataset-*/prompt-*, campaign campaign-*, "
             "live-edit sch-*, pcb the rest",
    )
    parser.add_argument(
        "--jobs", type=int, default=1, help="cases to run at a time (default: 1)"
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        help="runs per case; the reported verdict is the median one (default: 1). "
             "One agent run of one case scored 4, 6 and 7 on three identical tries, "
             "so a single run cannot tell an engine change from luck.",
    )
    parser.add_argument("--scoreboard", type=Path, help="write the scoreboard here too")
    parser.add_argument(
        "--question",
        action="append",
        default=[],
        help="named question this run is intended to answer; repeatable",
    )
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")

    available = {path.name: path for path in CASES.iterdir() if path.is_dir()}
    if args.list:
        print("\n".join(select(available, args)))
        return
    selected = select(available, args)
    unknown = [name for name in selected if name not in available]
    if unknown:
        parser.error(f"unknown cases: {', '.join(unknown)}")
    output = args.output if args.output.is_absolute() else (ROOT / args.output).resolve()
    if not args.question:
        print(
            "warning: no --question supplied; name the reason for the QC run",
            file=sys.stderr,
        )

    columns, row_of = suite_view(args.suite)

    def attempt(name, index):
        try:
            return run_case(available[name], output, index)
        except Exception as error:
            print(f"{name}: {error}", file=sys.stderr)
            return recover_failed_report(output, name, error)

    def one(name):
        if args.repeat <= 1:
            return attempt(name, None)
        return typical([attempt(name, i) for i in range(args.repeat)])

    def announce(report):
        print(" | ".join(row_of(report)), flush=True)
        for failure in report.get("checks", {}).get("fail", []):
            print(f"    failed check: {failure}", flush=True)

    if args.jobs == 1:
        reports = []
        for name in selected:
            report = one(name)
            reports.append(report)
            announce(report)
    else:
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {pool.submit(one, name): name for name in selected}
            for future in as_completed(futures):
                announce(future.result())
            reports = [futures_report(futures, name) for name in selected]

    table = scoreboard(reports, columns, row_of)
    print("\n" + table)
    if args.scoreboard:
        args.scoreboard.write_text(table + "\n", encoding="utf-8")
    suite = args.suite if not args.cases else "-".join(args.cases)
    suite = re.sub(r"[^A-Za-z0-9_.-]+", "-", suite).strip("-") or "custom"
    aggregate = aggregate_findings(reports, output, suite, args.question)
    print(f"\nfindings: {aggregate}")
    broken = any(
        "error" in report
        or report.get("checks", {}).get("fail")
        or report.get("judge", {}).get("error")
        or report.get("critic_schematic", {}).get("error")
        or report.get("critic_pcb", {}).get("error")
        or any(
            verdict.get("error")
            and not verdict["error"].startswith(f"no {kind} render")
            for kind, verdict in report.get("human_look", {}).items()
        )
        for report in reports
    )
    raise SystemExit(1 if broken else 0)


if __name__ == "__main__":
    main()
