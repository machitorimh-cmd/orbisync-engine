#!/usr/bin/env python3
"""Gate that every orbisync-config key has a production reader outside orbisync-config.

For each dotted key in crates/orbisync-config/src/keys.rs the script checks
that at least one Rust source file under crates/*/src (excluding
crates/orbisync-config) contains a code-level reference to that setting.
Only code is considered: comments and string literals are stripped before
searching, so a key mentioned solely in a comment does not count as a
reader.  The canonical example is auth.token_signing_key_env which is
mentioned in a comment in crates/orbisync-server/src/main.rs but whose
create_token_service discards the configuration and uses a hard-coded
Ed25519 key.

The check uses the leaf field name of each key (the part after the last dot)
and looks for a field access pattern ``.leaf`` in cleaned Rust code.
This avoids false positives from substring matches and from comment-only
mentions while remaining tolerant to variable naming (``config`` vs
``cfg``) and to the fact that some fields are read as ``config.log_level``
on a sub-config value rather than ``config.observability.log_level``.

No allowlist is provided: every dead key is reported.  The gate is expected
to be red while dead keys exist; callers must fix the production code or
remove the configuration surface, not suppress the report.

Usage:
    python scripts/check_config_usage.py [--json] [--root PATH]

Exit code 0 when every key has a reader, 1 otherwise.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
KEYS_PATH = ROOT / "crates" / "orbisync-config" / "src" / "keys.rs"
CRATES_ROOT = ROOT / "crates"
EXCLUDED_CRATES = {"orbisync-config"}


def parse_config_keys(root: Path = ROOT) -> list[str]:
    path = root / "crates" / "orbisync-config" / "src" / "keys.rs"
    text = path.read_text(encoding="utf-8")
    # Extract the CONFIG_KEYS array content
    m = re.search(r"CONFIG_KEYS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\]", text, re.S)
    if not m:
        raise RuntimeError(f"cannot parse CONFIG_KEYS from {path}")
    block = m.group(1)
    keys = re.findall(r'"([^"]+)"', block)
    if not keys:
        raise RuntimeError(f"no keys found in {path}")
    return keys


def collect_production_files(root: Path = ROOT) -> list[Path]:
    files: list[Path] = []
    for crate_dir in (root / "crates").iterdir():
        if not crate_dir.is_dir():
            continue
        if crate_dir.name in EXCLUDED_CRATES:
            continue
        # Only production source under src; tests/, benches/ etc. are not
        # considered production readers.  This matches the "本番コード"
        # requirement and avoids counting test helpers.
        src = crate_dir / "src"
        if not src.exists():
            continue
        for path in src.rglob("*.rs"):
            # Skip any path that is inside orbisync-config (already excluded)
            # and exclude generated or vendored files.
            files.append(path)
    # Also consider crates that may not follow crate/src layout but still
    # have production code (e.g., orbisync-server).  Already covered.
    return sorted(files)


def strip_rust_comments_and_strings(source: str) -> str:
    """Return *source* with comments and string/char literals replaced by spaces.

    Newlines are preserved so line numbers remain stable.  Content inside
    comments and literals is replaced with spaces (except newlines) to avoid
    accidental token concatenation.
    """
    n = len(source)
    out = []
    i = 0
    state = "code"  # code, line_comment, block_comment, string, char, raw_string
    block_depth = 0
    raw_hashes = 0
    # For string we need to know delimiter
    while i < n:
        c = source[i]
        nxt = source[i + 1] if i + 1 < n else ""

        if state == "code":
            # Check for raw string start: r#*"
            if c == "r":
                # Look ahead for hashes then "
                j = i + 1
                hashes = 0
                while j < n and source[j] == "#":
                    hashes += 1
                    j += 1
                if j < n and source[j] == '"':
                    # Raw string start
                    state = "raw_string"
                    raw_hashes = hashes
                    # Emit spaces for r, hashes, "
                    out.append(" ")
                    for _ in range(hashes):
                        out.append(" ")
                    out.append(" ")
                    i = j + 1
                    continue
                # Also handle b" or br"? Not needed for config keys
            if c == "/" and nxt == "/":
                state = "line_comment"
                out.append(" ")
                out.append(" ")
                i += 2
                continue
            if c == "/" and nxt == "*":
                state = "block_comment"
                block_depth = 1
                out.append(" ")
                out.append(" ")
                i += 2
                continue
            if c == '"':
                state = "string"
                out.append(" ")
                i += 1
                continue
            if c == "'":
                # Could be char literal or lifetime.  Treat as char only if
                # it looks like a char literal: next char and then closing '.
                # Otherwise keep as code.
                # Simple heuristic: if there's a closing ' within a few chars
                # and no newline, treat as char.
                k = i + 1
                # skip escaped?
                if k < n:
                    if source[k] == "\\":
                        k += 2
                    else:
                        k += 1
                    if k < n and source[k] == "'":
                        state = "char"
                        out.append(" ")
                        i += 1
                        continue
                # Lifetime or stray ', keep as code
                out.append(c)
                i += 1
                continue
            # Regular code character
            out.append(c)
            i += 1
            continue

        elif state == "line_comment":
            if c == "\n":
                state = "code"
                out.append("\n")
                i += 1
            else:
                # Replace comment content with space, preserve length
                out.append(" ")
                i += 1
            continue

        elif state == "block_comment":
            if c == "/" and nxt == "*":
                block_depth += 1
                out.append(" ")
                out.append(" ")
                i += 2
                continue
            if c == "*" and nxt == "/":
                block_depth -= 1
                out.append(" ")
                out.append(" ")
                i += 2
                if block_depth == 0:
                    state = "code"
                continue
            if c == "\n":
                out.append("\n")
            else:
                out.append(" ")
            i += 1
            continue

        elif state == "string":
            if c == "\\":
                # Escaped char
                out.append(" ")
                if nxt == "\n":
                    out.append("\n")
                    i += 2
                else:
                    # escape consumes next char as well
                    if nxt:
                        out.append(" ")
                        i += 2
                    else:
                        i += 1
                continue
            if c == '"':
                out.append(" ")
                state = "code"
                i += 1
                continue
            if c == "\n":
                # Unterminated string? Keep newline and exit string
                out.append("\n")
                i += 1
                continue
            out.append(" ")
            i += 1
            continue

        elif state == "char":
            if c == "\\":
                out.append(" ")
                if nxt:
                    out.append(" ")
                    i += 2
                else:
                    i += 1
                continue
            if c == "'":
                out.append(" ")
                state = "code"
                i += 1
                continue
            if c == "\n":
                out.append("\n")
                state = "code"
                i += 1
                continue
            out.append(" ")
            i += 1
            continue

        elif state == "raw_string":
            # Raw string content until " + hashes
            if c == '"':
                # Check if followed by raw_hashes #'s
                j = i + 1
                hashes = 0
                while hashes < raw_hashes and j < n and source[j] == "#":
                    hashes += 1
                    j += 1
                if hashes == raw_hashes:
                    # Closing delimiter
                    out.append(" ")
                    for _ in range(hashes):
                        out.append(" ")
                    state = "code"
                    i = j
                    continue
                # Inside raw string, not closing
                if c == "\n":
                    out.append("\n")
                else:
                    out.append(" ")
                i += 1
                continue
            if c == "\n":
                out.append("\n")
            else:
                out.append(" ")
            i += 1
            continue

        else:
            out.append(c)
            i += 1

    return "".join(out)


def key_to_leaf(key: str) -> str:
    return key.split(".")[-1]


def find_readers_for_key(
    key: str, files: list[Path], root: Path = ROOT
) -> list[str]:
    """Return list of production files that contain a code reference to *key*.

    Search is performed on cleaned Rust code (comments/strings stripped) and
    requires a field-access pattern that includes the parent segment for the
    two TTL keys that previously caused a false positive.  For
    ``auth.refresh_token_ttl_seconds`` and ``auth.access_token_ttl_seconds``
    the check requires ``.auth.<leaf>`` (e.g. ``.auth.refresh_token_ttl_seconds``)
    so that ``self.refresh_token_ttl_seconds`` in ``HttpState`` is not counted
    as a configuration reader.  This prevents the dead-config anti-pattern
    from being hidden by an unrelated struct field with the same leaf name.
    For all other keys the original leaf-only check (``.leaf``) is retained
    because those values are legitimately read via sub-config variables
    (e.g. ``config.max_connections`` where ``config: &DatabaseConfig``,
    ``config.log_format`` where ``config: &ObservabilityConfig``,
    ``config.max_message_bytes`` where ``config: &RealtimeConfig``).  In those
    cases ``.database.max_connections`` would not be present, so requiring the
    parent would be a false dead.  See report for the full list.
    The parent + leaf must appear consecutively with only dots and
    whitespace (including newlines) between them, so
    ``config.auth.refresh_token_ttl_seconds``,
    ``config . auth . refresh_token_ttl_seconds`` and
    ``config.auth\n    .refresh_token_ttl_seconds`` all match.
    """
    parts = key.split(".")
    # Harden only the two TTL keys that share leaf names with HttpState fields.
    # For all other keys keep the leaf-only check to handle sub-config reads.
    if key in ("auth.refresh_token_ttl_seconds", "auth.access_token_ttl_seconds"):
        parent = parts[-2]
        leaf = parts[-1]
        pattern = re.compile(
            r"\.\s*" + re.escape(parent) + r"\s*\.\s*" + re.escape(leaf) + r"\b"
        )
    else:
        leaf = parts[-1]
        pattern = re.compile(r"\.\s*" + re.escape(leaf) + r"\b")
    readers: list[str] = []
    for path in files:
        try:
            raw = path.read_text(encoding="utf-8")
        except OSError:
            continue
        cleaned = strip_rust_comments_and_strings(raw)
        if pattern.search(cleaned):
            readers.append(path.relative_to(root).as_posix())
    return readers


def check_config_usage(root: Path = ROOT) -> dict[str, list[str]]:
    """Return mapping from dead key -> [] (empty) or alive key -> readers.

    The returned dict contains only dead keys as keys with their (empty)
    reader list omitted for backward compatibility? Instead we return two
    dicts: dead and alive. This helper returns dead mapping for gate.
    """
    keys = parse_config_keys(root)
    files = collect_production_files(root)
    dead: dict[str, list[str]] = {}
    alive: dict[str, list[str]] = {}
    for key in keys:
        readers = find_readers_for_key(key, files, root)
        if readers:
            alive[key] = readers
        else:
            dead[key] = readers
    # For convenience return dead only via side function, but we need both
    # in main. This function returns dead; callers can also compute alive.
    return dead


def check_all(root: Path = ROOT) -> tuple[dict[str, list[str]], dict[str, list[str]]]:
    keys = parse_config_keys(root)
    files = collect_production_files(root)
    dead: dict[str, list[str]] = {}
    alive: dict[str, list[str]] = {}
    for key in keys:
        readers = find_readers_for_key(key, files, root)
        if readers:
            alive[key] = readers
        else:
            dead[key] = readers
    return dead, alive


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON report")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()

    root = args.root.resolve()
    try:
        dead, alive = check_all(root)
    except RuntimeError as exc:
        # parse_config_keys raises when no keys are found; translate to the
        # zero-scan guard message so the gate fails closed with a direct
        # "no ... were scanned" diagnostic rather than an unhandled traceback.
        # This preserves the fail-closed contract even if the file is empty.
        files_fallback = collect_production_files(root)
        print(f"config scan: 0 keys, {len(files_fallback)} production files")
        msg = "config gate: no configuration keys were scanned — expected >0 keys (check parse_config_keys path logic)"
        print(msg, file=sys.stderr)
        print(f"detail: {exc}", file=sys.stderr)
        if args.json:
            report = {
                "total_keys": 0,
                "alive": 0,
                "dead": 0,
                "dead_keys": [],
                "production_files": len(files_fallback),
                "error": msg,
                "detail": str(exc),
            }
            print(json.dumps(report, indent=2, ensure_ascii=False))
        return 1
    # Scan counts derived at runtime (no fixed expectations)
    files = collect_production_files(root)
    total_keys = len(alive) + len(dead)
    print(f"config scan: {total_keys} keys, {len(files)} production files")

    # Self-check: the config gate must actually scan something for each dimension.
    # An overall "dead==0" check cannot detect an inactive subset: "keys=0" with
    # 82 production files would still be 0 dead and be green, while "files=0"
    # would be 22 dead and be red only indirectly. These guards make the failure
    # direct and distinguishable, matching the S1/H1/A2/A3/A4/D3 guards added in
    # 21aa314 (5aa44ef) for the other gates.
    zero_problems: list[str] = []
    if total_keys == 0:
        zero_problems.append(
            "config gate: no configuration keys were scanned — expected >0 keys (check parse_config_keys path logic)"
        )
    if len(files) == 0:
        zero_problems.append(
            "config gate: no production files were scanned — expected >0 production files (check collect_production_files path logic)"
        )
    if zero_problems:
        for prob in zero_problems:
            print(prob, file=sys.stderr)
        if args.json:
            report = {
                "total_keys": total_keys,
                "alive": len(alive),
                "dead": len(dead),
                "dead_keys": sorted(dead.keys()),
                "production_files": len(files),
                "errors": zero_problems,
            }
            print(json.dumps(report, indent=2, ensure_ascii=False))
        return 1

    if args.json:
        report = {
            "total_keys": total_keys,
            "alive": len(alive),
            "dead": len(dead),
            "dead_keys": sorted(dead.keys()),
            "details": {
                key: {"leaf": key_to_leaf(key), "readers": readers}
                for key, readers in sorted(alive.items())
            },
            "dead_details": {
                key: {"leaf": key_to_leaf(key), "reason": "no `.leaf` field access in production Rust code outside orbisync-config (comments/strings stripped)"}
                for key in sorted(dead.keys())
            },
        }
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        if dead:
            print("Dead configuration keys (no production reader outside orbisync-config):", file=sys.stderr)
            for key in sorted(dead.keys()):
                leaf = key_to_leaf(key)
                print(f"  - {key} (leaf `{leaf}` not found as `. {leaf}` in crates/*/src outside orbisync-config, comments/strings ignored)", file=sys.stderr)
            print(f"\nTotal: {len(dead)} dead / {total_keys} keys", file=sys.stderr)
            print(f"Alive: {sorted(alive.keys())}", file=sys.stderr)
        else:
            print("All configuration keys have at least one production reader.")

    return 1 if dead else 0


if __name__ == "__main__":
    raise SystemExit(main())
