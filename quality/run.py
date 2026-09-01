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
import json
import mimetypes
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
CASES = Path(__file__).resolve().parent / "cases"
CAPPED_SCORE = 3


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


def tool(project, name, payload=None):
    args = [example("gordian-core", "tool_once"), str(project), name]
    if payload is not None:
        args.append(json.dumps(payload, separators=(",", ":")))
    value = json.loads(command(args).stdout)
    if value.get("error") or value.get("ok") is False:
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
        tool(project, "regenerate_board")
        tool(project, "place_board")
        tool(project, "route_board")
        tool(project, "check_board")


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


def deterministic_facts(project, before_project, artifacts, agent_result):
    schematic = first_schematic(project)
    board = next(iter(sorted(project.glob("*.kicad_pcb"))), None)
    erc = run_check("sch", schematic, artifacts / "erc.json")
    drc = run_check("pcb", board, artifacts / "drc.json")
    unconnected = drc.get("unconnected_items", []) if isinstance(drc, dict) else []
    fab = (
        sorted(path.name for path in (project / "fab").glob("*"))
        if (project / "fab").is_dir()
        else []
    )
    sch, detail = schematic_facts(project, before_project, artifacts)
    facts = {
        "agent_exit": agent_result.returncode,
        "pcb_created": board is not None,
        **severity_counts(erc, "erc"),
        **severity_counts(drc, "drc"),
        "unconnected_items": len(unconnected),
        "fab_files": fab,
        **sch,
    }
    return facts, detail


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


def extract_object(text):
    decoder = json.JSONDecoder()
    objects = []
    for match in re.finditer(r"\{", text):
        try:
            value, _ = decoder.raw_decode(text[match.start():])
            if isinstance(value, dict):
                objects.append(value)
        except json.JSONDecodeError:
            pass
    verdicts = [value for value in objects if "score" in value]
    if not verdicts:
        raise ValueError(f"judge returned no verdict object: {text!r}")
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


def judge(prompt, rubric, facts, checks, renders):
    base, key, model = llm_config()
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
    return {"path": str(destination)}


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

    prepare_project(case, project)
    shutil.copytree(project, before_project)
    renders = {"before": render_project(project, artifacts, "before")}

    agent_started = time.time()
    result = command(
        agent_command(project, prompt),
        timeout=int(os.environ.get("QUALITY_TIMEOUT", "900")),
        check=False,
        env={**os.environ, "GORDIAN_THREAD_ID": f"quality-{case.name}-{int(started)}"},
    )
    agent_seconds = round(time.time() - agent_started, 1)
    (artifacts / "agent.stdout.txt").write_text(result.stdout, encoding="utf-8")
    (artifacts / "agent.stderr.txt").write_text(result.stderr, encoding="utf-8")

    renders["after"] = render_project(project, artifacts, "after")
    facts, detail = deterministic_facts(project, before_project, artifacts, result)
    outcome = evaluate_checks(checks, facts)
    report = {
        "case": case.name,
        "checks": outcome,
        "agent_seconds": agent_seconds,
        "elapsed_seconds": round(time.time() - started, 1),
        **facts,
        **detail,
    }
    # The measured half is written before the judge is asked, so a gateway
    # failure costs the verdict and not the whole run.
    result_path = run_dir / "result.json"
    result_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    verdict = judge(prompt, rubric, facts, outcome, renders)
    report["judge"] = verdict
    report["judge_score"] = verdict["score"]
    report["score"] = (
        min(verdict["score"], CAPPED_SCORE) if outcome["fail"] else verdict["score"]
    )
    report["elapsed_seconds"] = round(time.time() - started, 1)
    result_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report


# --- scoreboard -------------------------------------------------------------


COLUMNS = ["case", "score", "checks", "erc e/w", "moved", "lost", "added", "elapsed"]


def count(report, name):
    return str(len(report[name])) if name in report else "-"


def row(report):
    if "error" in report:
        return [report["case"], "-", "-", "-", "-", "-", "-", report["error"][:60]]
    checks = report["checks"]
    total = len(checks["pass"]) + len(checks["fail"])
    return [
        report["case"],
        str(report.get("score", "-")),
        f"{len(checks['pass'])}/{total}" if total else "-",
        f"{report.get('erc_errors', '?')}/{report.get('erc_warnings', '?')}",
        count(report, "unchanged_symbols_moved"),
        count(report, "fields_lost"),
        count(report, "symbols_added"),
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
        choices=["schematic", "pcb", "all"],
        default="all",
        help="schematic runs every sch-* case, pcb runs the rest",
    )
    parser.add_argument("--scoreboard", type=Path, help="write the scoreboard here too")
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

    reports = []
    for name in selected:
        try:
            report = run_case(available[name], output)
        except Exception as error:
            report = {"case": name, "error": str(error)}
            print(f"{name}: {error}", file=sys.stderr)
        reports.append(report)
        print(" | ".join(row(report)), flush=True)
        for failure in report.get("checks", {}).get("fail", []):
            print(f"    failed check: {failure}", flush=True)

    table = scoreboard(reports)
    print("\n" + table)
    if args.scoreboard:
        args.scoreboard.write_text(table + "\n", encoding="utf-8")
    broken = any(
        "error" in report or report.get("checks", {}).get("fail") for report in reports
    )
    raise SystemExit(1 if broken else 0)


if __name__ == "__main__":
    main()
