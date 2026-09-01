#!/usr/bin/env python3
"""Run Gordian quality cases and grade the artifacts with one VLM judge."""

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
import tempfile
import tomllib
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
CASES = Path(__file__).resolve().parent / "cases"


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


def tool(project, name, payload=None):
    args = [
        "cargo", "run", "--release", "--quiet", "-p", "gordian-core",
        "--example", "tool_once", "--", str(project), name,
    ]
    if payload is not None:
        args.append(json.dumps(payload, separators=(",", ":")))
    result = command(
        args
    )
    value = json.loads(result.stdout)
    if value.get("error") or value.get("ok") is False:
        raise RuntimeError(f"{name} failed: {json.dumps(value, indent=2)}")
    return value


def prepare_project(case, project):
    source = case / "input"
    if source.is_dir():
        for item in source.iterdir():
            if item.name in {"seed.circuit.yaml", "seed-board"}:
                continue
            target = project / item.name
            if item.is_dir():
                shutil.copytree(item, target)
            else:
                shutil.copy2(item, target)

    seed = source / "seed.circuit.yaml"
    if not seed.exists():
        return
    state = project / ".gordian"
    state.mkdir()
    shutil.copy2(seed, state / "draft.circuit.yaml")
    (state / "draft.meta.json").write_text(
        '{"seeded_from_sch_hash":null}', encoding="utf-8"
    )
    tool(project, "apply_design", {"__commit": True})
    if (source / "seed-board").exists():
        tool(project, "regenerate_board")
        tool(project, "place_board")
        tool(project, "route_board")
        tool(project, "check_board")


def capture_render(project, tool_name, destination):
    try:
        value = tool(project, tool_name)
    except Exception as error:
        return {"error": str(error)}
    path = value.get("__image_path") or value.get("png_path")
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


def schematic_snapshot(schematic):
    """Placement + connectivity of a .kicad_sch, straight from `sch-doc`."""
    if schematic is None or not Path(schematic).is_file():
        return None
    result = command(
        [
            "cargo", "run", "--release", "--quiet", "-p", "sch-doc",
            "--example", "snapshot", "--", str(schematic),
        ],
        check=False,
    )
    if result.returncode:
        return None
    return json.loads(result.stdout)


def schematic_changes(before, after):
    """What the agent actually did: which parts moved, and how nets differ."""
    if not before or not after:
        return {}
    names = lambda snap: {key.split("#")[0] for key in snap["symbols"]}
    moved = sorted(
        key.split("#")[0]
        for key, value in before["symbols"].items()
        if key in after["symbols"] and after["symbols"][key][:3] != value[:3]
    )
    retyped = sorted(
        key.split("#")[0]
        for key, value in before["symbols"].items()
        if key in after["symbols"] and after["symbols"][key][3:] != value[3:]
    )
    shared = set(before["nets"]) & set(after["nets"])
    return {
        "symbols_added": sorted(names(after) - names(before)),
        "symbols_removed": sorted(names(before) - names(after)),
        "symbols_moved": moved,
        "symbols_untouched": len(before["symbols"]) - len(moved),
        "symbols_retyped": retyped,
        "nets_added": sorted(set(after["nets"]) - set(before["nets"])),
        "nets_removed": sorted(set(before["nets"]) - set(after["nets"])),
        "nets_changed": {
            name: {"before": before["nets"][name], "after": after["nets"][name]}
            for name in sorted(shared)
            if before["nets"][name] != after["nets"][name]
        },
    }


def deterministic_facts(project, artifacts, agent_result, before_sch=None):
    schematic = next(project.glob("*.kicad_sch"), None)
    board = next(project.glob("*.kicad_pcb"), None)
    erc = run_check("sch", schematic, artifacts / "erc.json")
    drc = run_check("pcb", board, artifacts / "drc.json")
    erc_findings = violations(erc)
    drc_findings = violations(drc)
    unconnected = drc.get("unconnected_items", []) if isinstance(drc, dict) else []
    fab = sorted(path.name for path in (project / "fab").glob("*")) if (project / "fab").is_dir() else []
    return {
        "schematic_changes": schematic_changes(before_sch, schematic_snapshot(schematic)),
        "agent_exit": agent_result.returncode,
        "schematic_created": schematic is not None,
        "pcb_created": board is not None,
        "erc_errors": sum(v.get("severity") == "error" for v in erc_findings),
        "erc_warnings": sum(v.get("severity") == "warning" for v in erc_findings),
        "drc_errors": sum(v.get("severity") == "error" for v in drc_findings),
        "drc_warnings": sum(v.get("severity") == "warning" for v in drc_findings),
        "unconnected_items": len(unconnected),
        "fab_files": fab,
    }


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
    if not objects:
        raise ValueError(f"judge returned no JSON object: {text}")
    return objects[-1]


def judge(prompt, rubric, facts, renders):
    config_root = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    config_path = config_root / "gordian" / "config.toml"
    try:
        llm = tomllib.loads(config_path.read_text(encoding="utf-8"))["llm"]
    except (OSError, KeyError, tomllib.TOMLDecodeError) as error:
        raise RuntimeError(f"cannot read judge configuration from {config_path}: {error}") from error
    base = str(llm.get("endpoint", "")).rstrip("/")
    key = str(llm.get("apiKey", ""))
    model = str(llm.get("model", ""))
    if not base or not key or not model:
        raise RuntimeError(
            f"set llm.endpoint, llm.apiKey, and llm.model in {config_path}"
        )

    text = f"""Review Gordian's result for the user request below.

REQUEST:
{prompt}

CASE RUBRIC:
{rubric}

AUTHORITATIVE CHECKS:
{json.dumps(facts, indent=2)}

The images are labeled by filename as before/after schematic or PCB renders.
Return only JSON with this exact shape:
{{"score": 0, "issues": []}}

Score must be an integer from 0 to 10. Issues must be short, concrete, actionable
strings. Do not repeat an issue already disproved by the authoritative checks.
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
        "max_tokens": 1200,
    }
    request = urllib.request.Request(
        f"{base}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=240) as response:
            payload = json.loads(response.read())
    except urllib.error.HTTPError as error:
        raise RuntimeError(error.read().decode(errors="replace")) from error
    verdict = extract_object(payload["choices"][0]["message"]["content"])
    score = verdict.get("score")
    issues = verdict.get("issues")
    if not isinstance(score, int) or not 0 <= score <= 10:
        raise ValueError(f"invalid judge score: {score!r}")
    if not isinstance(issues, list) or not all(isinstance(issue, str) for issue in issues):
        raise ValueError(f"invalid judge issues: {issues!r}")
    return {"score": score, "issues": issues}


def agent_command(project, prompt):
    configured = os.environ.get("GORDIAN_BIN")
    if configured:
        return [configured, "agent", "--project", str(project), "--no-review", prompt]
    return [
        "cargo", "run", "--release", "--quiet", "-p", "gordian", "--",
        "agent", "--project", str(project), "--no-review", prompt,
    ]


def run_case(case, output_root):
    prompt = (case / "prompt.txt").read_text(encoding="utf-8").strip()
    rubric = (case / "rubric.txt").read_text(encoding="utf-8").strip()
    run_dir = output_root / case.name
    if run_dir.exists():
        shutil.rmtree(run_dir)
    artifacts = run_dir / "artifacts"
    project = run_dir / "project"
    artifacts.mkdir(parents=True)
    project.mkdir()

    prepare_project(case, project)
    before_sch = schematic_snapshot(next(project.glob("*.kicad_sch"), None))
    renders = {"before": render_project(project, artifacts, "before")}
    result = command(
        agent_command(project, prompt),
        timeout=int(os.environ.get("QUALITY_TIMEOUT", "900")),
        check=False,
        env={**os.environ, "GORDIAN_THREAD_ID": f"quality-{case.name}-{int(time.time())}"},
    )
    (artifacts / "agent.stdout.txt").write_text(result.stdout, encoding="utf-8")
    (artifacts / "agent.stderr.txt").write_text(result.stderr, encoding="utf-8")
    renders["after"] = render_project(project, artifacts, "after")
    facts = deterministic_facts(project, artifacts, result, before_sch)
    verdict = judge(prompt, rubric, facts, renders)
    report = {"case": case.name, **facts, "judge": verdict}
    (run_dir / "result.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(report, indent=2))
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cases", nargs="*", help="case names; defaults to all cases")
    parser.add_argument("--output", type=Path, default=Path("quality/runs"))
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()
    available = {path.name: path for path in CASES.iterdir() if path.is_dir()}
    if args.list:
        print("\n".join(sorted(available)))
        return
    selected = args.cases or sorted(available)
    unknown = [name for name in selected if name not in available]
    if unknown:
        parser.error(f"unknown cases: {', '.join(unknown)}")
    output = (ROOT / args.output).resolve() if not args.output.is_absolute() else args.output
    failed = False
    for name in selected:
        try:
            run_case(available[name], output)
        except Exception as error:
            failed = True
            print(f"{name}: {error}", file=sys.stderr)
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
