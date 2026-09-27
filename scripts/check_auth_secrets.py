#!/usr/bin/env python3
"""Gate that ADR-002 required secrets are independent and not reused.

Checks derived from ADR-002 §Refresh Token and §Secretとlog:

  - `:31` DBにはraw tokenを保存せず、専用secret keyを用いたHMAC-SHA-256 digestだけを保存する
  - `:52` 相関が必要な場合は、用途別keyのHMACまたはserver生成のopaque correlation IDを使用する

Concretely:

  1. ADR requires five independent secrets, each as its own config key in
     crates/orbisync-config/src/keys.rs:
       - auth.token_signing_key_env   (JWT Ed25519 private key)
       - auth.pagination_hmac_key_env (pagination cursor HMAC)
       - auth.refresh_token_hmac_key_env (refresh digest HMAC, V-10)
       - auth.realtime_ticket_hmac_key_env (realtime ticket HMAC, C1)
       - auth.idempotency_hmac_key_env (idempotency HMAC, P2-C1)

     Each must be present in CONFIG_KEYS. Missing any is a gate failure.

  2. The same environment variable name must not be used for two purposes.
     That is the exact bug of V-10 (refresh digest reused pagination key).
     The gate checks two levels:
     a) CONFIG_KEYS mapped via env_var_for_key must be unique (covers
        accidental alias where two dotted keys map to same ORBISYNC_* name).
        The generic uniqueness is already tested in keys.rs, but the gate
        makes it fail-closed here as well.
     b) Production Rust code must not use the same dotted key for two
        purposes. Specifically, crates/orbisync-server/src/main.rs must read
        the refresh HMAC into `refresh_hmac_key` from
        `refresh_token_hmac_key_env`, not from `pagination_hmac_key_env`.
        If `refresh_hmac_key` is derived from `pagination_hmac_key_env`,
        the gate reports duplicate env-var usage.

Implementation follows existing gates: walk crates/*/src for production
readers, strip comments/strings before searching, and guard against
zero-scan (21aa314 / CR-15).

Usage:
    python scripts/check_auth_secrets.py [--json] [--root PATH]

Exit 0 when all checks pass, 1 otherwise.
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

# ADR-002 requires these five config keys to exist independently.
ADR_REQUIRED_KEYS = [
    "auth.token_signing_key_env",
    "auth.pagination_hmac_key_env",
    "auth.refresh_token_hmac_key_env",
    "auth.realtime_ticket_hmac_key_env",
    "auth.idempotency_hmac_key_env",
]


def env_var_for_key(key: str) -> str:
    return f"ORBISYNC_{key.replace('.', '_').upper()}"


def parse_config_keys(root: Path = ROOT) -> list[str]:
    path = root / "crates" / "orbisync-config" / "src" / "keys.rs"
    text = path.read_text(encoding="utf-8")
    m = re.search(r"CONFIG_KEYS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\]", text, re.S)
    if not m:
        raise RuntimeError(f"cannot parse CONFIG_KEYS from {path}")
    block = m.group(1)
    keys = re.findall(r'"([^"]+)"', block)
    # Do not treat empty as error here; caller handles zero-scan
    return keys


def collect_production_files(root: Path = ROOT) -> list[Path]:
    files: list[Path] = []
    for crate_dir in (root / "crates").iterdir():
        if not crate_dir.is_dir():
            continue
        if crate_dir.name in EXCLUDED_CRATES:
            continue
        src = crate_dir / "src"
        if not src.exists():
            continue
        for path in src.rglob("*.rs"):
            files.append(path)
    return sorted(files)


def strip_rust_comments_and_strings(source: str) -> str:
    """Reuse logic from check_config_usage: replace comments/strings with spaces."""
    n = len(source)
    out: list[str] = []
    i = 0
    state = "code"
    block_depth = 0
    raw_hashes = 0
    while i < n:
        c = source[i]
        nxt = source[i + 1] if i + 1 < n else ""
        if state == "code":
            if c == "r":
                j = i + 1
                hashes = 0
                while j < n and source[j] == "#":
                    hashes += 1
                    j += 1
                if j < n and source[j] == '"':
                    state = "raw_string"
                    raw_hashes = hashes
                    out.append(" ")
                    for _ in range(hashes):
                        out.append(" ")
                    out.append(" ")
                    i = j + 1
                    continue
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
                k = i + 1
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
                out.append(c)
                i += 1
                continue
            out.append(c)
            i += 1
            continue
        elif state == "line_comment":
            if c == "\n":
                state = "code"
                out.append("\n")
                i += 1
            else:
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
                out.append(" ")
                if nxt == "\n":
                    out.append("\n")
                    i += 2
                else:
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
            if c == '"':
                j = i + 1
                hashes = 0
                while hashes < raw_hashes and j < n and source[j] == "#":
                    hashes += 1
                    j += 1
                if hashes == raw_hashes:
                    out.append(" ")
                    for _ in range(hashes):
                        out.append(" ")
                    state = "code"
                    i = j
                    continue
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


def check_adr_keys_present(keys: list[str]) -> list[str]:
    errors: list[str] = []
    for required in ADR_REQUIRED_KEYS:
        if required not in keys:
            errors.append(
                f"ADR-002 requires secret key '{required}' to be declared in crates/orbisync-config/src/keys.rs (CONFIG_KEYS) — missing"
            )
    return errors


def check_env_var_uniqueness(keys: list[str]) -> list[str]:
    errors: list[str] = []
    seen: dict[str, str] = {}
    for key in keys:
        env = env_var_for_key(key)
        if env in seen:
            errors.append(
                f"duplicate environment variable '{env}' for keys '{seen[env]}' and '{key}' — same env var must not be reused for two purposes (V-10)"
            )
        else:
            seen[env] = key
    # Also check that the three ADR-required env vars are distinct
    # (redundant with above but makes the V-10 message explicit)
    adr_envs = [env_var_for_key(k) for k in ADR_REQUIRED_KEYS]
    if len(set(adr_envs)) != len(adr_envs):
        errors.append(
            f"ADR-002 secrets must map to distinct env vars: {adr_envs} contains duplicates"
        )
    return errors


def check_production_usage(root: Path, files: list[Path]) -> list[str]:
    errors: list[str] = []
    # Collect cleaned content for all files
    # For V-10 duplication detection: `refresh_hmac_key` must be derived from
    # refresh_token_hmac_key_env, not pagination_hmac_key_env.
    # We search for the pattern "refresh_hmac_key" assignment that references
    # pagination_hmac_key_env.
    refresh_pagination_pattern = re.compile(
        r"refresh_hmac_key.*?pagination_hmac_key_env", re.S | re.I
    )
    # P2-C1: idempotency_hmac_key must be derived from idempotency_hmac_key_env,
    # not from refresh/pagination/realtime keys.
    idempotency_reuse_pattern = re.compile(
        r"idempotency_hmac_key.*?(?:pagination_hmac_key_env|refresh_token_hmac_key_env|realtime_ticket_hmac_key_env)",
        re.S | re.I,
    )
    # Also detect any file where pagination_hmac_key_env is used in a refresh
    # context: heuristic is that the same cleaned file contains both
    # pagination_hmac_key_env and refresh_hmac_key or refresh_token usage in
    # a narrow window.
    pagination_usages: list[str] = []
    refresh_usages: list[str] = []
    idempotency_usages: list[str] = []
    realtime_usages: list[str] = []
    for path in files:
        try:
            raw = path.read_text(encoding="utf-8")
        except OSError:
            continue
        cleaned = strip_rust_comments_and_strings(raw)
        # Check for the specific V-10 bug pattern
        if refresh_pagination_pattern.search(cleaned):
            # Ensure it's not a comment about the bug itself; require actual code
            # `env.get(&config.auth.pagination_hmac_key_env)` near refresh_hmac_key
            # The pattern above already matched a refresh assignment from pagination,
            # which is exactly the bug.
            errors.append(
                f"{path.relative_to(root).as_posix()}: refresh digest key is derived from pagination_hmac_key_env — same environment variable used for two purposes (V-10). Use auth.refresh_token_hmac_key_env."
            )
        if idempotency_reuse_pattern.search(cleaned):
            errors.append(
                f"{path.relative_to(root).as_posix()}: idempotency HMAC key is derived from non-idempotency env var — same environment variable used for two purposes (P2-C1). Use auth.idempotency_hmac_key_env."
            )
        # Track generic presence for duplicate-env heuristic
        if "pagination_hmac_key_env" in cleaned:
            pagination_usages.append(path.relative_to(root).as_posix())
        if "refresh_token_hmac_key_env" in cleaned:
            refresh_usages.append(path.relative_to(root).as_posix())
        if "idempotency_hmac_key_env" in cleaned:
            idempotency_usages.append(path.relative_to(root).as_posix())
        if "realtime_ticket_hmac_key_env" in cleaned:
            realtime_usages.append(path.relative_to(root).as_posix())

    # If refresh key exists but is never read in production code outside
    # orbisync-config, that's effectively dead (same as check_config_usage, but
    # we make it explicit for ADR).
    if not refresh_usages:
        errors.append(
            "ADR-002: auth.refresh_token_hmac_key_env has no production reader outside orbisync-config (expected in crates/orbisync-server/src/main.rs)"
        )
    if not realtime_usages:
        errors.append(
            "ADR-002: auth.realtime_ticket_hmac_key_env has no production reader outside orbisync-config (expected in crates/orbisync-server/src/main.rs)"
        )
    if not idempotency_usages:
        errors.append(
            "ADR-002: auth.idempotency_hmac_key_env has no production reader outside orbisync-config (expected in crates/orbisync-server/src/main.rs)"
        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON report")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()
    root = args.root.resolve()

    errors: list[str] = []
    # Parse keys
    try:
        keys = parse_config_keys(root)
    except (RuntimeError, OSError, UnicodeDecodeError) as exc:
        keys = []
        errors.append(str(exc))

    files = collect_production_files(root)

    scanned_keys = len(keys)
    scanned_files = len(files)
    print(f"auth secrets scan: {scanned_keys} keys, {scanned_files} production files")

    # Zero-scan guards (21aa314 style)
    zero: list[str] = []
    if scanned_keys == 0:
        zero.append(
            "auth secrets gate: no configuration keys were scanned — expected >0 keys (check parse_config_keys path logic)"
        )
    if scanned_files == 0:
        zero.append(
            "auth secrets gate: no production files were scanned — expected >0 production files (check collect_production_files path logic)"
        )
    if zero:
        for prob in zero:
            print(prob, file=sys.stderr)
            errors.append(prob)
        if args.json:
            print(json.dumps({"errors": errors, "zero_scan": zero}, indent=2))
        return 1

    # Checks
    errors.extend(check_adr_keys_present(keys))
    errors.extend(check_env_var_uniqueness(keys))
    errors.extend(check_production_usage(root, files))

    # Also check that required_secret_env_vars in model includes refresh key
    # by inspecting model.rs directly (defense in depth).
    model_path = root / "crates" / "orbisync-config" / "src" / "model.rs"
    try:
        model_text = model_path.read_text(encoding="utf-8")
        if "refresh_token_hmac_key_env" not in model_text:
            errors.append(
                "crates/orbisync-config/src/model.rs: missing refresh_token_hmac_key_env field or usage (expected in AuthConfig, Default, validate, required_secret_env_vars)"
            )
        # Ensure required_secret_env_vars includes it
        if "required_secret_env_vars" in model_text and "refresh_token_hmac_key_env" not in model_text.split("required_secret_env_vars")[1].split("}")[0]:
            # Fallback simple check: file must contain the field in that function
            if "ORBISYNC_REFRESH_TOKEN_HMAC_KEY" not in model_text:
                errors.append(
                    "crates/orbisync-config/src/model.rs: required_secret_env_vars must include refresh_token_hmac_key_env / ORBISYNC_REFRESH_TOKEN_HMAC_KEY"
                )
    except OSError as exc:
        errors.append(f"cannot read {model_path}: {exc}")

    if args.json:
        report = {
            "scanned_keys": scanned_keys,
            "scanned_files": scanned_files,
            "adr_required": ADR_REQUIRED_KEYS,
            "errors": errors,
        }
        print(json.dumps(report, indent=2))

    if errors:
        print("Auth secrets check FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        print(f"\nerrors={len(errors)}", file=sys.stderr)
        return 1

    print("All ADR-002 secrets are independently declared and not reused.")
    print("errors=0")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
