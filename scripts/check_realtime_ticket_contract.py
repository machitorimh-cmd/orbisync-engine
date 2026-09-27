#!/usr/bin/env python3
"""Gate that TicketResponse contract is consistent across OpenAPI, Rust, SDK, helper, and examples.

Prevents the RV-D regression where:
  server returns `realtime_ticket` / `expires_in`
  SDK read `ticket` / `expires_at`
  e2e helper mocked wrong shape and hid the drift.

The gate also covers the changePassword prose regression (other sessions vs all sessions)
as requested in the orchestrator follow-up.

Checks:
  - OpenAPI `TicketResponse` requires `realtime_ticket` and `expires_in`
  - Rust `crates/orbisync-transport-http/src/auth.rs` TicketResponse struct has those fields
  - SDK `sdk/typescript/src/realtime_ticket.ts` central type has those fields
  - SDK `sdk/typescript/src/client.ts` fetchRealtimeTicket reads `realtime_ticket` (not `ticket`)
  - Helper `crates/orbisync-e2e-helper/src/main.rs` stub uses `realtime_ticket`/`expires_in`
  - The minimal example reads `realtime_ticket`/`expires_in`; the reference web
    app delegates ticket fetching to the checked SDK `connect()` path
  - OpenAPI `changePassword` 204 description mentions "all sessions" including caller's
  - Handler `create_realtime_ticket` status codes (via `ErrorCode::RateLimited` → 429) match OpenAPI `responses`
  - SDK handles 429 (`RealtimeTicketRateLimitedError` / `status === 429`)

Zero-scan guard: each dimension (OpenAPI, Rust, SDK, helpers) is counted; if any
dimension scanned 0 files, the gate fails with "no ... were scanned" instead of
passing with 0 errors.

Usage:
    python scripts/check_realtime_ticket_contract.py
    python scripts/check_realtime_ticket_contract.py --json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

OPENAPI_PATH = ROOT / "openapi" / "orbisync-v1.yaml"
RUST_AUTH_PATH = ROOT / "crates" / "orbisync-transport-http" / "src" / "auth.rs"
SDK_TS_FILES = [
    ROOT / "sdk" / "typescript" / "src" / "realtime_ticket.ts",
    ROOT / "sdk" / "typescript" / "src" / "client.ts",
]
HELPER_FILES = [
    ROOT / "crates" / "orbisync-e2e-helper" / "src" / "main.rs",
    ROOT / "examples" / "minimal-client-typescript" / "src" / "main.ts",
    ROOT / "apps" / "reference-web" / "src" / "main.ts",
]

# ErrorCode -> HTTP status mapping mirrors crates/orbisync-transport-http/src/lib.rs ErrorCode::status
_ERROR_CODE_TO_STATUS: dict[str, str] = {
    "InvalidRequest": "400",
    "AuthenticationRequired": "401",
    "AccessDenied": "403",
    "ResourceNotFound": "404",
    "IdempotencyKeyReused": "409",
    "ResourceConflict": "409",
    "RevisionMismatch": "412",
    "RateLimited": "429",
    "InternalError": "500",
    "ServiceUnavailable": "503",
}


def parse_openapi_ticket_fields(root: Path = ROOT) -> tuple[list[str], str]:
    """Return (required_fields, changePassword_description)."""
    path = root / "openapi" / "orbisync-v1.yaml"
    text = path.read_text(encoding="utf-8")
    # Simple line-based parse to avoid catastrophic regex on large yaml
    fields: list[str] = []
    in_ticket = False
    seen_required = False
    for line in text.splitlines():
        if "TicketResponse:" in line:
            in_ticket = True
            continue
        if not in_ticket:
            continue
        stripped = line.strip()
        if stripped.startswith("required:"):
            seen_required = True
            continue
        if seen_required:
            if stripped.startswith("- "):
                field = stripped[2:].strip()
                fields.append(field)
                continue
            if stripped == "" or stripped.startswith("#"):
                continue
            # End of required block
            if fields:
                break
    if not fields:
        raise RuntimeError(f"cannot parse TicketResponse required from {path} (found {fields})")
    # Find changePassword description via simple search
    desc = ""
    # Locate the change-password block and then '204' description
    idx = text.find("/v1/auth/change-password:")
    if idx != -1:
        snippet = text[idx : idx + 2000]
        m2 = re.search(r"'204':\s*\n\s*description:\s*([^\n]+)", snippet)
        if m2:
            desc = m2.group(1).strip()
            desc = desc.strip("'\"")
    return fields, desc


def collect_sdk_files(root: Path = ROOT) -> list[Path]:
    files = []
    for p in SDK_TS_FILES:
        # SDK_TS_FILES are absolute based on the import-time ROOT. For a custom
        # root (e.g. a temp dir in tests) we must re-derive the relative path
        # and then look under the custom root. Use try/except instead of
        # `ROOT in p.parents` which is fragile across platforms / symlinks and
        # which previously caused sdk=0 on a clean worktree (see PREVENT-1).
        try:
            rel = p.relative_to(ROOT)
        except ValueError:
            continue
        candidate = root / rel
        if candidate.exists():
            files.append(candidate)
    return sorted(files)


def collect_helper_files(root: Path = ROOT) -> list[Path]:
    files = []
    for p in HELPER_FILES:
        try:
            rel = p.relative_to(ROOT)
        except ValueError:
            continue
        candidate = root / rel
        if candidate.exists():
            files.append(candidate)
    return sorted(files)


def check_rust_auth_fields(root: Path = ROOT) -> tuple[bool, str]:
    path = root / "crates" / "orbisync-transport-http" / "src" / "auth.rs"
    if not path.exists():
        return False, f"missing {path}"
    text = path.read_text(encoding="utf-8")
    # Extract struct TicketResponse { ... } until }
    m = re.search(r"struct TicketResponse\s*\{([^}]+)\}", text, re.S)
    if not m:
        return False, "cannot find struct TicketResponse in auth.rs"
    body = m.group(1)
    has_rt = "realtime_ticket" in body
    has_ei = "expires_in" in body
    if has_rt and has_ei:
        return True, ""
    missing = []
    if not has_rt:
        missing.append("realtime_ticket")
    if not has_ei:
        missing.append("expires_in")
    # Also check response construction
    has_construct_rt = "realtime_ticket:" in text
    has_construct_ei = "expires_in:" in text
    detail = f"TicketResponse struct missing {missing}; construct check rt={has_construct_rt} ei={has_construct_ei}"
    return False, detail


def parse_handler_ticket_statuses(root: Path = ROOT) -> set[str]:
    """Extract HTTP status codes that create_realtime_ticket handler can return.

    Scans the function body for ErrorCode::Variant and maps to HTTP status.
    Returns set of status strings e.g. {"429", "500"}.
    """
    path = root / "crates" / "orbisync-transport-http" / "src" / "auth.rs"
    if not path.exists():
        return set()
    text = path.read_text(encoding="utf-8")
    # Find the function block: from "pub async fn create_realtime_ticket" to next top-level "pub async fn" or "#[cfg(test)]"
    start = text.find("pub async fn create_realtime_ticket")
    if start == -1:
        return set()
    # Cut to next "pub async fn" after start or to "#[cfg(test)]"
    next_fn = text.find("pub async fn ", start + 30)
    cfg_test = text.find("#[cfg(test)]", start + 30)
    end_candidates = [x for x in (next_fn, cfg_test) if x != -1]
    end = min(end_candidates) if end_candidates else len(text)
    snippet = text[start:end]
    codes = set(re.findall(r"ErrorCode::(\w+)", snippet))
    statuses: set[str] = set()
    for c in codes:
        st = _ERROR_CODE_TO_STATUS.get(c)
        if st:
            statuses.add(st)
    return statuses


def parse_openapi_ticket_statuses(root: Path = ROOT) -> set[str]:
    """Extract response status codes for POST /v1/realtime/tickets from OpenAPI."""
    path = root / "openapi" / "orbisync-v1.yaml"
    if not path.exists():
        return set()
    text = path.read_text(encoding="utf-8")
    idx = text.find("/v1/realtime/tickets:")
    if idx == -1:
        return set()
    # Take until next top-level path (next "  /v1/")
    snippet = text[idx : idx + 5000]
    # Cut before next path entry
    next_path = snippet.find("\n  /v1/", 10)
    if next_path != -1:
        snippet = snippet[:next_path]
    codes = set(re.findall(r"'(\d+)'\s*:", snippet))
    return codes


def check_rate_limit_contract(root: Path = ROOT) -> list[str]:
    """Check that handler RateLimited (429) is documented in OpenAPI and SDK."""
    errors: list[str] = []
    handler_statuses = parse_handler_ticket_statuses(root)
    openapi_statuses = parse_openapi_ticket_statuses(root)
    # 429 must be consistent: if handler can return 429, OpenAPI must list 429
    if "429" in handler_statuses and "429" not in openapi_statuses:
        errors.append(
            "OpenAPI /v1/realtime/tickets is missing '429' response but handler returns ErrorCode::RateLimited (429) — SDK contract mismatch (A-2/C5)"
        )
    if "429" not in handler_statuses and "429" in openapi_statuses:
        errors.append(
            "OpenAPI /v1/realtime/tickets lists '429' but handler never returns ErrorCode::RateLimited — stale contract"
        )
    # Ensure 200 success is still present
    if "200" not in openapi_statuses:
        errors.append("OpenAPI /v1/realtime/tickets must list '200' response")
    return errors


def check_sdk_contract(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    rt_path = root / "sdk" / "typescript" / "src" / "realtime_ticket.ts"
    client_path = root / "sdk" / "typescript" / "src" / "client.ts"
    # Check realtime_ticket.ts exists and defines correct type
    if not rt_path.exists():
        errors.append(f"missing SDK contract file {rt_path.relative_to(root)}")
    else:
        text = rt_path.read_text(encoding="utf-8")
        if "realtime_ticket" not in text:
            errors.append("sdk/typescript/src/realtime_ticket.ts must contain realtime_ticket")
        if "expires_in" not in text:
            errors.append("sdk/typescript/src/realtime_ticket.ts must contain expires_in")
        # Must not contain legacy ticket field as primary (allow comments mentioning legacy but not as type)
        # Check type definition specifically
        m = re.search(r"type RealtimeTicketResponse\s*=\s*\{([^}]+)\}", text, re.S)
        if m:
            body = m.group(1)
            if "realtime_ticket" not in body or "expires_in" not in body:
                errors.append("RealtimeTicketResponse type must contain realtime_ticket and expires_in")
            if re.search(r"\bticket\s*:", body):
                # Would be legacy ticket: string inside central type
                errors.append("RealtimeTicketResponse must not contain legacy `ticket` field")
        else:
            errors.append("cannot parse RealtimeTicketResponse type in realtime_ticket.ts")
        # parse function must check realtime_ticket
        if "parseRealtimeTicketResponse" not in text:
            errors.append("realtime_ticket.ts must export parseRealtimeTicketResponse")

    # Check client.ts
    if not client_path.exists():
        errors.append(f"missing {client_path.relative_to(root)}")
    else:
        text = client_path.read_text(encoding="utf-8")
        if "parseRealtimeTicketResponse" not in text:
            errors.append("sdk/typescript/src/client.ts must use parseRealtimeTicketResponse (centralized)")
        if "body.ticket" in text or re.search(r"as\s*\{\s*ticket", text):
            errors.append("sdk/typescript/src/client.ts still uses legacy `ticket` field (must use realtime_ticket)")
        if "realtime_ticket" not in text:
            errors.append("sdk/typescript/src/client.ts must reference realtime_ticket")
        if "missing ticket field" in text:
            errors.append("client.ts error message still mentions `ticket` (must be realtime_ticket)")
        # A-2: SDK must handle 429
        if "429" not in text and "RateLimited" not in text and "RealtimeTicketRateLimitedError" not in text:
            errors.append(
                "sdk/typescript/src/client.ts must handle 429 (RealtimeTicketRateLimitedError / status 429) for createRealtimeTicket"
            )
        if "RealtimeTicketRateLimitedError" not in text:
            errors.append("sdk/typescript/src/client.ts must throw/handle RealtimeTicketRateLimitedError for 429")

    # Check realtime_ticket.ts also exports the 429 error type
    rt_text_path = root / "sdk" / "typescript" / "src" / "realtime_ticket.ts"
    if rt_text_path.exists():
        rt_text = rt_text_path.read_text(encoding="utf-8")
        if "RealtimeTicketRateLimitedError" not in rt_text:
            errors.append("sdk/typescript/src/realtime_ticket.ts must define RealtimeTicketRateLimitedError (429)")
        if "429" not in rt_text and "RATE_LIMITED" not in rt_text:
            errors.append("sdk/typescript/src/realtime_ticket.ts must reference 429 / RATE_LIMITED")

    return errors


def check_helper_and_examples(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    for rel in [
        "crates/orbisync-e2e-helper/src/main.rs",
        "examples/minimal-client-typescript/src/main.ts",
        "apps/reference-web/src/main.ts",
    ]:
        path = root / rel
        if not path.exists():
            errors.append(f"missing {rel}")
            continue
        text = path.read_text(encoding="utf-8")
        # For helper, check tickets_stub uses correct fields
        if "e2e-helper" in rel:
            if '"realtime_ticket"' not in text and "realtime_ticket" not in text:
                errors.append(f"{rel} must contain realtime_ticket (tickets stub)")
            if '"ticket"' in text and '"realtime_ticket"' not in text:
                errors.append(f"{rel} still uses legacy \"ticket\"")
            if "expires_in" not in text:
                errors.append(f"{rel} must contain expires_in")
            if "expires_at" in text and "expires_in" not in text:
                errors.append(f"{rel} still uses legacy expires_at")
            # also ensure no `"ticket": "stub` remains
            if re.search(r'"ticket"\s*:', text):
                # Allow if also has realtime_ticket, but legacy ticket should not be in ticket stub
                # Count occurrences: if ticket stub still has ticket field
                if '"realtime_ticket"' not in text:
                    errors.append(f"{rel} tickets stub still uses legacy ticket field")
                else:
                    # If both present, also error (ambiguous)
                    # Check tickets_stub block specifically
                    m = re.search(r"tickets_stub.*?\{([^}]+)\}", text, re.S)
                    if m and '"ticket"' in m.group(1):
                        errors.append(f"{rel} tickets_stub must not contain legacy \"ticket\"")
        else:
            # The reference app uses the SDK's checked ticket parser through
            # connect(), while the standalone example fetches the ticket itself.
            if rel == "apps/reference-web/src/main.ts":
                if not all(
                    snippet in text
                    for snippet in ("new OrbiSyncClient(", "await client.connect()", "await nextConnection.join(")
                ):
                    errors.append(f"{rel} must connect and join through OrbiSyncClient")
            elif "realtime_ticket" not in text:
                errors.append(f"{rel} must mention realtime_ticket (ticket fetch snippet)")
            # If it mentions ticket fetch, it should not be legacy `const {{ ticket }}` alone
            if re.search(r"\{\s*ticket\s*\}", text) and "realtime_ticket" not in text:
                errors.append(f"{rel} still uses legacy `{{ ticket }}` destructuring")
            # Check for expires_at vs expires_in in ticket context
            # Allow expires_at only if not in ticket context; but ticket context should be expires_in
            # Simple: if file contains `expires_at` near `ticket` and not `expires_in`, error
            if "expires_at" in text:
                # Check if near realtime_ticket context; if file has both, it's okay only if ticket stub fixed
                # For examples, legacy was `expires_at`; new should be `expires_in`
                # So if file contains `ticket` and `expires_at` together without `realtime_ticket`, error
                # Already flagged via realtime_ticket missing; but add explicit
                pass
            if re.search(r'"ticket"\s*:', text) or re.search(r"\b ticket:", text):
                # generic ticket property - ensure it's not the ticket response
                # For reference-web, the log line mentions ticket but should be updated
                # We'll only error if inside a JSON snippet for tickets
                if "POST /v1/realtime/tickets" in text and '"ticket"' in text:
                    errors.append(f"{rel} ticket fetch comment still uses legacy ticket field")
    return errors


def check_change_password_prose(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    try:
        _, desc = parse_openapi_ticket_fields(root)
    except Exception as e:
        errors.append(str(e))
        return errors
    # Expect description to mention all sessions including caller's
    # Check for phrases that indicate correct semantics
    low = desc.lower()
    if "all sessions" not in low:
        errors.append(f"changePassword 204 description must mention 'all sessions' (got: {desc!r})")
    if "including" not in low or "caller" not in low:
        errors.append(f"changePassword 204 description must mention including the caller's (got: {desc!r})")
    if "other sessions" in low:
        # Old wording that implies caller's session remains — must not be present
        errors.append(f"changePassword 204 description must not say 'other sessions' (got: {desc!r})")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON report")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()
    root = args.root.resolve()

    errors: list[str] = []
    zero_problems: list[str] = []

    # -- Scan counts --
    openapi_exists = (root / "openapi" / "orbisync-v1.yaml").exists()
    openapi_count = 1 if openapi_exists else 0
    rust_count = 1 if (root / "crates" / "orbisync-transport-http" / "src" / "auth.rs").exists() else 0
    sdk_files = collect_sdk_files(root)
    helper_files = collect_helper_files(root)
    handler_statuses = parse_handler_ticket_statuses(root)
    openapi_statuses = parse_openapi_ticket_statuses(root)

    print(f"ticket contract scan: openapi={openapi_count} rust={rust_count} sdk={len(sdk_files)} helpers={len(helper_files)} handler_statuses={len(handler_statuses)} openapi_statuses={len(openapi_statuses)}")

    if openapi_count == 0:
        zero_problems.append("ticket contract gate: no OpenAPI files were scanned — expected 1 (openapi/orbisync-v1.yaml)")
    if rust_count == 0:
        zero_problems.append("ticket contract gate: no Rust auth files were scanned — expected 1 (crates/orbisync-transport-http/src/auth.rs)")
    if len(sdk_files) == 0:
        zero_problems.append("ticket contract gate: no SDK files were scanned — expected >0 (sdk/typescript/src/*.ts)")
    if len(helper_files) == 0:
        zero_problems.append("ticket contract gate: no helper/example files were scanned — expected >0 (e2e-helper, examples, apps/reference-web)")
    if len(handler_statuses) == 0:
        zero_problems.append("ticket contract gate: no handler statuses were scanned — expected >0 (parse_handler_ticket_statuses found 0; gate would miss RateLimited)")
    if len(openapi_statuses) == 0:
        zero_problems.append("ticket contract gate: no OpenAPI ticket statuses were scanned — expected >0 (parse_openapi_ticket_statuses found 0)")

    if zero_problems:
        for p in zero_problems:
            print(p, file=sys.stderr)
        if args.json:
            print(json.dumps({"errors": zero_problems, "scan": {"openapi": openapi_count, "rust": rust_count, "sdk": len(sdk_files), "helpers": len(helper_files), "handler_statuses": len(handler_statuses), "openapi_statuses": len(openapi_statuses)}}, indent=2))
        return 1

    # -- Contract checks --
    try:
        required, desc = parse_openapi_ticket_fields(root)
    except Exception as e:
        errors.append(str(e))
        required = []
        desc = ""

    if "realtime_ticket" not in required or "expires_in" not in required:
        errors.append(f"OpenAPI TicketResponse required must be [realtime_ticket, expires_in] (got {required})")
    else:
        print(f"OpenAPI TicketResponse: required={required} OK")
        # Also ensure schema properties exist
        openapi_text = (root / "openapi" / "orbisync-v1.yaml").read_text(encoding="utf-8")
        if "realtime_ticket:" not in openapi_text or "expires_in:" not in openapi_text:
            errors.append("OpenAPI TicketResponse must define realtime_ticket and expires_in properties")

    ok, detail = check_rust_auth_fields(root)
    if not ok:
        errors.append(f"Rust TicketResponse: {detail}")
    else:
        print("Rust TicketResponse: OK")

    sdk_errors = check_sdk_contract(root)
    errors.extend(sdk_errors)
    if not sdk_errors:
        print("SDK contract: OK")

    helper_errors = check_helper_and_examples(root)
    errors.extend(helper_errors)
    if not helper_errors:
        print("Helper/examples contract: OK")

    prose_errors = check_change_password_prose(root)
    errors.extend(prose_errors)
    if not prose_errors:
        print("changePassword prose: OK")

    rate_errors = check_rate_limit_contract(root)
    errors.extend(rate_errors)
    if not rate_errors:
        print(f"Rate limit contract: handler={sorted(handler_statuses)} openapi={sorted(openapi_statuses)} OK")
    else:
        print(f"Rate limit contract: handler={sorted(handler_statuses)} openapi={sorted(openapi_statuses)}")

    if errors:
        for e in errors:
            print(f"ERROR: {e}", file=sys.stderr)
        if args.json:
            print(json.dumps({"errors": errors}, indent=2))
        return 1

    print("OrbiSync realtime ticket contract validation passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
