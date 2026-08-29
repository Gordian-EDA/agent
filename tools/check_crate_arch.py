#!/usr/bin/env python3
"""Check workspace crate dependency direction using Cargo metadata."""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
API_CRATES = {"pcb-model", "pcb-place-api"}
PCB_IMPLEMENTATIONS = {"pcb-place", "pcb-route-grid", "pcb-route-mesh", "pcb-drc"}
AGENT_CRATES = {"gordian", "gordian-core", "gordian-runtime", "gordian-tools-pcb"}


def workspace_graph() -> dict[str, set[str]]:
    output = subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        text=True,
    )
    metadata = json.loads(output)
    workspace = {package["name"] for package in metadata["packages"]}
    graph: dict[str, set[str]] = {name: set() for name in workspace}
    for package in metadata["packages"]:
        for dependency in package["dependencies"]:
            if dependency["name"] not in workspace:
                continue
            kinds = dependency.get("kind")
            if kinds in (None, "normal", "build"):
                graph[package["name"]].add(dependency["name"])
    return graph


def cycles(graph: dict[str, set[str]]) -> list[list[str]]:
    found: set[tuple[str, ...]] = set()

    def visit(node: str, path: list[str], active: set[str]) -> None:
        if node in active:
            start = path.index(node)
            cycle = path[start:] + [node]
            body = cycle[:-1]
            pivot = min(range(len(body)), key=body.__getitem__)
            normalized = tuple(body[pivot:] + body[:pivot])
            found.add(normalized)
            return
        active.add(node)
        path.append(node)
        for dependency in sorted(graph[node]):
            visit(dependency, path, active)
        path.pop()
        active.remove(node)

    for crate in sorted(graph):
        visit(crate, [], set())
    return [list(cycle) for cycle in sorted(found)]


def main() -> int:
    graph = workspace_graph()
    errors: list[str] = []
    for cycle in cycles(graph):
        errors.append("normal dependency cycle: " + " -> ".join(cycle + [cycle[0]]))

    for api in sorted(API_CRATES):
        forbidden = graph.get(api, set()) & PCB_IMPLEMENTATIONS
        for dependency in sorted(forbidden):
            errors.append(f"API crate {api} depends on implementation {dependency}")

    for crate in sorted(PCB_IMPLEMENTATIONS | API_CRATES):
        forbidden = graph.get(crate, set()) & AGENT_CRATES
        for dependency in sorted(forbidden):
            errors.append(f"PCB crate {crate} depends on agent crate {dependency}")

    if errors:
        print("crate architecture check failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print("crate architecture check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
