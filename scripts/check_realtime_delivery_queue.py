#!/usr/bin/env python3
"""Ensure realtime interest filtering feeds the only production queue."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DELIVERY_FILE = Path("crates/orbisync-server/src/realtime_ws_connection_delivery.rs")
RUNTIME_FILE = Path("crates/orbisync-server/src/realtime_ws_connection_runtime.rs")


def inspect_delivery_queue(root: Path = ROOT) -> dict[str, object]:
    """Return structural facts for the interest-to-queue handoff."""
    delivery_path = root / DELIVERY_FILE
    runtime_path = root / RUNTIME_FILE
    delivery = delivery_path.read_text(encoding="utf-8") if delivery_path.is_file() else ""
    runtime = runtime_path.read_text(encoding="utf-8") if runtime_path.is_file() else ""

    filter_pos = max(delivery.find("filter_payload_for_viewer_with_roles("),
                     delivery.find("filter_payload_for_viewer_indexed("),
                     delivery.find("filter_payload_with_index("))
    filtered_queue_pos = delivery.find("self.queue_payload(filtered)")
    raw_queue_pos = delivery.find("self.queue_payload(payload)")
    return {
        "filter_before_queue": filter_pos >= 0 and filtered_queue_pos > filter_pos,
        "raw_payload_queue_bypasses_filter": raw_queue_pos >= 0,
        "runtime_delivery_rx": "delivery_rx" in runtime,
        "runtime_registers_legacy_queue": "state.delivery.register(" in runtime,
        "outbound_queue_allocations": delivery.count("OutboundQueue::from_config")
        + runtime.count("OutboundQueue::from_config"),
    }


def main() -> int:
    """Run the realtime delivery queue boundary check."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit machine-readable output")
    args = parser.parse_args()

    result = inspect_delivery_queue()
    ok = (
        result["filter_before_queue"]
        and not result["raw_payload_queue_bypasses_filter"]
        and not result["runtime_delivery_rx"]
        and not result["runtime_registers_legacy_queue"]
        and result["outbound_queue_allocations"] == 1
    )
    output = {"ok": ok, **result}
    if args.json:
        print(json.dumps(output, sort_keys=True))
    else:
        for key, value in result.items():
            print(f"{key}: {value}")
    if not ok:
        print("realtime delivery must filter before the single production queue", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
