#!/usr/bin/env python3
"""Ensure realtime WebSocket writes stay behind one transport boundary."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOCKET_FILE = Path("crates/orbisync-server/src/realtime_ws_socket.rs")
REALTIME_GLOB = "crates/orbisync-server/src/realtime_ws*.rs"
IMPLEMENTATION_SEND = re.compile(r"\bself\.inner\.send\s*\(")
MESSAGE_VARIANT = re.compile(r"\bMessage::(?:Binary|Text|Close|Ping|Pong)\b")
SEND_CALL = re.compile(
    r"\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*\s*\.\s*send\s*\(\s*"
    r"(?P<argument>Message::(?:Binary|Text|Close|Ping|Pong)\b|[A-Za-z_]\w*)"
)
MESSAGE_BINDING = re.compile(
    r"\blet\s+(?:mut\s+)?(?P<let_name>[A-Za-z_]\w*)"
    r"\s*(?::\s*(?:[A-Za-z_]\w*::)*Message)?\s*=\s*(?P<value>.*?);",
    re.DOTALL,
)
MESSAGE_PARAMETER = re.compile(
    r"\b(?P<param_name>[A-Za-z_]\w*)\s*:\s*(?:[A-Za-z_]\w*::)*Message\b"
)


def _message_bindings(source: str) -> set[str]:
    """Find local values whose source-level type/value is a WebSocket message."""

    bindings: set[str] = set()
    for match in MESSAGE_BINDING.finditer(source):
        name = match.group("let_name")
        if name and (
            MESSAGE_VARIANT.search(match.group("value"))
            or re.search(r"\bMessage\b", match.group(0))
        ):
            bindings.add(name)
    bindings.update(match.group("param_name") for match in MESSAGE_PARAMETER.finditer(source))
    return bindings


def _direct_message_send_lines(source: str) -> list[int]:
    """Return lines sending a WebSocket Message, including through a local binding."""

    bindings = _message_bindings(source)
    lines: list[int] = []
    for match in SEND_CALL.finditer(source):
        argument = match.group("argument")
        if MESSAGE_VARIANT.fullmatch(argument) or argument in bindings:
            lines.append(source.count("\n", 0, match.start()) + 1)
    return lines


def inspect_socket_writes(root: Path = ROOT) -> dict[str, object]:
    """Return the single implementation count and any direct-write bypasses."""
    socket_path = root / SOCKET_FILE
    realtime_files = sorted(root.glob(REALTIME_GLOB))
    implementation_count = 0
    bypasses: list[str] = []

    if socket_path.is_file():
        socket_text = socket_path.read_text(encoding="utf-8")
        implementation_count = len(IMPLEMENTATION_SEND.findall(socket_text))

    for path in realtime_files:
        if path == socket_path:
            continue
        text = path.read_text(encoding="utf-8")
        for line_number in _direct_message_send_lines(text):
            bypasses.append(f"{path.relative_to(root).as_posix()}:{line_number}")

    return {
        "socket_write_implementation_count": implementation_count,
        "direct_write_bypasses": bypasses,
    }


def main() -> int:
    """Run the realtime socket write boundary check."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit machine-readable output")
    args = parser.parse_args()

    result = inspect_socket_writes()
    ok = result["socket_write_implementation_count"] == 1 and not result[
        "direct_write_bypasses"
    ]
    output = {"ok": ok, **result}
    if args.json:
        print(json.dumps(output, sort_keys=True))
    else:
        print(
            "realtime socket write implementation: "
            f"{result['socket_write_implementation_count']}"
        )
        print(f"direct write bypasses: {len(result['direct_write_bypasses'])}")
        for bypass in result["direct_write_bypasses"]:
            print(f"  {bypass}")
    if not ok:
        print("realtime socket writes must have exactly one implementation and no bypasses", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
