#!/usr/bin/env python3
"""Ensure production instance work is routed through task-owned handles."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SERVER_SRC = Path("crates/orbisync-server/src")
PRODUCTION_FILES = ("main.rs", "runtime_tick.rs", "runtime_maintenance.rs", "realtime_ws*.rs")


def _production_sources(root: Path) -> list[Path]:
    source_root = root / SERVER_SRC
    paths: set[Path] = set()
    for pattern in PRODUCTION_FILES:
        paths.update(source_root.glob(pattern))
    return sorted(
        path
        for path in paths
        if path.is_file() and not path.name.startswith("realtime_ws_tests")
    )


def inspect_instance_task_ownership(root: Path = ROOT) -> dict[str, object]:
    """Return structural facts about the production registry boundary."""
    sources = _production_sources(root)
    text = "\n".join(path.read_text(encoding="utf-8") for path in sources)
    forbidden = []
    for marker in ("registry.inner()", "tick_registry.inner()", "actor.submit("):
        if marker in text:
            forbidden.append(marker)
    return {
        "production_files": len(sources),
        "legacy_registry_lock_bypasses": forbidden,
        "task_spawn_boundary": "ensure_instance(actor)" in text,
        "handle_tick_boundary": ".handles()" in text and ".tick(now, checkpoint_due)" in text,
        "handle_submit_boundary": ".registry\n                            .submit(" in text
        or ".registry.submit(" in text,
    }


def main() -> int:
    """Run the instance task ownership check."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit machine-readable output")
    args = parser.parse_args()

    result = inspect_instance_task_ownership()
    ok = (
        not result["legacy_registry_lock_bypasses"]
        and result["task_spawn_boundary"]
        and result["handle_tick_boundary"]
        and result["handle_submit_boundary"]
    )
    output = {"ok": ok, **result}
    if args.json:
        print(json.dumps(output, sort_keys=True))
    else:
        for key, value in result.items():
            print(f"{key}: {value}")
    if not ok:
        print(
            "production instance work must use task-owned handles and avoid registry actor processing",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
