#!/usr/bin/env python3
"""Check workspace crate dependency direction using Cargo metadata.

Two rules beyond acyclicity: the phase/API direction on the PCB side, and the LEAF rule —
every key algorithm crate links only its model crate, `geom`, and the listed pure helpers,
so an independent author can work on one without coupling to the rest.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
API_CRATES = {"pcb-model"}

# ── the LEAF rule ────────────────────────────────────────────────────────────
# Every key algorithm is a self-contained leaf: a defined input, a defined output, and
# collaborators injected as traits. A leaf's LIBRARY therefore links only its model crate,
# `geom`, and the listed pure data/classification helpers — never another algorithm, a
# realiser, or an orchestrator.
#
# A leaf's TESTS may additionally name sibling leaves of the same domain, so a contract
# test can inject the PRODUCTION collaborator and keep asserting something real (a routing
# leaf checked against a stub DRC asserts nothing). What they may never reach is an
# orchestrator, the agent, or KiCAD: `cargo test -p <leaf>` and
# `cargo run -p <leaf> --example bench` must work with no installation and no agent.
PURE_HELPERS = {"geom", "circuit-graph", "kicad-symbol", "pcb-grid"}
MODEL_CRATES = {"sch-model": PURE_HELPERS, "pcb-model": PURE_HELPERS}
SCH_LEAVES = {"anneal-place", "cluster-place", "spine-place"}
PCB_LEAVES = {"pcb-place", "pcb-route-grid", "pcb-route-mesh", "pcb-drc"}
LEAF_LIBRARY_DEPS = {
    **{leaf: {"sch-model"} | PURE_HELPERS for leaf in SCH_LEAVES},
    **{leaf: {"pcb-model"} | PURE_HELPERS for leaf in PCB_LEAVES},
}
# `cluster-place` is a PORTFOLIO engine: it runs the other two schematic leaves and keeps
# the better result, so composing them is its method, not a coupling.
LEAF_LIBRARY_DEPS["cluster-place"] |= {"anneal-place", "spine-place"}
LEAF_TEST_DEPS = {
    **{leaf: LEAF_LIBRARY_DEPS[leaf] | SCH_LEAVES for leaf in SCH_LEAVES},
    **{leaf: LEAF_LIBRARY_DEPS[leaf] | PCB_LEAVES for leaf in PCB_LEAVES},
}

PCB_IMPLEMENTATIONS = {"kicad-board", "pcb-engine"} | PCB_LEAVES
AGENT_CRATES = {"gordian", "gordian-core", "gordian-runtime", "pcb-workflow"}


def workspace_graph(kinds: tuple[str | None, ...]) -> dict[str, set[str]]:
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
            if dependency.get("kind") in kinds:
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
    graph = workspace_graph((None, "normal", "build"))
    dev_graph = workspace_graph(("dev",))
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

    for model, allowed in sorted(MODEL_CRATES.items()):
        if model not in graph:
            errors.append(f"model crate {model} is missing from the workspace")
            continue
        for dependency in sorted(graph[model] - allowed):
            errors.append(
                f"model crate {model} depends on {dependency}; "
                f"a model crate may only depend on {sorted(allowed)}"
            )

    for leaf, allowed in sorted(LEAF_LIBRARY_DEPS.items()):
        if leaf not in graph:
            errors.append(f"algorithm leaf {leaf} is missing from the workspace")
            continue
        for dependency in sorted(graph[leaf] - allowed):
            errors.append(
                f"algorithm leaf {leaf} links {dependency}; a leaf library may only "
                f"depend on {sorted(allowed)} — inject the collaborator as a trait "
                f"from its model crate instead"
            )
        for dependency in sorted(dev_graph[leaf] - LEAF_TEST_DEPS[leaf]):
            errors.append(
                f"algorithm leaf {leaf} dev-depends on {dependency}; a leaf's tests may "
                f"only reach {sorted(LEAF_TEST_DEPS[leaf])} — they must run with no "
                f"orchestrator, no agent and no KiCAD installation"
            )

    core_forbidden = graph.get("gordian-core", set()) & PCB_IMPLEMENTATIONS
    for dependency in sorted(core_forbidden):
        errors.append(
            f"gordian-core bypasses pcb-workflow and depends on PCB implementation {dependency}"
        )

    workflow_dependencies = graph.get("pcb-workflow", set())
    for required in ("kicad", "kicad-board", "pcb-engine"):
        if required not in workflow_dependencies:
            errors.append(f"pcb-workflow must depend on boundary crate {required}")

    if errors:
        print("crate architecture check failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print("crate architecture check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
