#!/usr/bin/env python3
"""Check the small, source-level realtime documentation contract.

This gate deliberately inspects only the composition-root route block, the
ticket route, the minimal sample, and the operator-facing configuration/docs.
It does not start a server or connect to a service. Rust comments are removed
before inspecting code structure. The runnable sample delegates wire handling
to the SDK, whose canonical ticket/path/subprotocol are checked here.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXPECTED_WS_PATHS = {"/ws", "/v1/realtime/ws"}
SAMPLE_WS_PATH = "/ws"
CANONICAL_SUBPROTOCOL = "orbisync.v1.protobuf"

_REQUIRED_FILES = (
    "crates/orbisync-server/src/main.rs",
    "crates/orbisync-transport-http/src/lib.rs",
    "examples/minimal-client-typescript/src/main.ts",
    "sdk/typescript/src/session.ts",
    "sdk/typescript/src/client.ts",
    "examples/minimal-client-typescript/README.md",
    "docs/operations/deployment.md",
    "deploy/compose/.env.example",
    "deploy/compose/compose.dev.yml",
    "orbisync.toml.example",
    "README.md",
    "apps/reference-console-web/README.md",
    "apps/reference-console-web/src/main.ts",
    "sdk/typescript/src/client.ts",
    "apps/admin-console-web/README.md",
)


def read(root: Path, relative: str) -> str:
    path = root / relative
    if not path.exists():
        raise FileNotFoundError(relative)
    return path.read_text(encoding="utf-8")


def strip_rust_comments(source: str) -> str:
    """Replace Rust comments with spaces while preserving string literals."""

    out = list(source)
    i = 0
    n = len(source)
    block_depth = 0
    while i < n:
        if block_depth:
            if source.startswith("/*", i):
                out[i : i + 2] = [" ", " "]
                block_depth += 1
                i += 2
            elif source.startswith("*/", i):
                out[i : i + 2] = [" ", " "]
                block_depth -= 1
                i += 2
            else:
                if source[i] != "\n":
                    out[i] = " "
                i += 1
            continue

        if source.startswith("//", i):
            out[i : i + 2] = [" ", " "]
            i += 2
            while i < n and source[i] != "\n":
                out[i] = " "
                i += 1
            continue
        if source.startswith("/*", i):
            out[i : i + 2] = [" ", " "]
            block_depth = 1
            i += 2
            continue

        # Skip over literals as code so a comment marker inside one is not
        # treated as a comment. Keep literal contents for route regexes.
        # A lifetime such as 'static is not a character literal.
        is_char = source[i] == "'" and re.match(r"'(?:[^'\\\n]|\\(?:.|u\{[0-9a-fA-F_]+\}))'", source[i:])
        if source[i] == '"' or is_char:
            quote = source[i]
            i += 1
            while i < n:
                if source[i] == "\\":
                    i += 2
                elif source[i] == quote:
                    i += 1
                    break
                else:
                    i += 1
            continue
        raw = re.match(r"r(#+)\"", source[i:])
        if raw:
            hashes = raw.group(1)
            i += len(raw.group(0))
            terminator = '"' + hashes
            end = source.find(terminator, i)
            i = n if end == -1 else end + len(terminator)
            continue
        i += 1
    return "".join(out)


def _skip_rust_literal(source: str, start: int) -> int:
    """Return the first offset after a Rust string or raw string literal."""

    if source.startswith("r", start):
        raw = re.match(r"r(#+)?\"", source[start:])
        if raw:
            hashes = raw.group(1) or ""
            body_start = start + len(raw.group(0))
            terminator = '"' + hashes
            end = source.find(terminator, body_start)
            return len(source) if end == -1 else end + len(terminator)
    if source[start] != '"':
        return start + 1
    i = start + 1
    while i < len(source):
        if source[i] == "\\":
            i += 2
        elif source[i] == '"':
            return i + 1
        else:
            i += 1
    return len(source)


def _matching_rust_brace(source: str, opening: int) -> int | None:
    """Find a brace's matching close while ignoring braces in literals."""

    depth = 0
    i = opening
    while i < len(source):
        if source[i] == '"' or source[i] == "r":
            next_offset = _skip_rust_literal(source, i)
            if next_offset != i + 1 or source[i] == '"':
                i = next_offset
                continue
        if source[i] == "{":
            depth += 1
        elif source[i] == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return None


def _ticket_selection_branches(
    server: str,
) -> tuple[str, str, str] | None:
    """Extract the condition, true branch, and false branch of ticket selection."""

    marker = re.search(r"\bif\s+config\.realtime\.allow_stub_ticket\b", server)
    if marker is None:
        return None
    opening = server.find("{", marker.end())
    if opening == -1:
        return None
    true_close = _matching_rust_brace(server, opening)
    if true_close is None:
        return None
    else_match = re.match(r"\s*else\s*\{", server[true_close + 1 :])
    if else_match is None:
        return None
    false_open = true_close + 1 + else_match.end() - 1
    false_close = _matching_rust_brace(server, false_open)
    if false_close is None:
        return None
    condition = server[marker.end() : opening].strip()
    return condition, server[opening + 1 : true_close], server[false_open + 1 : false_close]


def server_websocket_paths(root: Path = ROOT) -> set[str]:
    """Extract only handler routes from the composition-root ws router."""

    source = strip_rust_comments(read(root, "crates/orbisync-server/src/main.rs"))
    marker = "let ws_router = axum::Router::new()"
    start = source.find(marker)
    if start == -1:
        return set()
    end = source.find(".with_state", start)
    if end == -1:
        return set()
    block = source[start:end]
    route_pattern = re.compile(
        r'\.route\(\s*"([^"]+)"\s*,\s*'
        r'axum::routing::get\(\s*realtime_ws::realtime_ws_handler\s*\)\s*,?\s*\)'
    )
    return {match.group(1) for match in route_pattern.finditer(block)}


def check_server_ticket_composition(root: Path = ROOT) -> list[str]:
    """Check executable verifier selection and the HTTP ticket route."""

    try:
        server = strip_rust_comments(read(root, "crates/orbisync-server/src/main.rs"))
        http = strip_rust_comments(read(root, "crates/orbisync-transport-http/src/lib.rs"))
    except FileNotFoundError as error:
        return [f"missing contract input: {error.args[0]}"]
    errors: list[str] = []
    branches = _ticket_selection_branches(server)
    if branches is None:
        errors.append(
            "server ticket selection must contain an if/else allow_stub_ticket branch"
        )
    else:
        condition, true_branch, false_branch = branches
        if condition:
            errors.append(
                "allow_stub_ticket condition must be the direct positive flag (true selects stub)"
            )
        if not re.search(r"Arc::new\(\s*StubTicketVerifier::new\(\)\s*\)", true_branch):
            errors.append("true allow_stub_ticket branch must select StubTicketVerifier")
        if re.search(r"HmacRealtimeTicketVerifier::new\(", true_branch):
            errors.append("true allow_stub_ticket branch must not select HmacTicketVerifier")
        if not re.search(r"HmacRealtimeTicketVerifier::new\(", false_branch):
            errors.append(
                "false allow_stub_ticket branch must select HmacRealtimeTicketVerifier"
            )
        if re.search(r"StubTicketVerifier::new\(\)", false_branch):
            errors.append("false allow_stub_ticket branch must not select StubTicketVerifier")
    if not re.search(
        r"\.with_realtime_ticket_store\(\s*Arc::clone\(&realtime_ticket_store\)\s*\)",
        server,
    ):
        errors.append("server ticket composition must wire the realtime ticket store")
    if not re.search(
        r'\.route\(\s*"/v1/realtime/tickets"\s*,\s*'
        r'post\(\s*auth::create_realtime_ticket\s*\)\s*\)',
        http,
    ):
        errors.append("HTTP router must serve POST /v1/realtime/tickets")
    return errors


def section(text: str, heading: str) -> str:
    """Return one TOML section, excluding the next section heading."""

    match = re.search(
        rf"(?ms)^\[{re.escape(heading)}\]\s*$\n(?P<body>.*?)(?=^\[|\Z)", text
    )
    return match.group("body") if match else ""


def has_env_assignment(text: str, name: str) -> bool:
    return re.search(rf"(?m)^\s*{re.escape(name)}\s*=\s*\S+\s*$", text) is not None


def strip_yaml_comments(text: str) -> str:
    """Remove Compose comments without broadening the checked YAML scope."""

    return "\n".join(line.split("#", 1)[0] for line in text.splitlines())


# Match the legacy root path, but not the valid /v1/realtime/* namespace.
# This is intentionally path-shaped instead of a substring search: "realtime"
# is also a valid part of the ticket endpoint and protocol documentation.
_LEGACY_WS_PATH = re.compile(r"(?<!/v1)/realtime(?=(?:/|$|[\s`\"'(),.:;]))")


def check_contract(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    try:
        files = {relative: read(root, relative) for relative in _REQUIRED_FILES}
    except FileNotFoundError as error:
        return [f"missing contract input: {error.args[0]}"]

    server_source = files["crates/orbisync-server/src/main.rs"]
    sample_source = files["examples/minimal-client-typescript/src/main.ts"]
    sample_readme = files["examples/minimal-client-typescript/README.md"]
    reference_source = files["apps/reference-console-web/src/main.ts"]
    sdk_source = files["sdk/typescript/src/client.ts"]
    deployment = files["docs/operations/deployment.md"]
    config_example = files["orbisync.toml.example"]
    compose_env = files["deploy/compose/.env.example"]
    compose = files["deploy/compose/compose.dev.yml"]
    compose_code = strip_yaml_comments(compose)

    actual_paths = server_websocket_paths(root)
    if actual_paths != EXPECTED_WS_PATHS:
        errors.append(
            "server realtime routes changed: "
            f"expected {sorted(EXPECTED_WS_PATHS)}, found {sorted(actual_paths)}"
        )
    errors.extend(check_server_ticket_composition(root))

    # The runnable starter delegates ticket/wire handling to the SDK.
    connector = files["sdk/typescript/src/session.ts"]
    sdk = files["sdk/typescript/src/client.ts"]
    for required in ("connectSession", "movementView"):
        if required not in sample_source:
            errors.append(f"sample must use {required}")
    for required in ("client.auth.methods()", "client.connect()", "connection.join(", "instance.ready()"):
        if required not in connector:
            errors.append(f"sample connector must use {required}")
    if '/v1/realtime/tickets' not in sdk or 'parseRealtimeTicketResponse(json)' not in sdk:
        errors.append("SDK must fetch and validate the realtime ticket")
    if '?? "/ws"' not in sdk or CANONICAL_SUBPROTOCOL not in sdk:
        errors.append("SDK must use canonical realtime path and subprotocol")
    for source in (sample_source, connector, sdk):
        if "orbisync.realtime.v1" in source:
            errors.append("sample contains obsolete WebSocket subprotocol")
        if _LEGACY_WS_PATH.search(source):
            errors.append("sample contains obsolete root WebSocket path /realtime")
    if "new OrbiSyncClient" not in reference_source or "client.connect()" not in reference_source:
        errors.append("reference web must use the SDK default for WebSocket setup")
    if "orbisync.realtime.v1" in reference_source:
        errors.append("reference web sample contains obsolete WebSocket subprotocol")
    if _LEGACY_WS_PATH.search(reference_source):
        errors.append("reference web sample contains obsolete root WebSocket path /realtime")

    # Readmes are operator-facing contracts. Exact path matching avoids
    # treating /v1/realtime/tickets or /v1/realtime/ws as the legacy path.
    for name, text in (
        ("sample README", sample_readme),
        ("reference web README", files["apps/reference-console-web/README.md"]),
        ("admin web README", files["apps/admin-console-web/README.md"]),
        ("root README", files["README.md"]),
    ):
        if _LEGACY_WS_PATH.search(text):
            errors.append(f"{name} contains obsolete /realtime WebSocket path")
    if not re.search(r"(?<![\w/])/ws(?![\w/])", sample_readme):
        errors.append("sample README must document /ws")
    if "POST /v1/realtime/tickets" not in sample_readme:
        errors.append("sample README must document the realtime ticket endpoint")

    # These are exact obsolete claims, not a substring ban. Historical review
    # notes are intentionally not part of this current-contract file set.
    obsolete_phrases = (
        "no real ticket verifier exists",
        "rejects every realtime connection",
        "nothing reads this key",
        "still accepts any non-empty",
    )
    current_docs = (
        ("deployment guide", deployment),
        ("config example", config_example),
        ("reference web README", files["apps/reference-console-web/README.md"]),
        ("admin web README", files["apps/admin-console-web/README.md"]),
        ("sample README", sample_readme),
        ("root README", files["README.md"]),
    )
    for name, text in current_docs:
        folded = text.casefold()
        for phrase in obsolete_phrases:
            if re.search(rf"(?<![\w-]){re.escape(phrase)}(?![\w-])", folded):
                errors.append(f"{name} contains obsolete realtime verifier text: {phrase}")

    if "HmacRealtimeTicketVerifier" not in deployment or "single-use" not in deployment:
        errors.append("deployment guide must describe the database-backed one-time verifier")
    if not re.search(
        r"`?(?:realtime\.)?allow_stub_ticket`?\s*(?:=|defaults to)\s*`?false",
        deployment,
        re.IGNORECASE,
    ):
        errors.append("deployment guide must document allow_stub_ticket=false as the default")
    if "/v1/realtime/tickets" not in deployment or "/v1/realtime/ws" not in deployment:
        errors.append("deployment guide must document the served ticket and WebSocket routes")
    if "explicitly selects `StubTicketVerifier`" not in deployment:
        errors.append("deployment guide must say stub mode is selected only when explicitly enabled")
    if "never combine stub mode with a public bind" not in deployment.casefold():
        errors.append("deployment guide must forbid public exposure of stub mode")

    realtime_config = section(config_example, "realtime")
    server_config = section(config_example, "server")
    if not re.search(r'(?m)^\s*bind\s*=\s*"127\.0\.0\.1:8080"\s*(?:#.*)?$', server_config):
        errors.append("orbisync.toml.example must keep the sample server loopback-bound")
    if not re.search(r"(?m)^\s*allow_stub_ticket\s*=\s*false\s*(?:#.*)?$", realtime_config):
        errors.append("orbisync.toml.example must set realtime.allow_stub_ticket = false")
    auth_config = section(config_example, "auth")
    if not re.search(
        r'(?m)^\s*realtime_ticket_hmac_key_env\s*=\s*"ORBISYNC_REALTIME_TICKET_HMAC_KEY"\s*(?:#.*)?$',
        auth_config,
    ):
        errors.append("orbisync.toml.example must name ORBISYNC_REALTIME_TICKET_HMAC_KEY")
    if not has_env_assignment(compose_env, "ORBISYNC_REALTIME_TICKET_HMAC_KEY"):
        errors.append("compose .env example must provide ORBISYNC_REALTIME_TICKET_HMAC_KEY")
    if not re.search(r"(?m)^\s*ORBISYNC_REALTIME_ALLOW_STUB_TICKET\s*=\s*false\s*$", compose_env):
        errors.append("compose .env example must set ORBISYNC_REALTIME_ALLOW_STUB_TICKET=false")
    if not has_env_assignment(compose_env, "ORBISYNC_HOST_BIND"):
        errors.append("compose .env example must provide ORBISYNC_HOST_BIND")
    if "ORBISYNC_SERVER_BIND: 0.0.0.0:8080" not in compose_code:
        errors.append("Compose must bind the server container to 0.0.0.0:8080")
    if "ORBISYNC_REALTIME_ALLOW_STUB_TICKET: ${ORBISYNC_REALTIME_ALLOW_STUB_TICKET:-false}" not in compose_code:
        errors.append("Compose must pass the realtime stub setting with a false default")
    if '"${ORBISYNC_HOST_BIND:-127.0.0.1}:${ORBISYNC_PORT:-8080}:8080"' not in compose_code:
        errors.append("Compose must publish the server on the loopback host by default")

    if "let ws_router = axum::Router::new()" not in strip_rust_comments(server_source):
        errors.append("server route contract anchor is missing from main.rs")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    root = args.root.resolve()
    errors = check_contract(root)
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        if args.json:
            print(json.dumps({"errors": errors}, indent=2))
        return 1
    print(
        "realtime sample contract: "
        f"server_paths={sorted(server_websocket_paths(root))} "
        f"sample_path={SAMPLE_WS_PATH} subprotocol={CANONICAL_SUBPROTOCOL} OK"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
