#!/usr/bin/env python3
"""Enforce the crate dependency rules of `docs/design/repo-crate-conventions.md`.

The script reads `cargo metadata` and verifies the acceptance conditions of
§8.1 that can be decided from the dependency graph:

1. `domain` has no Axum, SQLx, Tokio or protocol generated code in its
   dependency graph (condition 1).
2. `application` cannot name an HTTP, DB or WebSocket type, because none of
   those crates is reachable from it (structural half of condition 2).
3. The crate dependency graph has no cycle (condition 3). Normal dependency
   cycles are already rejected by Cargo, so the check also walks `dev` and
   `build` edges, where Cargo does allow a cycle.
4. `world-runtime` does not depend on `interest` or `realtime` (condition 4).
5. `interest` does not depend on `realtime` (condition 5).

It additionally checks the allowed dependency matrix of §3.2 and the rule that
production crates reach `testkit` through dev-dependencies only (§2.2).

Usage:

    python scripts/check_architecture.py [--json]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Allowed internal (workspace) dependencies per crate, from
# `repo-crate-conventions.md` §3.2. An empty set means the crate must not depend
# on any other workspace crate.
ALLOWED_INTERNAL: dict[str, set[str]] = {
    "orbisync-domain": set(),
    "orbisync-config": set(),
    "orbisync-application": {"orbisync-domain"},
    "orbisync-protocol": {"orbisync-domain"},
    "orbisync-interest": {"orbisync-domain"},
    "orbisync-identity": {"orbisync-domain", "orbisync-application"},
    "orbisync-world-runtime": {"orbisync-domain", "orbisync-application"},
    "orbisync-world-directory": {"orbisync-domain", "orbisync-application"},
    "orbisync-realtime": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-protocol",
        "orbisync-config",
        "orbisync-identity",
    },
    "orbisync-storage-postgres": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-config",
    },
    "orbisync-transport-http": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-config",
        "orbisync-identity",
    },
    "orbisync-extensions": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-config",
    },
    "orbisync-observability": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-config",
    },
    "orbisync-testkit": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-protocol",
        "orbisync-realtime",
        "orbisync-world-runtime",
        "orbisync-world-directory",
        "orbisync-interest",
        "orbisync-identity",
        "orbisync-config",
    },
    "orbisync-server": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-protocol",
        "orbisync-realtime",
        "orbisync-world-runtime",
        "orbisync-world-directory",
        "orbisync-interest",
        "orbisync-identity",
        "orbisync-storage-postgres",
        "orbisync-transport-http",
        "orbisync-extensions",
        "orbisync-observability",
        "orbisync-config",
    },
    "orbisync-e2e-helper": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-protocol",
        "orbisync-realtime",
        "orbisync-world-runtime",
        "orbisync-interest",
        "orbisync-config",
        "orbisync-testkit",
        "orbisync-server",
    },
    # Workspace-wide integration tests are test code; they may drive any crate.
    "orbisync-integration-tests": {
        "orbisync-domain",
        "orbisync-application",
        "orbisync-protocol",
        "orbisync-realtime",
        "orbisync-world-runtime",
        "orbisync-world-directory",
        "orbisync-interest",
        "orbisync-identity",
        "orbisync-storage-postgres",
        "orbisync-transport-http",
        "orbisync-extensions",
        "orbisync-observability",
        "orbisync-config",
        "orbisync-testkit",
    },
}

# Framework crates that must not appear in the graph of a pure crate
# (`architecture.md` §2.1, `repo-crate-conventions.md` §3.3).
FORBIDDEN_IN_PURE_GRAPH = {
    "axum": "HTTP framework",
    "sqlx": "database driver",
    "sqlx-core": "database driver",
    "sqlx-postgres": "database driver",
    "tokio": "async runtime",
    "prost": "protocol generated code",
    "prost-types": "protocol generated code",
    "tower": "HTTP middleware",
    "tower-http": "HTTP middleware",
    "hyper": "HTTP implementation",
    "tokio-tungstenite": "WebSocket implementation",
}

# Crates whose normal dependency graph must stay free of the crates above.
PURE_CRATES = ("orbisync-domain", "orbisync-application", "orbisync-interest")

# ADR-007: the production server cannot embed an extension loader. Walk the
# transitive normal graph (including target-specific edges), not Cargo.lock:
# build/dev tools may use these packages without becoming server code.
EXTENSION_LOADERS = {
    "libloading", "dlopen", "dlopen2", "libffi", "libffi-sys",
    "wasmtime", "wasmi", "wasmer", "extism",
}

NATIVE_LOADER_API = re.compile(
    r"\b(?:dlopen|dlmopen|dlsym|LoadLibrary(?:Ex)?[AW]?|LdrLoadDll|GetProcAddress)\b"
)


def extension_source_errors(source: str, path: str) -> list[str]:
    # Conservative lexical guard also rejects imported aliases and declarations.
    # It intentionally covers every Rust source, including target-gated code.
    return [f"ADR-007: native loader API {name} in {path}"
            for name in sorted(set(NATIVE_LOADER_API.findall(source)))]


def extension_loader_errors(members, nodes, names) -> list[str]:
    server = members.get("orbisync-server")
    if server is None:
        return ["ADR-007: production server missing from dependency graph"]
    found = transitive(server, nodes, names) & EXTENSION_LOADERS
    return [f"ADR-007: in-process extension loader reachable from server: {name}"
            for name in sorted(found)]


def load_metadata() -> dict:
    """Returns `cargo metadata` for the workspace."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        print(result.stderr, file=sys.stderr)
        raise SystemExit("cargo metadata failed")
    return json.loads(result.stdout)


def build_index(metadata: dict) -> tuple[dict[str, str], dict[str, dict]]:
    """Maps package id to name and package id to resolve node."""
    names = {package["id"]: package["name"] for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    return names, nodes


def edges(node: dict, kinds: set[str | None]) -> list[str]:
    """Returns the dependency ids of `node` restricted to the given kinds."""
    out = []
    for dependency in node["deps"]:
        dep_kinds = {entry.get("kind") for entry in dependency.get("dep_kinds", [])}
        if not dep_kinds:
            dep_kinds = {None}
        if dep_kinds & kinds:
            out.append(dependency["pkg"])
    return out


def transitive(start: str, nodes: dict[str, dict], names: dict[str, str]) -> set[str]:
    """Returns the transitive normal-dependency closure of `start`, by name."""
    seen: set[str] = set()
    stack = [start]
    while stack:
        current = stack.pop()
        for dependency in edges(nodes[current], {None}):
            if dependency in seen:
                continue
            seen.add(dependency)
            stack.append(dependency)
    return {names[package_id] for package_id in seen}


def find_cycle(graph: dict[str, list[str]]) -> list[str] | None:
    """Returns one cycle in `graph`, or `None` when the graph is acyclic."""
    visiting: set[str] = set()
    done: set[str] = set()
    path: list[str] = []

    def walk(node: str) -> list[str] | None:
        if node in done:
            return None
        if node in visiting:
            start = path.index(node)
            return [*path[start:], node]
        visiting.add(node)
        path.append(node)
        for neighbour in graph.get(node, []):
            cycle = walk(neighbour)
            if cycle:
                return cycle
        path.pop()
        visiting.discard(node)
        done.add(node)
        return None

    for node in sorted(graph):
        cycle = walk(node)
        if cycle:
            return cycle
    return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--json", action="store_true", help="print findings as JSON")
    args = parser.parse_args()

    metadata = load_metadata()
    names, nodes = build_index(metadata)
    members = {names[package_id]: package_id for package_id in metadata["workspace_members"]}
    errors: list[str] = []
    errors.extend(extension_loader_errors(members, nodes, names))
    extension_sources = sorted((ROOT / "crates").glob("*/src/**/*.rs"))
    if not extension_sources:
        errors.append("ADR-007: no production Rust sources scanned")
    for source_path in extension_sources:
        errors.extend(extension_source_errors(
            source_path.read_text(encoding="utf-8"), str(source_path.relative_to(ROOT))))

    # Self-check: the architecture gate must actually scan something.
    # If the workspace is 0 members while cargo metadata succeeds, the gate is
    # misconfigured and would silently be green (partial-set inactivity not
    # caught by overall error count).
    if not members:
        errors.append(
            "architecture gate: no workspace members were scanned — "
            "expected >0 members (check cargo metadata path logic)"
        )

    unknown = sorted(set(members) - set(ALLOWED_INTERNAL))
    if unknown:
        errors.append(
            "workspace member(s) missing from the allowed dependency matrix: "
            + ", ".join(unknown)
        )

    # §3.2 allowed dependency matrix, evaluated on direct normal dependencies.
    for name, package_id in sorted(members.items()):
        allowed = ALLOWED_INTERNAL.get(name)
        if allowed is None:
            continue
        direct = {
            names[dependency]
            for dependency in edges(nodes[package_id], {None})
            if names[dependency] in members
        }
        for dependency in sorted(direct - allowed):
            errors.append(
                f"{name} depends on {dependency}, which the allowed dependency "
                "matrix forbids (repo-crate-conventions.md §3.2)"
            )

    # §8.1 conditions 1 and 2: the pure crates carry no framework dependency.
    # Self-check for A2: each pure crate's transitive graph must be non-empty;
    # an empty graph while members exist would be silently green.
    for name in PURE_CRATES:
        package_id = members.get(name)
        if package_id is None:
            errors.append(f"{name} is not a workspace member")
            continue
        graph = transitive(package_id, nodes, names)
        if not graph and members:
            errors.append(
                f"architecture gate: no transitive dependencies were scanned for {name} — "
                "expected >0 deps (check transitive path logic)"
            )
        for forbidden, reason in sorted(FORBIDDEN_IN_PURE_GRAPH.items()):
            if forbidden in graph:
                errors.append(
                    f"{name} has {forbidden} ({reason}) in its dependency graph "
                    "(repo-crate-conventions.md §8.1 conditions 1 and 2)"
                )

    # §8.1 condition 3: no cycle, including dev and build edges.
    internal_graph = {
        name: sorted(
            names[dependency]
            for dependency in edges(nodes[package_id], {None, "dev", "build"})
            if names[dependency] in members
        )
        for name, package_id in members.items()
    }
    # Self-check for A3/A4: allowed-matrix and cycle detection share the
    # internal graph; if it is empty while members exist the gate would be green.
    # Use edge count rather than node count because the dict always has an entry per member.
    total_edges = sum(len(v) for v in internal_graph.values())
    if members and total_edges == 0:
        errors.append(
            "architecture gate: no internal dependency edges were scanned — "
            "expected >0 edges (check cargo metadata path logic)"
        )
    if not internal_graph and members:
        errors.append(
            "architecture gate: no internal dependency graph was built — "
            "expected >0 nodes (check cargo metadata path logic)"
        )
    cycle = find_cycle(internal_graph)
    if cycle:
        errors.append(
            "dependency cycle across normal, dev and build edges: " + " -> ".join(cycle)
        )

    # §2.2: production crates reach testkit through dev-dependencies only.
    for name, package_id in sorted(members.items()):
        if name in ("orbisync-testkit", "orbisync-integration-tests", "orbisync-e2e-helper"):
            continue
        normal = {names[dependency] for dependency in edges(nodes[package_id], {None})}
        if "orbisync-testkit" in normal:
            errors.append(
                f"{name} depends on orbisync-testkit outside dev-dependencies "
                "(repo-crate-conventions.md §2.2)"
            )

    summary = {
        "workspace_members": len(members),
        "pure_crates_checked": list(PURE_CRATES),
        "extension_boundary_sources": len(extension_sources),
        "errors": len(errors),
    }
    if args.json:
        print(json.dumps({"summary": summary, "errors": errors}, indent=2))
    else:
        for key, value in summary.items():
            print(f"{key}={value}")
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)

    if errors:
        return 1
    print("OrbiSync architecture check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
