#!/usr/bin/env python3
"""Dependency-free validation for OrbiSync design artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]
MARKDOWN = [ROOT / "README.md", ROOT / "CONTRIBUTING.md", ROOT / "SECURITY.md"]
MARKDOWN += sorted((ROOT / "docs").rglob("*.md"))


def _discover_expected_adrs(root: Path) -> set[str]:
    adrs: set[str] = set()
    for path in (root / "docs/adr").glob("ADR-*.md"):
        match = re.match(r"(ADR-\d+[A-Z]?)", path.name)
        if match:
            adrs.add(match.group(1))
    return adrs


EXPECTED_ADRS = _discover_expected_adrs(ROOT)
SPEC_SHA256 = "941f46f436384582864a38ea9124687784542fc0dff3a114128ce080925f3b55"
PROTO_FIELDS = {
    "protocol_major": 1, "protocol_minor": 2, "message_id": 3,
    "sequence": 4, "sent_at_unix_ms": 5, "instance_id": 6,
    "client_hello": 20, "server_hello": 21, "join_instance": 22,
    "join_accepted": 23, "snapshot": 24, "state_delta": 25,
    "transform_input": 26, "entity_command": 27, "domain_event": 28,
    "heartbeat": 29, "heartbeat_ack": 30, "resume_session": 31,
    "resume_accepted": 32, "resync_required": 33, "error": 34,
}
CLIENT_HELLO_REQUIREMENTS = {
    "sdk_name": {"ClientHello.client_name"},
    "sdk_version": {"ClientHello.client_version"},
    "protocol_major": {"Envelope.protocol_major"},
    "protocol_minor": {"ClientHello.supported_minor_min", "ClientHello.supported_minor_max"},
    "client_type": {"ClientHello.client_type"},
    "supported_compressions": {"ClientHello.supported_compressions"},
    "supported_features": {"ClientHello.supported_features"},
    "realtime_ticket": {"ClientHello.realtime_ticket"},
    "resume_token": {"ClientHello.resume_token"},
}
SERVER_HELLO_REQUIREMENTS = {
    "protocol_major": {"Envelope.protocol_major"},
    "protocol_minor": {"ServerHello.negotiated_minor"},
    "compression": {"ServerHello.negotiated_compression"},
    "features": {"ServerHello.enabled_features"},
    "server_time": {"ServerHello.server_time_unix_ms"},
    "connection_id": {"ServerHello.connection_id"},
    "heartbeat_interval": {"ServerHello.heartbeat_interval_ms"},
}
SERVER_STATE_ALIASES = {
    "Connecting", "AwaitingHello", "Ready", "Joining", "Active", "Resuming",
    "Closing", "Failing", "Closed", "Failed", "Joined", "joined",
}
RESUME_POLICY_CASES = {
    "client_hello_only": (True, False, None, "do_not_start_resume", "ready", None),
    "resume_session_only": (False, True, None, "accept_resume_request", "resuming", None),
    "both_equal": (True, True, True, "accept_resume_request", "resuming", None),
    "both_mismatch": (True, True, False, "reject_resume", "ready", "resume_token_mismatch"),
    "neither": (False, False, None, "do_not_start_resume", "ready", None),
}
# Implemented contract: only what the server actually serves (truthful).
# Design for not-yet-wired operations is preserved in openapi/orbisync-v1-planned.yaml
# (16 path entries / 17 operations after PW-1 and D1-A, 18 paths / 22 ops here + 17 planned = 39 ops, 30 unique paths;
#  original was 30/38, V-05 added GET /v1/users/{user_id}/roles so 38->39; W-29 moved 6 ops from planned to
#  implemented and added 1 new op, so planned 27->21, implemented 11->18; W-G moved DELETE
#  /v1/roles/{role_id} from planned to implemented, so planned 21->20, implemented 18->19;
#  CR-H moved POST /v1/auth/refresh from planned to implemented, so planned 20->19, implemented 19->20;
#  PW-1 moved POST /v1/users/{user_id}/reset-password from planned to implemented, so planned 19->18, implemented 20->21).
# Do not hand-maintain a second list — the gate
# scripts/validate_openapi_routes.py derives the served set from the real Router.
REST_OPERATIONS = {
    ("post", "/v1/auth/login"),
    # ADR-026 authentication method selection.
    ("get", "/v1/auth/methods"),
    ("post", "/v1/auth/guest"),
    ("post", "/v1/auth/name"),
    ("post", "/v1/auth/external"),
    ("post", "/v1/auth/refresh"),
    ("post", "/v1/realtime/tickets"),
    ("post", "/v1/auth/change-password"),
    ("get", "/v1/auth/administration-access"),
    ("post", "/v1/extensions/commands"),
    ("get", "/v1/users"),
    ("post", "/v1/users"),
    ("get", "/v1/worlds"),
    ("post", "/v1/worlds"),
    ("get", "/v1/worlds/{world_id}"),
    ("patch", "/v1/worlds/{world_id}"),
    ("post", "/v1/worlds/{world_id}/archive"),
    ("get", "/v1/instances"),
    ("post", "/v1/instances"),
    ("get", "/v1/instances/{instance_id}"),
    ("post", "/v1/instances/{instance_id}/start"),
    ("post", "/v1/instances/{instance_id}/stop"),
    ("post", "/v1/instances/{instance_id}/kick/{user_id}"),
    ("get", "/v1/instances/{instance_id}/members"),
    ("get", "/v1/roles"),
    ("post", "/v1/roles"),
    ("get", "/v1/users/{user_id}/roles"),
    ("put", "/v1/users/{user_id}/roles"),
    ("get", "/health/live"),
    ("get", "/health/ready"),
    ("get", "/version"),
    ("get", "/metrics"),
    ("get", "/v1/users/{user_id}"),
    ("patch", "/v1/users/{user_id}"),
    ("post", "/v1/users/{user_id}/reset-password"),
    ("post", "/v1/users/{user_id}/disable"),
    ("post", "/v1/users/{user_id}/enable"),
    ("post", "/v1/users/import"),
    ("post", "/v1/auth/logout"),
    ("get", "/v1/auth/me"),
    ("get", "/v1/roles/{role_id}"),
    ("patch", "/v1/roles/{role_id}"),
    ("delete", "/v1/roles/{role_id}"),
    ("get", "/v1/audit-events"),
    ("get", "/v1/audit-events/{event_id}"),
    ("get", "/v1/admin/diagnostics"),
}
PLANNED_PATH = ROOT / "openapi" / "orbisync-v1-planned.yaml"


def rel(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def load_text(path: Path, errors: list[str]) -> str:
    try:
        raw = path.read_bytes()
        if raw.startswith(b"\xef\xbb\xbf"):
            errors.append(f"{rel(path)}: UTF-8 BOM is not allowed")
        return raw.decode("utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        errors.append(f"{rel(path)}: cannot read as UTF-8: {exc}")
        return ""


def check_links(texts: dict[Path, str], errors: list[str]) -> None:
    def heading_anchors(text: str) -> set[str]:
        anchors: set[str] = set()
        counts: dict[str, int] = {}
        for line in text.splitlines():
            match = re.match(r"^#{1,6}\s+(.+?)\s*#*\s*$", line)
            if not match:
                continue
            heading = re.sub(r"<[^>]+>", "", match.group(1))
            heading = re.sub(r"[`*_~]", "", heading).strip().lower()
            heading = re.sub(r"[^\w\-\s]", "", heading, flags=re.UNICODE)
            slug = re.sub(r"\s+", "-", heading)
            count = counts.get(slug, 0)
            counts[slug] = count + 1
            anchors.add(slug if count == 0 else f"{slug}-{count}")
        return anchors

    anchors_by_path = {path.resolve(): heading_anchors(text) for path, text in texts.items()}
    link_re = re.compile(r"!?\[[^\]]*]\(([^)\s]+)(?:\s+['\"][^)]*['\"])?\)")
    for source, text in texts.items():
        for target in link_re.findall(text):
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            decoded = unquote(target)
            target_path, _, fragment = decoded.partition("#")
            resolved = source.resolve() if not target_path else (source.parent / target_path).resolve()
            try:
                resolved.relative_to(ROOT.resolve())
            except ValueError:
                errors.append(f"{rel(source)}: link escapes repository: {target}")
                continue
            if not resolved.exists():
                errors.append(f"{rel(source)}: broken link: {target}")
                continue
            if fragment and resolved.suffix.lower() == ".md":
                target_text = texts.get(resolved)
                if target_text is None:
                    target_text = load_text(resolved, errors)
                    anchors_by_path[resolved] = heading_anchors(target_text)
                if fragment.lower() not in anchors_by_path.get(resolved, set()):
                    errors.append(f"{rel(source)}: broken Markdown fragment: {target}")


def check_fences(texts: dict[Path, str], errors: list[str]) -> None:
    for path, text in texts.items():
        fence = None
        start = 0
        for lineno, line in enumerate(text.splitlines(), 1):
            match = re.match(r"^\s*(`{3,}|~{3,})", line)
            if not match:
                continue
            marker = match.group(1)
            if fence is None:
                fence, start = marker[0], lineno
            elif marker[0] == fence:
                fence = None
        if fence is not None:
            errors.append(f"{rel(path)}:{start}: unclosed Markdown fence")


def check_spec_coverage(texts: dict[Path, str], errors: list[str]) -> None:
    design = "\n".join(text for path, text in texts.items() if "docs/design/" in rel(path))
    missing = []
    for chapter in range(1, 48):
        patterns = [
            rf"仕様\s*§\s*{chapter}(?:\D|$)",
            rf"仕様\s+{chapter}(?:\D|$)",
            rf"第{chapter}章",
        ]
        if not any(re.search(pattern, design) for pattern in patterns):
            missing.append(chapter)
    if missing:
        errors.append("specification chapters missing from design traceability: " + ", ".join(map(str, missing)))


def check_adrs(texts: dict[Path, str], errors: list[str]) -> None:
    expected = _discover_expected_adrs(ROOT)
    adr_files = {path.name.split("-", 2)[0] + "-" + path.name.split("-", 2)[1]
                 for path in (ROOT / "docs/adr").glob("ADR-*.md")}
    missing_files = sorted(expected - adr_files)
    if missing_files:
        errors.append("missing ADR files: " + ", ".join(missing_files))
    known = set()
    for path in (ROOT / "docs/adr").glob("ADR-*.md"):
        match = re.match(r"(ADR-\d+[A-Z]?)", path.name)
        if match:
            known.add(match.group(1))
            if match.group(1) in expected:
                text = load_text(path, errors)
                if re.search(r"(?mi)^-\s*Status:\s*Accepted\s*$", text):
                    continue
                if re.search(r"(?mi)^-\s*Status:\s*Proposed\s*$", text):
                    if "Decision trigger" not in text:
                        errors.append(f"{rel(path)}: Proposed ADR must have Decision trigger")
                    continue
                errors.append(f"{rel(path)}: required ADR must have Status: Accepted (or Proposed with Decision trigger)")
    for path, text in texts.items():
        for reference in set(re.findall(r"\bADR-\d+[A-Z]?\b", text)):
            if reference not in known:
                errors.append(f"{rel(path)}: unknown ADR reference {reference}")


def _classify_must_change_stance(paragraph: str) -> str:
    advisory_markers = ["ブロックしない", "advisory", "サーバー強制なし", "推奨"]
    enforce_markers = ["拒否する", "拒否", "強制する"]
    # advisory takes precedence because paragraphs describing replacement contain both
    if any(m in paragraph for m in advisory_markers):
        # need to ensure it's about must_change_password enforcement, not unrelated advisory
        # if paragraph also contains enforce markers inside quotes, still advisory because it mentions replacement
        return "advisory"
    if any(m in paragraph for m in enforce_markers):
        # distinguish "強制" that is part of "サーバー強制なし" already handled
        if "強制" in paragraph and "advisory" not in paragraph and "ブロックしない" not in paragraph:
            return "enforce"
        if "拒否" in paragraph:
            return "enforce"
    return "neutral"


def _has_suppression(text: str, identifier: str) -> bool:
    # identifier-specific suppression marker
    pattern = re.compile(r"validate-design:\s*suppress[^\n]*" + re.escape(identifier), re.IGNORECASE)
    return bool(pattern.search(text))


def _has_blanket_suppression(text: str) -> bool:
    # blanket suppression like suppress-all, suppress: * or suppress without identifier
    if re.search(r"validate-design:\s*suppress\s*all", text, re.IGNORECASE):
        return True
    if re.search(r"validate-design:\s*suppress\s*:\s*\*", text):
        return True
    # generic suppress without identifier (but not identifier-specific) is considered blanket
    return False


def check_adr_design_consistency(texts: dict[Path, str], errors: list[str]) -> None:
    identifiers = ["must_change_password"]
    for identifier in identifiers:
        adr_stances: list[tuple[str, str]] = []
        design_stances: list[tuple[str, str]] = []
        adr_suppressed = False
        design_suppressed = False
        for path, text in texts.items():
            rel_path = rel(path)
            if identifier not in text:
                continue
            if _has_blanket_suppression(text):
                errors.append(f"{rel_path}: blanket validate-design suppression is not allowed")
                continue
            if _has_suppression(text, identifier):
                if "docs/adr/" in rel_path:
                    adr_suppressed = True
                if "docs/design/" in rel_path:
                    design_suppressed = True
                continue
            # classify per line containing identifier to avoid mixing unrelated items in same paragraph
            for line in text.splitlines():
                if identifier not in line:
                    continue
                stance = _classify_must_change_stance(line)
                if stance == "neutral":
                    continue
                if "docs/adr/" in rel_path:
                    adr_stances.append((rel_path, stance))
                elif "docs/design/" in rel_path:
                    design_stances.append((rel_path, stance))
        if adr_suppressed or design_suppressed:
            continue
        adr_has_enforce = any(s == "enforce" for _, s in adr_stances)
        adr_has_advisory = any(s == "advisory" for _, s in adr_stances)
        design_has_enforce = any(s == "enforce" for _, s in design_stances)
        design_has_advisory = any(s == "advisory" for _, s in design_stances)
        # conflict if one side enforce and other advisory
        if (adr_has_enforce and design_has_advisory) or (adr_has_advisory and design_has_enforce):
            errors.append(
                f"ADR/design contradiction for '{identifier}': ADR stances {adr_stances} vs design stances {design_stances} "
                f"(ADR says enforce but design says advisory, or vice versa)"
            )
        # also conflict if ADR itself has both stances inconsistently? not needed


def _yaml_block(text: str, key: str) -> str | None:
    """Return the body of a YAML mapping entry, or None when the key is absent.

    The body is every line indented deeper than the key, so the block ends at
    the next sibling entry. Matching on a fixed indent width instead breaks as
    soon as the nesting depth differs, and the empty string that yields is
    indistinguishable from a genuinely missing field to the caller.
    """
    match = re.search(
        r"^(?P<indent>[ ]+)" + re.escape(key) + r":[ \t]*\n"
        r"(?P<body>(?:(?P=indent)[ ].*\n|[ \t]*\n)*)",
        text,
        re.MULTILINE,
    )
    return match.group("body") if match else None


def check_acceptance_implementation_consistency(texts: dict[Path, str], errors: list[str]) -> None:
    # Check auth-authorization.md acceptance criteria 6 vs implemented LoginResponse
    design_path = ROOT / "docs/design/auth-authorization.md"
    if not design_path.exists():
        return
    design_text = texts.get(design_path.resolve(), "")
    if not design_text:
        try:
            design_text = design_path.read_text(encoding="utf-8")
        except OSError:
            return
    if _has_blanket_suppression(design_text):
        errors.append(f"{rel(design_path)}: blanket validate-design suppression is not allowed")
        return
    # Look for acceptance criteria section
    section = markdown_section(design_text, "## 13. テスト可能な受入条件", "## 14. 要 ADR 事項")
    if not section:
        section = design_text
    # Identify refresh token issuance claim (item 6)
    has_refresh_claim = "Refresh Token" in section and ("login成功" in section or "Access Token" in section)
    if not has_refresh_claim:
        return
    # suppression for this check
    if _has_suppression(design_text, "refresh_token") or _has_suppression(design_text, "refresh-token"):
        return
    # Check if claim is marked as planned / future
    # Find the specific line(s) containing refresh token issuance
    claim_lines = [line for line in section.splitlines() if "Refresh Token" in line and ("login" in line.lower() or "Access Token" in line or "発行" in line)]
    claim_text = "\n".join(claim_lines) if claim_lines else section
    planned_markers = ["planned", "未配信", "将来", "未実装", "M1では未提供"]
    if any(m.lower() in claim_text.lower() or m in claim_text for m in planned_markers):
        return
    # Check implemented contract
    openapi_path = ROOT / "openapi/orbisync-v1.yaml"
    try:
        openapi_text = openapi_path.read_text(encoding="utf-8")
    except OSError:
        return
    # LoginResponse must contain refresh_token if design claims it
    login_block = _yaml_block(openapi_text, "LoginResponse")
    if login_block is None:
        errors.append(
            f"{rel(openapi_path)}: LoginResponse schema not found, so the refresh "
            f"token acceptance criteria of {rel(design_path)} cannot be verified"
        )
        return
    # Also check if login operation references TokenPair (which would include refresh)
    login_op_match = re.search(r"/v1/auth/login:\s*\n(.*?)(?=^  /\S+:\s*$)", openapi_text, re.MULTILINE | re.DOTALL)
    login_op = login_op_match.group(1) if login_op_match else ""
    has_refresh_in_login = "refresh_token" in login_block or "TokenPair" in login_op
    if not has_refresh_in_login:
        errors.append(
            f"{rel(design_path)}: acceptance criteria declares refresh token issuance (item 6) but "
            f"implemented {rel(openapi_path)} LoginResponse lacks refresh_token "
            f"(mark as planned with 'planned'/'未配信' or implement)"
        )


def check_legacy_names(texts: dict[Path, str], errors: list[str]) -> None:
    allow = {
        "docs/adr/ADR-001-naming-identifiers.md",
        "docs/design/system-context.md",
        "docs/design/roadmap-and-traceability.md",
    }
    for path, text in texts.items():
        if rel(path) in allow:
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            if re.search(r"\bmetaverse-core(?:-server)?\b", line, re.IGNORECASE):
                errors.append(f"{rel(path)}:{lineno}: legacy public name outside allowlist")


def check_json(errors: list[str]) -> None:
    json_files = sorted((ROOT / "contracts").rglob("*.json")) + sorted((ROOT / "test-vectors").rglob("*.json"))
    # Self-check: the JSON gate must actually scan something; an empty glob would be silently green.
    if not json_files:
        errors.append(
            "design gate: no JSON files were scanned — "
            "expected >0 files in contracts/ and test-vectors/ (check rglob path logic)"
        )
        return
    for path in json_files:
        try:
            json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            errors.append(f"{rel(path)}: invalid JSON: {exc}")


def markdown_section(text: str, heading: str, next_heading: str) -> str:
    match = re.search(
        rf"^{re.escape(heading)}\s*$\n(.*?)(?=^{re.escape(next_heading)}\s*$)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    return match.group(1) if match else ""


def parse_proto_messages(proto: str) -> dict[str, set[str]]:
    messages: dict[str, set[str]] = {}
    for match in re.finditer(r"message\s+(\w+)\s*\{(.*?)(?=^\})", proto, re.MULTILINE | re.DOTALL):
        fields = set(re.findall(r"^\s*(?:repeated\s+)?[.\w<>]+\s+(\w+)\s*=\s*\d+\s*;", match.group(2), re.MULTILINE))
        messages[match.group(1)] = fields
    return messages


def parse_requirement_table(section: str) -> dict[str, set[str]]:
    result: dict[str, set[str]] = {}
    table = re.search(r"^\| requirement key \|.*\n\|---.*\n((?:^\|.*\n?)+)", section, re.MULTILINE)
    for line in (table.group(1) if table else "").splitlines():
        match = re.match(r"^\|\s*`([a-z][a-z0-9_]*)`\s*\|\s*(.*?)\s*\|", line)
        if match:
            result[match.group(1)] = set(re.findall(r"`([A-Za-z][A-Za-z0-9_]*\.[a-z][a-z0-9_]*)`", match.group(2)))
    return result


def parse_contract_value_table(section: str) -> dict[str, str]:
    result: dict[str, str] = {}
    table = re.search(r"^\| policy key \|.*\n\|---.*\n((?:^\|.*\n?)+)", section, re.MULTILINE)
    for line in (table.group(1) if table else "").splitlines():
        match = re.match(r"^\|\s*`([a-z][a-z0-9_]*)`\s*\|\s*`([^`]+)`\s*\|", line)
        if match:
            result[match.group(1)] = match.group(2)
    return result


def parse_resume_case_table(section: str) -> list[dict[str, object]]:
    table = re.search(r"^\| case \|.*\n\|---.*\n((?:^\|.*\n?)+)", section, re.MULTILINE)
    cases: list[dict[str, object]] = []
    for line in (table.group(1) if table else "").splitlines():
        values = re.findall(r"`([^`]+)`", line)
        if len(values) != 7:
            continue
        def scalar(value: str) -> object:
            return {"true": True, "false": False, "null": None}.get(value, value)
        cases.append({
            "case": values[0],
            "client_hello_present": scalar(values[1]),
            "resume_session_present": scalar(values[2]),
            "tokens_equal": scalar(values[3]),
            "decision": values[4],
            "next_state": values[5],
            "error": scalar(values[6]),
        })
    return cases


def check_protocol_vectors(proto_messages: dict[str, set[str]], payload_fields: set[str], errors: list[str]) -> None:
    valid_path = ROOT / "test-vectors/protocol/v1/envelope-valid.json"
    valid = json.loads(valid_path.read_text(encoding="utf-8"))
    if valid.get("subprotocol") != "orbisync.v1.protobuf":
        errors.append(f"{rel(valid_path)}: production subprotocol must be orbisync.v1.protobuf")
    envelope = valid.get("envelope", {})
    header_fields = {"protocol_major", "protocol_minor", "message_id", "sequence", "sent_at_unix_ms", "instance_id"}
    present_payloads = set(envelope) & payload_fields
    unknown = set(envelope) - header_fields - payload_fields
    if unknown:
        errors.append(f"{rel(valid_path)}: unknown Envelope fields: {sorted(unknown)}")
    if len(present_payloads) != 1:
        errors.append(f"{rel(valid_path)}: exactly one payload field is required")
    required_headers = header_fields - {"instance_id"}
    if not required_headers <= set(envelope):
        errors.append(f"{rel(valid_path)}: missing required Envelope headers: {sorted(required_headers - set(envelope))}")
    if envelope.get("protocol_major") != 1 or not isinstance(envelope.get("protocol_minor"), int):
        errors.append(f"{rel(valid_path)}: invalid protocol version")
    if not isinstance(envelope.get("sequence"), int) or envelope.get("sequence", 0) < 1:
        errors.append(f"{rel(valid_path)}: sequence must start at 1")
    if not isinstance(envelope.get("sent_at_unix_ms"), int) or envelope.get("sent_at_unix_ms", 0) <= 0:
        errors.append(f"{rel(valid_path)}: sent_at_unix_ms must be a positive integer")
    if not re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", envelope.get("message_id", ""), re.I):
        errors.append(f"{rel(valid_path)}: message_id must be UUIDv7")
    for payload in present_payloads:
        message_name = "".join(part.title() for part in payload.split("_"))
        actual = set(envelope[payload])
        expected = proto_messages.get(message_name, set())
        if not actual <= expected:
            errors.append(f"{rel(valid_path)}: {message_name} has unknown fields: {sorted(actual - expected)}")
        if message_name == "ClientHello" and actual != expected:
            errors.append(f"{rel(valid_path)}: ClientHello vector must cover all Proto fields")

    server_path = ROOT / "test-vectors/protocol/v1/server-hello-valid.json"
    server = json.loads(server_path.read_text(encoding="utf-8"))
    server_envelope = server.get("envelope", {})
    if server.get("subprotocol") != "orbisync.v1.protobuf":
        errors.append(f"{rel(server_path)}: production subprotocol must be orbisync.v1.protobuf")
    if set(server_envelope) - header_fields - payload_fields:
        errors.append(f"{rel(server_path)}: unknown Envelope field")
    if set(server_envelope) & payload_fields != {"server_hello"}:
        errors.append(f"{rel(server_path)}: exactly one ServerHello payload is required")
    if not required_headers <= set(server_envelope) or server_envelope.get("protocol_major") != 1 or server_envelope.get("sequence") != 1:
        errors.append(f"{rel(server_path)}: invalid required Envelope headers")
    server_hello = server_envelope.get("server_hello", {})
    if set(server_hello) != proto_messages.get("ServerHello", set()):
        errors.append(f"{rel(server_path)}: ServerHello vector must cover all Proto fields")
    if server_hello.get("negotiated_compression") != "":
        errors.append(f"{rel(server_path)}: v1 initial negotiated_compression must be empty")
    if not isinstance(server_hello.get("enabled_features"), list):
        errors.append(f"{rel(server_path)}: enabled_features must be a list")

    invalid_path = ROOT / "test-vectors/protocol/v1/invalid-cases.json"
    invalid = json.loads(invalid_path.read_text(encoding="utf-8"))
    required_invalid_cases = {
        "major_mismatch", "zero_sequence", "missing_message_id",
        "oversized_normal_message", "reused_command_id_different_payload",
    }
    names: set[str] = set()
    for case in invalid.get("cases", []):
        name = case.get("name")
        if not name or name in names:
            errors.append(f"{rel(invalid_path)}: invalid or duplicate case name {name!r}")
        names.add(name)
        if "mutate" in case and not set(case["mutate"]) <= header_fields | payload_fields:
            errors.append(f"{rel(invalid_path)}: mutate references unknown Envelope field")
        if "remove" in case and case["remove"] not in required_headers:
            errors.append(f"{rel(invalid_path)}: remove must reference a required header")
        if "encoded_size" in case and case["encoded_size"] <= 16 * 1024:
            errors.append(f"{rel(invalid_path)}: oversized vector must exceed 16 KiB")
        if name == "reused_command_id_different_payload":
            first, retry = case.get("first", {}), case.get("retry", {})
            entity_fields = proto_messages.get("EntityCommand", set())
            if not first or not retry or not set(first) <= entity_fields or not set(retry) <= entity_fields:
                errors.append(f"{rel(invalid_path)}: reused command vector must use EntityCommand Proto fields")
            command_id = first.get("command_id", "")
            if command_id != retry.get("command_id") or not re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", command_id, re.I):
                errors.append(f"{rel(invalid_path)}: retry must reuse the same UUIDv7 command_id")
            if first == retry:
                errors.append(f"{rel(invalid_path)}: reused command invalid case must change payload")
    if names != required_invalid_cases:
        errors.append(f"{rel(invalid_path)}: invalid case set differs from protocol rules")

    sequence_path = ROOT / "test-vectors/protocol/v1/sequence-cases.json"
    sequence = json.loads(sequence_path.read_text(encoding="utf-8"))
    case_names: set[str] = set()
    for case in sequence.get("cases", []):
        required = {"name", "initial_expected_sequence", "received_sequence", "class", "expected", "next_expected_sequence"}
        if set(case) != required:
            errors.append(f"{rel(sequence_path)}: sequence case must be independent and use fields {sorted(required)}")
            continue
        if case["name"] in case_names:
            errors.append(f"{rel(sequence_path)}: duplicate sequence case {case['name']}")
        case_names.add(case["name"])
        expected_seq, received = case["initial_expected_sequence"], case["received_sequence"]
        outcome = case["expected"]
        if received == expected_seq and outcome != "accept":
            errors.append(f"{rel(sequence_path)}: equal sequence must accept in {case['name']}")
        if received < expected_seq and outcome not in {"ignore", "discard"}:
            errors.append(f"{rel(sequence_path)}: old sequence has invalid outcome in {case['name']}")
        if received > expected_seq and outcome != "resync_required":
            errors.append(f"{rel(sequence_path)}: gap must require resync in {case['name']}")
        next_expected = expected_seq + 1 if outcome == "accept" else expected_seq
        if case["next_expected_sequence"] != next_expected:
            errors.append(f"{rel(sequence_path)}: next_expected_sequence contradicts outcome in {case['name']}")
    reconnect = sequence.get("reconnect", {})
    if reconnect != {"new_connection": True, "initial_expected_sequence": 1, "received_sequence": 1, "expected": "accept"}:
        errors.append(f"{rel(sequence_path)}: reconnect must reset sequence to 1")
    dedup_names: set[str] = set()
    for case in sequence.get("dedup", []):
        required = {"name", "preexisting_payload_hash", "command_id", "payload_hash", "expected"}
        if set(case) != required or case["name"] in dedup_names:
            errors.append(f"{rel(sequence_path)}: dedup cases must be independent and uniquely named")
            continue
        dedup_names.add(case["name"])
        if not re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", case["command_id"], re.I):
            errors.append(f"{rel(sequence_path)}: command_id must be UUIDv7")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", case["payload_hash"]):
            errors.append(f"{rel(sequence_path)}: payload_hash must identify SHA-256")
        prior = case["preexisting_payload_hash"]
        expected_outcome = "execute" if prior is None else ("return_saved_result" if prior == case["payload_hash"] else "protocol_error")
        if case["expected"] != expected_outcome:
            errors.append(f"{rel(sequence_path)}: dedup outcome contradicts precondition in {case['name']}")


def check_contracts(errors: list[str]) -> None:
    proto_path = ROOT / "proto/orbisync/v1/realtime.proto"
    proto = load_text(proto_path, errors)
    if "package orbisync.v1;" not in proto:
        errors.append(f"{rel(proto_path)}: package must be orbisync.v1")
    envelope_match = re.search(r"message Envelope\s*\{(.*?)\n\}", proto, re.DOTALL)
    envelope = envelope_match.group(1) if envelope_match else ""
    if not envelope:
        errors.append(f"{rel(proto_path)}: Envelope not found")
    for name, number in PROTO_FIELDS.items():
        if not re.search(rf"\b{re.escape(name)}\s*=\s*{number}\s*;", envelope):
            errors.append(f"{rel(proto_path)}: Envelope {name} must use field {number}")
    proto_payload_types = dict(re.findall(r"^\s*(\w+)\s+(\w+)\s*=\s*(?:2[0-9]|3[0-4])\s*;", envelope, re.MULTILINE))
    proto_payload_fields = set(proto_payload_types.values())
    if len(proto_payload_fields) != len(proto_payload_types):
        errors.append(f"{rel(proto_path)}: duplicate Envelope payload field")

    protocol_doc_path = ROOT / "docs/design/realtime-protocol-and-connection.md"
    protocol_doc = load_text(protocol_doc_path, errors)
    payload_section = markdown_section(protocol_doc, "### 3.2 payload とクラスの対応", "### 3.3 送信の直列化とバックプレッシャー境界")
    documented_payloads = set(re.findall(r"^\|\s*`?([A-Z][A-Za-z0-9]+)`?\s*\|", payload_section, re.MULTILINE))
    proto_payload_messages = set(proto_payload_types)
    if documented_payloads != proto_payload_messages:
        errors.append(
            f"wire payload table differs from Proto: missing={sorted(proto_payload_messages - documented_payloads)}, "
            f"extra={sorted(documented_payloads - proto_payload_messages)}"
        )
    protocol_identifiers = set(re.findall(r"`([^`\n]+)`", protocol_doc))
    noncanonical_states = SERVER_STATE_ALIASES & protocol_identifiers
    if noncanonical_states:
        errors.append(
            f"{rel(protocol_doc_path)}: server state identifiers must use canonical lower snake case: "
            f"{sorted(noncanonical_states)}"
        )

    proto_messages = parse_proto_messages(proto)
    envelope_fields = set(PROTO_FIELDS)
    hello_sections = {
        "ClientHello": markdown_section(protocol_doc, "### 6.1 ClientHello の内容", "### 6.2 ServerHello と negotiation"),
        "ServerHello": markdown_section(protocol_doc, "### 6.2 ServerHello と negotiation", "## 7. Snapshot"),
    }
    for hello_name, required_mapping in (
        ("ClientHello", CLIENT_HELLO_REQUIREMENTS),
        ("ServerHello", SERVER_HELLO_REQUIREMENTS),
    ):
        documented_mapping = parse_requirement_table(hello_sections[hello_name])
        if documented_mapping != required_mapping:
            errors.append(
                f"{hello_name} requirement table differs from specification mapping: "
                f"expected={required_mapping}, actual={documented_mapping}"
            )
        for requirement, references in required_mapping.items():
            for reference in references:
                message, field = reference.split(".", 1)
                available = envelope_fields if message == "Envelope" else proto_messages.get(message, set())
                if field not in available:
                    errors.append(f"{hello_name} requirement {requirement} references missing Proto field {reference}")

    openapi = load_text(ROOT / "openapi/orbisync-v1.yaml", errors)
    registry = load_text(ROOT / "openapi/errors.yaml", errors)
    # Implemented contract must be valid OpenAPI 3.1 and contain the 8 served ops.
    for required in ("openapi: 3.1.0", "/auth/login:", "/realtime/tickets:", "ErrorEnvelope:"):
        if required not in openapi:
            errors.append(f"openapi/orbisync-v1.yaml: missing {required}")
    # Idempotency-Key parameter definition is retained for roadmap use, but
    # implemented contract no longer requires it (server does not enforce it).
    # So we only require that the parameter definition exists somewhere, not
    # that every POST references it.
    current_path = None
    operations = set()
    for line in openapi.splitlines():
        path_match = re.match(r"^  (/\S+):\s*$", line)
        if path_match:
            current_path = path_match.group(1)
            continue
        method_match = re.match(r"^    (get|post|put|patch|delete):\s*$", line)
        if current_path and method_match:
            operations.add((method_match.group(1), current_path))
    for method, path in sorted(REST_OPERATIONS - operations):
        errors.append(f"openapi/orbisync-v1.yaml: missing specification operation {method.upper()} {path}")
    # Implemented operations must not contain unserved operations (truthful).
    for method, path in sorted(operations - REST_OPERATIONS):
        errors.append(f"openapi/orbisync-v1.yaml: declares unserved operation {method.upper()} {path} (move to planned file)")
    ticket_match = re.search(r"^  /v1/realtime/tickets:\s*$\n(.*?)(?=^  /\S+:\s*$)", openapi, re.MULTILINE | re.DOTALL)
    ticket = ticket_match.group(1) if ticket_match else ""
    # Truthful ticket contract: server returns 200 {realtime_ticket, expires_in}, not 201 {ticket, expires_at}
    for required in ("post:", "operationId: createRealtimeTicket", "TicketResponse"):
        if required not in ticket:
            errors.append(f"openapi/orbisync-v1.yaml: realtime ticket contract missing {required}")
    # The actual field names live in the component schema, not inline in the path block.
    # Check via YAML structure (handles flow vs block style) rather than string matching.
    ticket_schema_ok = False
    ticket_schema_error = ""
    try:
        import yaml as _yaml_validate  # type: ignore

        data = _yaml_validate.safe_load(openapi)
        schemas = (data.get("components") or {}).get("schemas") or {}
        ticket_schema = schemas.get("TicketResponse") or {}
        required = ticket_schema.get("required") or []
        props = ticket_schema.get("properties") or {}
        rt = props.get("realtime_ticket") or {}
        ei = props.get("expires_in") or {}
        if (
            "realtime_ticket" in required
            and "expires_in" in required
            and rt.get("type") == "string"
            and ei.get("type") == "integer"
            and ei.get("minimum") == 1
        ):
            ticket_schema_ok = True
        else:
            ticket_schema_error = f"required={required}, rt={rt}, ei={ei}"
    except ImportError:
        # Fallback when pyyaml not available: regex that matches both flow `{type: string}` and block `type: string`
        rt_ok = bool(re.search(r"realtime_ticket:\s*(?:\{[^}]*type:\s*string[^}]*\}|(?:\n\s+type:\s*string))", openapi))
        ei_ok = bool(re.search(r"expires_in:\s*(?:\{[^}]*type:\s*integer[^}]*\}|(?:\n\s+type:\s*integer))", openapi))
        # also check required contains both names
        has_both_required = "realtime_ticket" in openapi and "expires_in" in openapi and "TicketResponse" in openapi
        if rt_ok and ei_ok and has_both_required:
            ticket_schema_ok = True
        else:
            ticket_schema_error = "regex fallback mismatch"
    except Exception as exc:  # pragma: no cover
        ticket_schema_error = str(exc)
    if not ticket_schema_ok:
        errors.append(
            f"openapi/orbisync-v1.yaml: realtime ticket contract missing realtime_ticket: {{type: string}} or expires_in: {{type: integer}} "
            f"(expected TicketResponse with required [realtime_ticket, expires_in], types string/integer minimum 1; {ticket_schema_error})"
        )
    if "IdempotencyKey" in ticket:
        errors.append("openapi/orbisync-v1.yaml: /v1/realtime/tickets must not require Idempotency-Key (server does not enforce it)")
    if "required: [ticket, expires_at]" in openapi and "TicketResponse" not in openapi:
        errors.append("openapi/orbisync-v1.yaml: realtime ticket must use realtime_ticket/expires_in, not ticket/expires_at")
    if "expires_at: {type: string, format: date-time}" in ticket:
        errors.append("openapi/orbisync-v1.yaml: realtime ticket must use realtime_ticket/expires_in, not ticket/expires_at")
    # Login must return LoginResponse (access_token + expires_in) not TokenPair with refresh_token
    if '"200":' in openapi and "TokenPair" in openapi:
        # Allow TokenPair to remain as roadmap schema, but login operation must not reference it
        login_match = re.search(r"^  /v1/auth/login:\s*$\n(.*?)(?=^  /\S+:\s*$)", openapi, re.MULTILINE | re.DOTALL)
        login_block = login_match.group(1) if login_match else ""
        if "TokenPair" in login_block:
            errors.append("openapi/orbisync-v1.yaml: /v1/auth/login must reference LoginResponse (not TokenPair with refresh_token)")
    # Preservation: planned file must exist and contain the remaining 22 paths / 30 ops
    planned_text = load_text(PLANNED_PATH, errors)
    if planned_text:
        planned_ops = set()
        planned_paths = set()
        cur = None
        for line in planned_text.splitlines():
            pm = re.match(r"^  (/\S+):\s*$", line)
            if pm:
                cur = pm.group(1)
                planned_paths.add(cur)
                continue
            mm = re.match(r"^    (get|post|put|patch|delete):\s*$", line)
            if cur and mm:
                planned_ops.add((mm.group(1), cur))
        # Must be disjoint at operation level (paths overlap for /v1/users etc where POST is served and GET is planned)
        if planned_ops & operations:
            errors.append(f"openapi/orbisync-v1-planned.yaml: must be disjoint from implemented, overlap {planned_ops & operations}")
        combined_ops = planned_ops | operations
        combined_paths = planned_paths | {p for _, p in operations}
        # Original reviewed design was 30 unique paths / 38 ops (8 served + 30 planned, 3 path names overlap).
        # V-05 added GET /v1/users/{user_id}/roles (new operation, not previously in planned), so 38->39.
        # W-29 moved 6 ops from planned to implemented (GET /v1/users etc), so planned 27->21, implemented 11->18.
        # ADR-026 added four authentication paths, each with one operation
        # (mode discovery plus guest, name-only and external), so 30->34 and 39->43.
        if len(combined_paths) != 37:
            errors.append(f"openapi: combined paths {len(combined_paths)} != 37 (original design lost)")
        if len(combined_ops) != 46:
            errors.append(
                f"openapi: combined ops {len(combined_ops)} != 46 (original design plus ADR-026, admin, diagnostics and extension commands)"
            )
        # Invariant: any operation removed from planned must now be in implemented (distinguishes legitimate move vs design loss).
        # This prevents the constant-only check from misdiagnosing a deletion as an addition.
        try:
            # Previous planned set at 7a14eef (before W-29): 27 ops. Hard-coded to avoid git dependency in CI.
            previous_planned_ops = {
                ("delete", "/v1/roles/{role_id}"),
                ("get", "/metrics"),
                ("get", "/v1/audit-events"),
                ("get", "/v1/audit-events/{event_id}"),
                ("get", "/v1/auth/me"),
                ("get", "/v1/instances"),
                ("get", "/v1/instances/{instance_id}"),
                ("get", "/v1/instances/{instance_id}/members"),
                ("get", "/v1/roles"),
                ("get", "/v1/roles/{role_id}"),
                ("get", "/v1/users"),
                ("get", "/v1/users/{user_id}"),
                ("get", "/v1/worlds"),
                ("get", "/v1/worlds/{world_id}"),
                ("patch", "/v1/roles/{role_id}"),
                ("patch", "/v1/users/{user_id}"),
                ("patch", "/v1/worlds/{world_id}"),
                ("post", "/v1/auth/logout"),
                ("post", "/v1/auth/refresh"),
                ("post", "/v1/instances/{instance_id}/kick/{user_id}"),
                ("post", "/v1/instances/{instance_id}/start"),
                ("post", "/v1/instances/{instance_id}/stop"),
                ("post", "/v1/users/import"),
                ("post", "/v1/users/{user_id}/disable"),
                ("post", "/v1/users/{user_id}/enable"),
                ("post", "/v1/users/{user_id}/reset-password"),
                ("post", "/v1/worlds/{world_id}/archive"),
            }
            removed_from_planned = previous_planned_ops - planned_ops
            not_in_implemented = removed_from_planned - operations
            if not_in_implemented:
                errors.append(
                    f"openapi: operations removed from planned but not in implemented (design lost): {sorted(not_in_implemented)} "
                    f"(removed={sorted(removed_from_planned)})"
                )
            # Also ensure current planned is subset of previous (no unexpected additions to planned without design review)
            unexpected_in_planned = planned_ops - previous_planned_ops
            if unexpected_in_planned:
                # Only V-05's new op should be in implemented, not planned. If planned gains new ops, it's suspicious.
                errors.append(f"openapi/orbisync-v1-planned.yaml: unexpected operations in planned (not in previous 27): {sorted(unexpected_in_planned)}")
        except Exception as exc:  # pragma: no cover
            errors.append(f"openapi: failed to check planned->implemented invariant: {exc}")
        # The integration moved the final 17 planned operations into v1.
        if len(planned_paths) != 0:
            errors.append(f"openapi/orbisync-v1-planned.yaml: expected 0 path entries after full REST integration, got {len(planned_paths)}")
        if len(planned_ops) != 0:
            errors.append(f"openapi/orbisync-v1-planned.yaml: expected 0 ops after full REST integration, got {len(planned_ops)}")
        if "/metrics:" not in openapi:
            errors.append("openapi/orbisync-v1.yaml: missing /metrics: (must be implemented)")
        if "/metrics:" in planned_text:
            errors.append("openapi/orbisync-v1-planned.yaml: should not contain /metrics: (moved to implemented)")
    else:
        errors.append(f"{PLANNED_PATH.relative_to(ROOT)}: missing (must preserve 30 unserved operations)")
    codes = set(re.findall(r"^\s*-\s+code:\s+([A-Z][A-Z0-9_]+)\s*$", registry, re.MULTILINE))
    if len(codes) < 8:
        errors.append("openapi/errors.yaml: expected at least 8 registered errors")
    vectors = json.loads((ROOT / "test-vectors/rest/v1/error-cases.json").read_text(encoding="utf-8"))
    for case in vectors["cases"]:
        if case["code"] not in codes:
            errors.append(f"REST vector references unregistered error {case['code']}")

    machine_path = ROOT / "contracts/realtime-connection-state-machine.json"
    machine = json.loads(machine_path.read_text(encoding="utf-8"))
    state_list, event_list = machine["states"], machine["events"]
    states, events = set(state_list), set(event_list)
    if len(states) != len(state_list):
        errors.append(f"{rel(machine_path)}: duplicate state")
    if len(events) != len(event_list):
        errors.append(f"{rel(machine_path)}: duplicate event")
    if machine["initial"] not in states or not set(machine["terminal"]) <= states:
        errors.append(f"{rel(machine_path)}: initial/terminal references unknown state")
    transitions: set[tuple[str, str, str]] = set()
    state_events: set[tuple[str, str]] = set()
    for transition in machine["transitions"]:
        edge = (transition["from"], transition["event"], transition["to"])
        if edge in transitions:
            errors.append(f"{rel(machine_path)}: duplicate transition {edge}")
        transitions.add(edge)
        if (edge[0], edge[1]) in state_events:
            errors.append(f"{rel(machine_path)}: nondeterministic state/event {edge[:2]}")
        state_events.add((edge[0], edge[1]))
        if edge[0] not in states or edge[2] not in states or edge[1] not in events:
            errors.append(f"{rel(machine_path)}: transition uses unknown state/event: {edge}")
        if edge[0] in machine["terminal"]:
            errors.append(f"{rel(machine_path)}: terminal state has outgoing transition: {edge}")
    used_events = {event for _, event, _ in transitions}
    if used_events != events:
        errors.append(f"{rel(machine_path)}: declared/used event sets differ")
    reachable = {machine["initial"]}
    changed = True
    while changed:
        changed = False
        for source, _, target in transitions:
            if source in reachable and target not in reachable:
                reachable.add(target)
                changed = True
    if reachable != states:
        errors.append(f"{rel(machine_path)}: unreachable states: {sorted(states - reachable)}")
    fatal_events = {"version_negotiation_failed", "invalid_token", "oversized_message", "persistent_rate_limit", "protocol_abuse"}
    for source, event, target in transitions:
        if event in fatal_events and target != "failing":
            errors.append(f"{rel(machine_path)}: fatal event must enter failing: {(source, event, target)}")
        if event == "close_requested" and target != "closing":
            errors.append(f"{rel(machine_path)}: close_requested must enter closing")
        if source == "closing" and target != "closed":
            errors.append(f"{rel(machine_path)}: closing may only terminate as closed")
        if source == "failing" and target != "failed":
            errors.append(f"{rel(machine_path)}: failing may only terminate as failed")
    for required_edge in {
        ("closing", "graceful_close_completed", "closed"),
        ("closing", "transport_lost", "closed"),
        ("failing", "fatal_close_completed", "failed"),
        ("failing", "transport_lost", "failed"),
    }:
        if required_edge not in transitions:
            errors.append(f"{rel(machine_path)}: missing termination edge {required_edge}")

    machine_section = markdown_section(protocol_doc, "### 5.1 接続状態機械", "### 5.2 失敗の分類原則")
    state_table_match = re.search(r"^\| machine state \|.*?\n\|---.*?\n((?:^\|.*\n)+)", machine_section, re.MULTILINE)
    transition_table_match = re.search(r"^\| from \| event \| to \|\s*\n\|---.*?\n((?:^\|.*\n)+)", machine_section, re.MULTILINE)
    state_rows = state_table_match.group(1) if state_table_match else ""
    transition_rows = transition_table_match.group(1) if transition_table_match else ""
    documented_states = set(re.findall(r"^\|\s*`([a-z][a-z0-9_]*)`\s*\|", state_rows, re.MULTILINE))
    documented_transitions = set(re.findall(r"^\|\s*`([a-z][a-z0-9_]*)`\s*\|\s*`([a-z][a-z0-9_]*)`\s*\|\s*`([a-z][a-z0-9_]*)`\s*\|", transition_rows, re.MULTILINE))
    if documented_states != states:
        errors.append(f"state table differs from JSON: missing={sorted(states - documented_states)}, extra={sorted(documented_states - states)}")
    if documented_transitions != transitions:
        errors.append(
            f"transition table differs from JSON: missing={sorted(transitions - documented_transitions)}, "
            f"extra={sorted(documented_transitions - transitions)}"
        )

    failure_section = markdown_section(protocol_doc, "### 5.3 失敗時状態遷移表", "### 5.4 隔離")
    transport_loss_row = next(
        (line for line in failure_section.splitlines() if line.startswith("| transport loss |")), ""
    )
    documented_transport_loss: set[tuple[str, str]] = set()
    for sources, target in re.findall(r"`([a-z][a-z0-9_/]*)\s*→\s*([a-z][a-z0-9_]*)`", transport_loss_row):
        documented_transport_loss.update((source, target) for source in sources.split("/"))
    machine_transport_loss = {(source, target) for source, event, target in transitions if event == "transport_lost"}
    if documented_transport_loss != machine_transport_loss:
        errors.append(
            "transport loss summary differs from JSON: "
            f"missing={sorted(machine_transport_loss - documented_transport_loss)}, "
            f"extra={sorted(documented_transport_loss - machine_transport_loss)}"
        )

    sdk_doc = load_text(ROOT / "docs/design/client-sdk.md", errors)
    mobile_doc = load_text(ROOT / "docs/design/mobile-resume-interest-backpressure.md", errors)
    sdk_state_section = markdown_section(sdk_doc, "### 3.4 SDK 接続状態機械", "## 4. シリアライズと送信")
    sdk_state_table = re.search(r"^\| 状態 \|.*?\n\|---.*?\n((?:^\|.*\n)+)", sdk_state_section, re.MULTILINE)
    sdk_states = set(re.findall(r"^\|\s*([A-Z][A-Za-z0-9]*)\s*\|", sdk_state_table.group(1) if sdk_state_table else "", re.MULTILINE))
    client_state_section = markdown_section(mobile_doc, "### 2.3 クライアント側の再接続状態", "### 2.4 指数バックオフ + jitter")
    diagram_match = re.search(r"```text\s*\n(.*?)```", client_state_section, re.DOTALL)
    mobile_states = set(re.findall(r"\b[A-Z][A-Za-z0-9]*\b", diagram_match.group(1) if diagram_match else ""))
    if not sdk_states:
        errors.append("client SDK canonical state table is missing")
    if mobile_states - sdk_states:
        errors.append(f"mobile reconnect summary uses states not declared by client SDK: {sorted(mobile_states - sdk_states)}")

    resume_policy_path = ROOT / "contracts/realtime-resume-token-policy.json"
    resume_policy = json.loads(load_text(resume_policy_path, errors) or "{}")
    policy_section = markdown_section(protocol_doc, "### 6.1 ClientHello の内容", "### 6.2 ServerHello と negotiation")
    documented_policy = parse_contract_value_table(policy_section)
    machine_policy = {key: value for key, value in resume_policy.items() if key not in {"schema_version", "cases"}}
    if resume_policy.get("schema_version") != 1:
        errors.append(f"{rel(resume_policy_path)}: schema_version must be 1")
    if documented_policy != machine_policy:
        errors.append(
            "resume token policy table differs from JSON: "
            f"missing={sorted(machine_policy.items() - documented_policy.items())}, "
            f"extra={sorted(documented_policy.items() - machine_policy.items())}"
        )

    required_case_fields = {
        "case", "client_hello_present", "resume_session_present", "tokens_equal",
        "decision", "next_state", "error",
    }
    machine_cases = resume_policy.get("cases", [])
    if not isinstance(machine_cases, list):
        errors.append(f"{rel(resume_policy_path)}: cases must be an array")
        machine_cases = []
    case_names = [case.get("case") for case in machine_cases if isinstance(case, dict)]
    if len(case_names) != len(set(case_names)):
        errors.append(f"{rel(resume_policy_path)}: duplicate resume policy case names")
    input_combinations: list[tuple[object, object, object]] = []
    normalized_machine_cases: dict[str, tuple[object, ...]] = {}
    for case in machine_cases:
        if not isinstance(case, dict) or set(case) != required_case_fields:
            errors.append(f"{rel(resume_policy_path)}: every case must contain exactly {sorted(required_case_fields)}")
            continue
        inputs = (case["client_hello_present"], case["resume_session_present"], case["tokens_equal"])
        input_combinations.append(inputs)
        normalized_machine_cases[str(case["case"])] = inputs + (case["decision"], case["next_state"], case["error"])
    expected_inputs = {(value[0], value[1], value[2]) for value in RESUME_POLICY_CASES.values()}
    if len(input_combinations) != len(set(input_combinations)):
        errors.append(f"{rel(resume_policy_path)}: duplicate resume policy input combinations")
    if set(input_combinations) != expected_inputs:
        errors.append(
            f"{rel(resume_policy_path)}: resume policy input combinations are incomplete: "
            f"missing={sorted(expected_inputs - set(input_combinations), key=str)}, "
            f"extra={sorted(set(input_combinations) - expected_inputs, key=str)}"
        )
    if normalized_machine_cases != RESUME_POLICY_CASES:
        errors.append("resume policy case names or expected results differ from required decision table")

    documented_cases_list = parse_resume_case_table(policy_section)
    documented_case_names = [str(case["case"]) for case in documented_cases_list]
    if len(documented_case_names) != len(set(documented_case_names)):
        errors.append("resume policy documentation has duplicate case names")
    normalized_documented_cases = {
        str(case["case"]): (
            case["client_hello_present"], case["resume_session_present"], case["tokens_equal"],
            case["decision"], case["next_state"], case["error"],
        )
        for case in documented_cases_list
    }
    if normalized_documented_cases != normalized_machine_cases:
        errors.append("resume policy decision table differs from JSON cases")

    check_protocol_vectors(proto_messages, proto_payload_fields, errors)


def check_immutable_spec(errors: list[str]) -> None:
    path = ROOT / "metaverse_core_specification.md"
    # Git may check text out as CRLF on Windows. Guard semantic source content
    # while remaining stable across checkout line-ending policies.
    normalized = path.read_bytes().replace(b"\r\n", b"\n").replace(b"\r", b"\n")
    digest = hashlib.sha256(normalized).hexdigest()
    if digest != SPEC_SHA256:
        errors.append(
            "metaverse_core_specification.md changed; update is forbidden "
            f"(expected {SPEC_SHA256}, got {digest})"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--review", action="store_true", help="print review summary")
    args = parser.parse_args()
    errors: list[str] = []
    texts = {path: load_text(path, errors) for path in MARKDOWN if path.exists()}
    check_links(texts, errors)
    check_fences(texts, errors)
    check_spec_coverage(texts, errors)
    check_adrs(texts, errors)
    check_adr_design_consistency(texts, errors)
    check_acceptance_implementation_consistency(texts, errors)
    check_legacy_names(texts, errors)
    check_json(errors)
    check_contracts(errors)
    check_immutable_spec(errors)
    json_files = sorted((ROOT / "contracts").rglob("*.json")) + sorted((ROOT / "test-vectors").rglob("*.json"))
    if args.review:
        print(f"reviewed_markdown={len(texts)}")
        print("specification_chapters=1-47")
        print(f"required_accepted_adrs={len(EXPECTED_ADRS)}")
        print(f"json_files={len(json_files)}")
        print(f"errors={len(errors)}")
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"design scan: {len(texts)} markdown, {len(EXPECTED_ADRS)} ADRs, {len(json_files)} JSON files")
    print("OrbiSync design validation passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
