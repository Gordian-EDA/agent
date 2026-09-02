#!/usr/bin/env python3
"""Run Gordian quality cases: deterministic facts first, one VLM judge second.

A case is `cases/<name>/{prompt.txt,rubric.txt,input/}`. The prompt goes to the
real agent; the artifacts it leaves behind are measured, not guessed at. Every
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
from datetime import datetime, timezone
import json
import math
import mimetypes
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
CASES = Path(__file__).resolve().parent / "cases"
CAPPED_SCORE = 3
FINDING_TAGS = (
    "tool-contract",
    "prompt",
    "engine",
    "harness",
    "judge",
    "self-diagnosis",
    "variance",
)


def command(args, *, timeout=600, check=True, env=None):
    result = subprocess.run(
        args, cwd=ROOT, text=True, capture_output=True, timeout=timeout, env=env
    )
    if check and result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(map(str, args))}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return result


BUILT = {}


def example(package, name):
    """Path to a release example, built once per run.

    Every invocation used to go through `cargo run`, which takes the workspace
    target-dir lock — so a concurrent `cargo test` elsewhere on the machine
    could stall a case for as long as that build ran, and land in the timings.
    """
    if name not in BUILT:
        command(
            ["cargo", "build", "--release", "-p", package, "--example", name],
            timeout=1800,
        )
        BUILT[name] = str(ROOT / "target" / "release" / "examples" / name)
    return BUILT[name]


def tool(project, name, payload=None, *, allow_failed_verdict=False):
    args = [example("gordian-core", "tool_once"), str(project), name]
    if payload is not None:
        args.append(json.dumps(payload, separators=(",", ":")))
    value = json.loads(command(args).stdout)
    if value.get("error") or (
        value.get("ok") is False and not allow_failed_verdict
    ):
        raise RuntimeError(f"{name} failed: {json.dumps(value, indent=2)}")
    return value


def prepare_project(case, project):
    source = case / "input"
    if source.is_dir():
        for item in source.iterdir():
            if item.name in {"seed.place-parts.json", "seed-board"}:
                continue
            target = project / item.name
            if item.is_dir():
                shutil.copytree(item, target)
            else:
                shutil.copy2(item, target)

    seed = source / "seed.place-parts.json"
    if not seed.exists():
        return
    tool(project, "place_parts", json.loads(seed.read_text(encoding="utf-8")))
    if (source / "seed-board").exists():
        tool(project, "sync_board")
        tool(project, "place_board")
        tool(project, "route_board")
        tool(project, "check_board")


# --- deterministic board facts ---------------------------------------------


def pcb_facts(project):
    """Where every footprint sits, read from the `.kicad_pcb` itself."""
    configured = os.environ.get("PCB_FACTS_BIN")
    binary = configured or example("kicad-board", "pcb_facts")
    result = command([binary, str(project)], check=False)
    if result.returncode:
        return {"error": (result.stderr or result.stdout).strip()}
    return json.loads(result.stdout)


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


def total_track_length(tracks):
    length = 0.0
    for track in tracks:
        if not isinstance(track, dict) or not isinstance(track.get("path"), list):
            raise ValueError("get_board returned a track without a path")
        path = track["path"]
        for start, end in zip(path, path[1:]):
            if not (
                isinstance(start, list)
                and isinstance(end, list)
                and len(start) >= 2
                and len(end) >= 2
                and all(isinstance(value, (int, float)) for value in (*start[:2], *end[:2]))
            ):
                raise ValueError("get_board returned a malformed track point")
            length += math.hypot(end[0] - start[0], end[1] - start[1])
    return round(length, 3)


def board_quality_facts(project):
    """DRC diagnostics and cheap copper metrics from the public board tools."""
    facts = {}
    try:
        with tempfile.TemporaryDirectory(prefix="gordian-quality-board-") as temporary:
            check_project = Path(temporary) / "project"
            shutil.copytree(project, check_project)
            checked = tool(
                check_project, "check_board", allow_failed_verdict=True
            )
            unconnected = checked.get("unconnected")
            unconnected_count = checked.get("unconnected_items")
            if not isinstance(unconnected, list) or not isinstance(
                unconnected_count, int
            ):
                raise ValueError("check_board omitted unconnected diagnostics")
    except Exception as error:
        facts["board_check_error"] = str(error)
    else:
        facts.update(
            {
                "board_check_error": None,
                "drc_blocking_findings": checked.get("blocking_findings"),
                "drc_reported_findings": checked.get("reported_findings"),
                "drc_copper_violations": checked.get("copper_violations"),
            }
        )
        if unconnected_count and not unconnected:
            unconnected = [
                f"{unconnected_count} unconnected item(s); pad pairs unavailable"
            ]
        facts["unrouted"] = unconnected

    try:
        board = tool(project, "get_board", {"include_copper": True})
        copper = board["board"]["copper"]
        if (
            not isinstance(copper.get("tracks"), list)
            or not isinstance(copper.get("via_count"), int)
        ):
            raise ValueError("get_board omitted copper metrics")
    except Exception as error:
        facts["board_metrics_error"] = str(error)
    else:
        facts.update(
            {
                "board_metrics_error": None,
                "via_count": copper["via_count"],
                "total_track_length": total_track_length(copper["tracks"]),
            }
        )
    return facts


# --- deterministic schematic facts -----------------------------------------


def sch_facts_binary():
    """The `sch-doc` facts example, built on first use."""
    configured = os.environ.get("SCH_FACTS_BIN")
    if configured:
        return [configured]
    return [example("sch-doc", "sch_facts")]


def sch_facts(*args):
    result = command(sch_facts_binary() + [str(a) for a in args], check=False)
    if result.returncode:
        return {"error": (result.stderr or result.stdout).strip()}
    return json.loads(result.stdout)


def kicad_partition(schematic, out_path):
    """KiCAD's own net partition: sorted `REF.PIN` groups, power symbols and
    single-pin nets dropped so it is comparable to the extractor's, plus the
    named nets exactly as KiCAD sees them. The named form is what settles a
    connectivity question for the judge, which otherwise has only a render —
    where a wire passing behind a symbol reads as a short that is not there."""
    result = command(
        [
            "kicad-cli", "sch", "export", "netlist",
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
            "kicad-cli", kind, "erc" if kind == "sch" else "drc",
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


# Board tools whose call the flow is measured in. `get_board` is a pure query
# and does not count against a flow's budget.
BOARD_TOOLS = {
    "sync_board", "place_board", "route_board", "check_board", "export_fab",
    "move_parts", "route_track", "delete_copper", "set_net_width",
    "update_board_outline", "render_board", "open_board",
}

# The tools that own board geometry. A refusal here is the board contract
# telling the model no; a refusal from `sync_board` is usually a mis-shaped
# payload, and one from a review tool is an opinion.
GEOMETRY_TOOLS = {
    "place_board", "route_board", "move_parts", "route_track", "delete_copper",
    "update_board_outline",
}


def transcript_facts(artifacts):
    """What the agent actually did, read from its own event stream.

    A board that ends DRC-clean can still have been reached through a dozen
    refused calls and three outline guesses. A rubric can only say so if the
    calls themselves are measured, so every `tool ->` and every refused
    `tool <-` becomes a fact."""
    path = artifacts / "agent.stderr.txt"
    text = path.read_text(encoding="utf-8", errors="replace") if path.exists() else ""
    calls = re.findall(r"^\s*tool -> (\S+)", text, re.M)
    refused = re.findall(r"^\s*tool <- (\S+): error", text, re.M)
    return {
        "tool_calls": calls,
        "board_tool_calls": [name for name in calls if name in BOARD_TOOLS],
        "refused_tools": refused,
        "refused_board_tools": [name for name in refused if name in BOARD_TOOLS],
        "refused_geometry_tools": [name for name in refused if name in GEOMETRY_TOOLS],
    }


def deterministic_facts(project, before_project, artifacts, agent_result):
    schematic = first_schematic(project)
    board = next(iter(sorted(project.glob("*.kicad_pcb"))), None)
    board_quality = board_quality_facts(project) if board is not None else {}
    erc = run_check("sch", schematic, artifacts / "erc.json")
    drc = run_check("pcb", board, artifacts / "drc.json")
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
    return facts, detail


VISUAL_FACTS = ("body_overlaps", "text_collisions", "wires_through_bodies")


def schematic_visual_facts(renders):
    """Measured visual defects after the run, plus the ones the run ADDED.

    An edit case works on a sheet that may already carry collisions (hand-drawn
    inputs do); `<fact>_added` is what the agent is answerable for there, while
    the absolute fact is what a created sheet is judged on."""
    rendered = renders.get("after", {}).get("schematic", {})
    visual = rendered.get("visual")
    if not isinstance(visual, dict):
        return {"schematic_visual_error": rendered.get("error", "not measured")}
    before = renders.get("before", {}).get("schematic", {}).get("visual")
    before = before if isinstance(before, dict) else {}
    measured = {}
    for name in VISUAL_FACTS:
        if not isinstance(visual.get(name), list):
            continue
        measured[name] = visual[name]
        seen = {json.dumps(item, sort_keys=True) for item in before.get(name, []) or []}
        measured[f"{name}_added"] = [
            item for item in visual[name] if json.dumps(item, sort_keys=True) not in seen
        ]
    missing = [name for name in VISUAL_FACTS if name not in measured]
    return {
        "schematic_visual_error": (
            f"render_schematic omitted: {', '.join(missing)}" if missing else None
        ),
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
    try:
        expected = json.loads(match.group("value"))
    except json.JSONDecodeError:
        return False, f"expected value is not JSON: {match.group('value')}"
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
    if not verdicts:
        raise ValueError(f"model returned no object with {key!r}: {text!r}")
    return verdicts[-1]


def llm_config():
    config_root = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    config_path = config_root / "gordian" / "config.toml"
    try:
        llm = tomllib.loads(config_path.read_text(encoding="utf-8"))["llm"]
    except (OSError, KeyError, tomllib.TOMLDecodeError) as error:
        raise RuntimeError(
            f"cannot read judge configuration from {config_path}: {error}"
        ) from error
    base = str(llm.get("endpoint", "")).rstrip("/")
    key = str(llm.get("apiKey", ""))
    model = str(llm.get("model", ""))
    if not base or not key or not model:
        raise RuntimeError(f"set llm.endpoint, llm.apiKey, and llm.model in {config_path}")
    return base, key, model


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


def critic(kind, rendered, prompt, facts, llm):
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
        timeout=300,
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
    if (
        isinstance(score, bool)
        or not isinstance(score, (int, float))
        or not math.isfinite(score)
        or not 0 <= score <= 10
        or not isinstance(issues, list)
        or not all(isinstance(issue, dict) for issue in issues)
    ):
        raise ValueError(f"invalid {kind} critic verdict: {verdict!r}")
    return {"score": score, "issues": issues}


# --- render + run -----------------------------------------------------------


def capture_render(project, tool_name, destination):
    try:
        value = tool(project, tool_name)
    except Exception as error:
        return {"error": str(error)}
    path = value.get("_image_path") or value.get("png_path")
    if not path or not Path(path).is_file():
        return {"error": f"{tool_name} returned no image"}
    shutil.copy2(path, destination)
    rendered = {"path": str(destination)}
    if isinstance(value.get("visual"), dict):
        rendered["visual"] = value["visual"]
    return rendered


def render_project(project, artifacts, prefix):
    rendered = {}
    if next(project.glob("*.kicad_sch"), None):
        rendered["schematic"] = capture_render(
            project, "render_schematic", artifacts / f"{prefix}-schematic.png"
        )
    if next(project.glob("*.kicad_pcb"), None):
        rendered["pcb"] = capture_render(
            project, "render_board", artifacts / f"{prefix}-pcb.png"
        )
    return rendered


def agent_command(project, prompt):
    configured = os.environ.get("GORDIAN_BIN")
    if configured:
        return [configured, "agent", "--project", str(project), "--no-review", prompt]
    return [
        "cargo", "run", "--release", "--quiet", "-p", "gordian", "--",
        "agent", "--project", str(project), "--no-review", prompt,
    ]


ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
TOOL_STARTED = re.compile(r"^tool -> (?P<tool>[A-Za-z0-9_]+)\s*$")
TOOL_FINISHED = re.compile(
    r"^tool <- (?P<tool>[A-Za-z0-9_]+):\s?(?P<summary>.*)$"
)
USAGE_REQUESTS = re.compile(r"^usage: provider_requests=(\d+)\b")
TOTAL_REQUESTS = re.compile(r"\bprovider requests:\s*(\d+)\b")
REQUEST_CAP = re.compile(r"ProviderRequestLimit\s*\{\s*requests:\s*(\d+)\s*\}")
REFUSAL = re.compile(r"\brefus(?:e|ed|al|ing)\b", re.IGNORECASE)


def parse_agent_stderr(stderr):
    """Turn the headless agent's compact event transcript into durable facts."""
    text = ANSI.sub("", stderr)
    calls = []
    attempted = []
    usage_counts = []
    total_counts = []
    for line in text.splitlines():
        stripped = line.strip()
        if match := TOOL_STARTED.match(stripped):
            name = match.group("tool")
            attempted.append(name)
            calls.append({"tool": name, "status": None, "summary": ""})
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
            continue
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
        signal = {"tool": call["tool"], "message": call["summary"]}
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

    cap = REQUEST_CAP.search(text)
    return {
        "tool_calls": calls,
        "request_count": total_counts[-1] if total_counts else sum(usage_counts),
        "refusals": refusals,
        "errors": errors,
        "repeated_calls": repeated,
        "request_cap_hit": int(cap.group(1)) if cap else None,
    }


def run_agent(project, prompt, timeout, env):
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


def run_case(case, output_root):
    started = time.time()
    prompt = (case / "prompt.txt").read_text(encoding="utf-8").strip()
    rubric, checks = parse_rubric((case / "rubric.txt").read_text(encoding="utf-8"))
    run_dir = output_root / case.name
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
        int(os.environ.get("QUALITY_TIMEOUT", "900")),
        {**os.environ, "GORDIAN_THREAD_ID": f"quality-{case.name}-{int(started)}"},
    )
    agent_seconds = round(time.time() - agent_started, 1)
    (artifacts / "agent.stdout.txt").write_text(result.stdout, encoding="utf-8")
    (artifacts / "agent.stderr.txt").write_text(result.stderr, encoding="utf-8")
    transcript_facts = parse_agent_stderr(result.stderr)
    result_path.write_text(
        json.dumps(
            {
                "case": case.name,
                "started_utc": started_utc,
                "_started_epoch": started,
                "agent_exit": result.returncode,
                "agent_seconds": agent_seconds,
                "elapsed_seconds": round(time.time() - started, 1),
                **transcript_facts,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    facts, detail = deterministic_facts(project, before_project, artifacts, result)
    renders["after"] = render_project(project, artifacts, "after")
    facts.update(schematic_visual_facts(renders) if first_schematic(project) else {})
    facts.update(transcript_facts)
    facts["elapsed_seconds"] = round(time.time() - started, 1)
    outcome = evaluate_checks(checks, facts)
    report = {
        "case": case.name,
        "started_utc": started_utc,
        "checks": outcome,
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
        report["judge"] = {"score": None, "issues": [], "error": str(error)}
    else:
        try:
            report["judge"] = judge(prompt, rubric, facts, outcome, renders, llm)
        except Exception as error:
            report["judge"] = {"score": None, "issues": [], "error": str(error)}

    for kind in ("schematic", "pcb"):
        key = f"critic_{kind}"
        rendered = renders.get("after", {}).get(kind, {})
        if not rendered.get("path"):
            report[key] = critic(kind, rendered, prompt, facts, llm)
        elif llm is None:
            report[key] = {
                "score": None,
                "issues": [],
                "error": "judge credentials unavailable",
            }
        else:
            try:
                report[key] = critic(kind, rendered, prompt, facts, llm)
            except Exception as error:
                report[key] = {"score": None, "issues": [], "error": str(error)}

    if llm is None:
        report["self_diagnosis"] = {"error": "judge credentials unavailable"}
    else:
        try:
            report["self_diagnosis"] = self_diagnose(prompt, result.stderr, llm)
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

    cap = report.get("request_cap_hit")
    cost_tag = "prompt" if cap or report.get("repeated_calls") else "variance"
    cost = (
        f"cost: {report.get('elapsed_seconds', 0):.1f}s elapsed, "
        f"{report.get('agent_seconds', 0):.1f}s agent, "
        f"{report.get('request_count', 0)} provider requests"
    )
    if cap:
        cost += f"; request cap hit at {cap}"
    findings.append((cost_tag, cost))
    return findings


def write_case_findings(run_dir, report):
    lines = [f"# Findings: {report['case']}", ""]
    lines.extend(f"- [{tag}] {text}" for tag, text in findings_for(report))
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
            grouped[tag].append((report["case"], finding))
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
    "case", "score", "sch critic", "pcb critic", "checks", "erc e/w", "moved",
    "lost", "added", "pcb moved", "elapsed",
]


def count(report, name):
    return str(len(report[name])) if name in report else "-"


def critic_score(report, kind):
    score = report.get(f"critic_{kind}", {}).get("score")
    return "-" if score is None else str(score)


def row(report):
    if "error" in report:
        return [
            report["case"], "-", "-", "-", "-", "-", "-", "-", "-", "-",
            report["error"][:60],
        ]
    checks = report["checks"]
    total = len(checks["pass"]) + len(checks["fail"])
    return [
        report["case"],
        str(report.get("score", "-")),
        critic_score(report, "schematic"),
        critic_score(report, "pcb"),
        f"{len(checks['pass'])}/{total}" if total else "-",
        f"{report.get('erc_errors', '?')}/{report.get('erc_warnings', '?')}",
        count(report, "unchanged_symbols_moved"),
        count(report, "fields_lost"),
        count(report, "symbols_added"),
        count(report, "board_parts_moved"),
        f"{report['elapsed_seconds']:.0f}s",
    ]


def scoreboard(reports):
    rows = [COLUMNS] + [row(report) for report in reports]
    widths = [max(len(r[i]) for r in rows) for i in range(len(COLUMNS))]
    lines = ["| " + " | ".join(c.ljust(w) for c, w in zip(rows[0], widths)) + " |"]
    lines.append("| " + " | ".join("-" * w for w in widths) + " |")
    for r in rows[1:]:
        lines.append("| " + " | ".join(c.ljust(w) for c, w in zip(r, widths)) + " |")
    return "\n".join(lines)


def select(available, args):
    if args.cases:
        return args.cases
    if args.suite == "campaign":
        return sorted(n for n in available if n.startswith("campaign-"))
    if args.suite == "schematic":
        return sorted(n for n in available if n.startswith("sch-"))
    if args.suite == "pcb":
        return sorted(n for n in available if not n.startswith("sch-"))
    return sorted(available)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cases", nargs="*", help="case names; defaults to the suite")
    parser.add_argument("--output", type=Path, default=Path("quality/runs"))
    parser.add_argument(
        "--suite",
        choices=["campaign", "schematic", "pcb", "all"],
        default="all",
        help="campaign runs campaign-*, schematic runs sch-*, pcb runs the rest",
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

    reports = []
    for name in selected:
        try:
            report = run_case(available[name], output)
        except Exception as error:
            report = recover_failed_report(output, name, error)
            print(f"{name}: {error}", file=sys.stderr)
        reports.append(report)
        print(" | ".join(row(report)), flush=True)
        for failure in report.get("checks", {}).get("fail", []):
            print(f"    failed check: {failure}", flush=True)

    table = scoreboard(reports)
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
        for report in reports
    )
    raise SystemExit(1 if broken else 0)


if __name__ == "__main__":
    main()
