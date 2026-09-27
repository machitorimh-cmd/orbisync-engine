#!/usr/bin/env python3
"""Gate that handler failure is fail-closed for HttpState Option accessors.

C3: `create_realtime_ticket` had a fallback that issued a JWT when
`realtime_ticket_store` was `None` (old behavior survived without `#[cfg(test)]`).
The general defect is:

    let Some(dep) = state.dep() else { <fallback success path> };

For every `HttpState` accessor returning `Option<Arc<...>>`, the `None` arm
must return an error (never a 2xx with a ticket/body).  This gate also bans
any handler-local `issue_realtime_ticket` call – that method is only for
`orbisync-identity::token` unit tests and for the HMAC gate, not for HTTP
handlers after C1.

Zero-scan guards ensure the gate is not green due to broken file selection.

Usage:
    python scripts/check_http_handler_fail_closed.py [--json] [--root PATH]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HTTP_SRC = ROOT / "crates" / "orbisync-transport-http" / "src"
HTTP_LIB = HTTP_SRC / "lib.rs"


def parse_option_accessors(root: Path = ROOT) -> list[str]:
    """Return field names of HttpState that are `Option<Arc<...>>`."""
    path = root / "crates" / "orbisync-transport-http" / "src" / "lib.rs"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    m = re.search(r"pub struct HttpState\s*\{(.*?)\n\}", text, re.S)
    if not m:
        return []
    block = m.group(1)
    # Collapse multiline `field:\n  Type` so the line regex sees `field: Type`
    block = re.sub(r":\s*\n\s*", ": ", block)
    accessors: list[str] = []
    for line in block.splitlines():
        stripped = line.split("//")[0].strip()
        if not stripped or stripped.startswith("#"):
            continue
        fm = re.match(r"(\w+)\s*:\s*(.+?)\s*,?\s*$", stripped)
        if not fm:
            continue
        name = fm.group(1)
        ty = fm.group(2).strip()
        if ty.startswith("Option<Arc<"):
            accessors.append(name)
    return accessors


def accessor_method_name(field: str) -> str:
    """Map field -> accessor method name (e.g. `identity_admin` -> `identity_admin_service`)."""
    # Special cases where field name != accessor suffix
    # From lib.rs:
    #   identity_admin -> identity_admin_service()
    #   worlds -> world_directory()
    special = {
        "identity_admin": "identity_admin_service",
        "worlds": "world_directory",
    }
    if field in special:
        return special[field]
    return field


def collect_handler_files(root: Path = ROOT) -> list[Path]:
    """Handler source files under transport-http."""
    src = root / "crates" / "orbisync-transport-http" / "src"
    try:
        files = sorted(src.glob("*.rs"))
    except OSError:
        return []
    # Exclude lib.rs? It contains authenticate/authorize which also use accessors
    # but we still want to check them – they are part of handler wiring.
    # Include lib.rs as well, but we will scan all for issue_realtime_ticket ban
    # and for Option accessor handling.
    return [p for p in files if p.name not in ("__init__.py",)]


def find_accessor_usages(text: str, accessors: list[str]) -> list[tuple[str, int, str]]:
    """Find all `let Some(...) = state.<accessor>() else` occurrences.

    Returns list of (accessor, line_no, snippet_200chars_around)
    """
    out: list[tuple[str, int, str]] = []
    # Pattern handles `let Some(x) = state.foo() else {` and also `let Some(x) = state.foo() else { // comment`
    pat = re.compile(r"let\s+Some\s*\(.*?\)\s*=\s*state\.(\w+)\s*\(\)\s*else\s*\{", re.S)
    for mm in pat.finditer(text):
        accessor = mm.group(1)
        if accessor in accessors or accessor in [accessor_method_name(a) for a in accessors]:
            # map back to field name if needed – check accessor is known
            line_no = text[: mm.start()].count("\n") + 1
            snippet = text[mm.start() : mm.start() + 400]
            out.append((accessor, line_no, snippet))
    return out


def extract_else_block(text: str, start_pos: int) -> str | None:
    """Given pos of `else {`, extract the block content with brace matching.

    start_pos points at the `{` after `else`. Returns block content (without outer braces) or None.
    """
    depth = 0
    i = start_pos
    end = -1
    while i < len(text):
        ch = text[i]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                end = i
                break
        i += 1
    if end == -1:
        return None
    # Extract inside the outer braces
    return text[start_pos + 1 : end]


def check_fail_closed(root: Path = ROOT) -> tuple[list[str], dict]:
    accessors = parse_option_accessors(root)
    # Derive accessor method names (what handlers actually call)
    method_names = set(accessor_method_name(a) for a in accessors) | set(accessors)
    handler_files = collect_handler_files(root)
    errors: list[str] = []
    total_usages = 0
    checked_files = 0
    # For V-11 style: `if let Some` without else, `match`, `unwrap_or`, `is_some`/`is_none`
    # These are all fail-open because None silently falls through or unwraps with default.
    # The only allowed pattern is `let Some(x) = state.dep() else { return error }`.

    for fpath in handler_files:
        try:
            text = fpath.read_text(encoding="utf-8")
        except OSError:
            continue
        checked_files += 1
        # Ban issue_realtime_ticket in handlers (C3)
        if "issue_realtime_ticket" in text:
            errors.append(
                f"{fpath.relative_to(root)}: handler must not call `issue_realtime_ticket` "
                f"(JWT fallback removed, use RealtimeTicketStore; C3)"
            )
        # 1. Correct pattern: `let Some(...) = state.<accessor>() else {`
        pat_else = re.compile(r"let\s+Some\s*\(.*?\)\s*=\s*state\.(\w+)\s*\(\)\s*else\s*\{", re.S)
        for mm in pat_else.finditer(text):
            accessor = mm.group(1)
            if accessor not in method_names:
                continue
            total_usages += 1
            brace_pos = mm.end() - 1
            block = extract_else_block(text, brace_pos)
            if block is None:
                errors.append(
                    f"{fpath.relative_to(root)}:{text[:mm.start()].count(chr(10))+1}: "
                    f"could not parse else block for state.{accessor}() (C3 gate internal)"
                )
                continue
            has_return = "return" in block
            has_error = (
                "error_response" in block
                or "auth_error_response" in block
                or "InternalError" in block
                or "INTERNAL_ERROR" in block
                or "StatusCode::INTERNAL_SERVER_ERROR" in block
                or "ticket service unavailable" in block
                or "unavailable" in block.lower()
            )
            has_success = "StatusCode::OK" in block and "realtime_ticket" in block.lower()
            has_jwt_fallback = "issue_realtime_ticket" in block
            if not has_return or not has_error:
                errors.append(
                    f"{fpath.relative_to(root)}:{text[:mm.start()].count(chr(10))+1}: "
                    f"state.{accessor}() None arm must return an error (fail-closed) – "
                    f"found block without proper error return (C3)"
                )
            if has_success:
                errors.append(
                    f"{fpath.relative_to(root)}:{text[:mm.start()].count(chr(10))+1}: "
                    f"state.{accessor}() None arm returns success (StatusCode::OK with ticket) – "
                    f"fail-open fallback forbidden (C3)"
                )
            if has_jwt_fallback:
                errors.append(
                    f"{fpath.relative_to(root)}:{text[:mm.start()].count(chr(10))+1}: "
                    f"state.{accessor}() None arm calls issue_realtime_ticket – JWT fallback forbidden (C3)"
                )

        # 2. V-11 style: `if let Some(x) = state.<accessor>()` – fail-open if no else error.
        # Use line-local pattern to avoid cross-line false positives (e.g. `if let Some(s)=q.from` far from `state.`).
        pat_if_let = re.compile(r"if\s+let\s+Some\s*\([^\)]*\)\s*=\s*state\.(\w+)\s*\(\)")
        for mm in pat_if_let.finditer(text):
            accessor = mm.group(1)
            if accessor not in method_names:
                continue
            line_no = text[: mm.start()].count("\n") + 1
            errors.append(
                f"{fpath.relative_to(root)}:{line_no}: "
                f"`if let Some` on state.{accessor}() is forbidden – use `let Some(...) = state.{accessor}() else {{ return error }}` (fail-closed, V-11/C3)"
            )

        # 3. `match state.<accessor>()` – fail-open if None arm not error return
        pat_match = re.compile(r"match\s+state\.(\w+)\s*\(\)\s*\{")
        for mm in pat_match.finditer(text):
            accessor = mm.group(1)
            if accessor not in method_names:
                continue
            line_no = text[: mm.start()].count("\n") + 1
            brace_pos = mm.end() - 1
            block = extract_else_block(text, brace_pos)
            if block is not None:
                m_none = re.search(r"None\s*=>\s*\{?(.*?)(?:,|\})", block, re.S)
                has_none_error = False
                if m_none is not None:
                    none_body = m_none.group(1)
                    if "return" in none_body and (
                        "error_response" in none_body or "InternalError" in none_body
                    ):
                        has_none_error = True
                    if "error_response" in block and "None" in block:
                        has_none_error = "error_response" in block
                if not has_none_error:
                    errors.append(
                        f"{fpath.relative_to(root)}:{line_no}: "
                        f"`match state.{accessor}()` must have `None => return error` (fail-closed); found match without proper error arm (C3)"
                    )
            else:
                errors.append(
                    f"{fpath.relative_to(root)}:{line_no}: "
                    f"`match state.{accessor}()` is forbidden – use `let Some else` with error return (C3)"
                )

        # 4. `unwrap_or` family – always fail-open for Option accessor
        for meth in ("unwrap_or", "unwrap_or_else", "unwrap_or_default", "unwrap", "expect"):
            pat_unwrap = re.compile(rf"state\.(\w+)\s*\(\)\s*\.\s*{meth}\s*\(")
            for mm in pat_unwrap.finditer(text):
                accessor = mm.group(1)
                if accessor not in method_names:
                    continue
                line_no = text[: mm.start()].count("\n") + 1
                errors.append(
                    f"{fpath.relative_to(root)}:{line_no}: "
                    f"`state.{accessor}().{meth}()` is forbidden – None must return error, not default (fail-closed, C3)"
                )

        # 5. `is_some` / `is_none` branching – fail-open because it doesn't enforce error return
        pat_is = re.compile(r"state\.(\w+)\s*\(\)\s*\.\s*is_(some|none)\s*\(")
        for mm in pat_is.finditer(text):
            accessor = mm.group(1)
            if accessor not in method_names:
                continue
            line_no = text[: mm.start()].count("\n") + 1
            errors.append(
                f"{fpath.relative_to(root)}:{line_no}: "
                f"`state.{accessor}().is_{mm.group(2)}()` branching is forbidden – use `let Some else {{ return error }}` (fail-closed, C3)"
            )

    info = {
        "option_accessors": len(accessors),
        "accessor_list": accessors,
        "method_names": sorted(method_names),
        "checked_files": checked_files,
        "total_usages": total_usages,
    }
    return errors, info


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON report")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()
    root = args.root.resolve()

    accessors = parse_option_accessors(root)
    handler_files = collect_handler_files(root)
    usages = 0
    for f in handler_files:
        try:
            t = f.read_text(encoding="utf-8")
        except OSError:
            continue
        usages += len(re.findall(r"let\s+Some\s*\(.*?\)\s*=\s*state\.\w+\s*\(\)\s*else\s*\{", t))

    print(
        f"http handler fail-closed scan: {len(accessors)} Option accessors, {len(handler_files)} handler files, {usages} else-blocks"
    )

    zero: list[str] = []
    if len(accessors) == 0:
        zero.append(
            "http handler fail-closed gate: no Option accessors were scanned — expected >0 (check parse_option_accessors)"
        )
    if len(handler_files) == 0:
        zero.append(
            "http handler fail-closed gate: no handler files were scanned — expected >0 (check collect_handler_files)"
        )
    if usages == 0:
        zero.append(
            "http handler fail-closed gate: no state.<accessor>() else-blocks were found — expected >0 (check find_accessor_usages pattern)"
        )

    errors, info = check_fail_closed(root)

    if zero:
        for prob in zero:
            print(prob, file=sys.stderr)
        errors = zero + errors
        if args.json:
            print(json.dumps({"errors": errors, "zero_scan": zero, "info": info}, indent=2))
        return 1

    if args.json:
        report = {
            "scanned_option_accessors": len(accessors),
            "scanned_handler_files": len(handler_files),
            "scanned_else_blocks": usages,
            "accessors": accessors,
            "handler_files": [str(p.relative_to(root)) for p in handler_files],
            "errors": errors,
            "info": info,
        }
        print(json.dumps(report, indent=2))

    if errors:
        print("Http handler fail-closed check FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        print(f"\nerrors={len(errors)}", file=sys.stderr)
        return 1

    print(
        f"All {info['total_usages']} handler Option accessor usages are fail-closed (no JWT fallback)."
    )
    print("errors=0")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
