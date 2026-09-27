#!/usr/bin/env python3
"""Gate that HttpState dependencies are wired in the composition root.

IDEM-1: `HttpState` keeps several `Option<Arc<dyn ...>>` / `Arc<dyn ...>`
dependencies. If the production composition root (`crates/orbisync-server/src/main.rs`)
forgets to call `with_*` or pass the `HttpState::new` arg, the handler falls back
to a fail-open path (e.g. idempotency silently skipped). This gate derives the
dependency fields from `crates/orbisync-transport-http/src/lib.rs` (no hand-written
list) and verifies each is wired in `crates/orbisync-server/src/main.rs`.

Wiring is considered present when `main.rs` contains the corresponding
`with_*` call (for `Option` deps) or the `HttpState::new` argument (for required deps
like `clock`). The mapping `field -> with_* method` is itself derived from the
`impl HttpState` block in `lib.rs` by scanning `pub fn with_*` definitions.

Zero-scan guards ensure the gate is not green due to broken file-selection.

Usage:
    python scripts/check_http_state_wiring.py [--json] [--root PATH]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HTTP_LIB = ROOT / "crates" / "orbisync-transport-http" / "src" / "lib.rs"
MAIN_RS = ROOT / "crates" / "orbisync-server" / "src" / "main.rs"


def parse_http_state_fields(root: Path = ROOT) -> list[tuple[str, str]]:
    """Derive HttpState dependency fields from source.

    Returns list of (field_name, type_text) for fields whose type contains `Arc<`.
    This covers `Option<Arc<dyn ...>>`, `Option<Arc<...>>` and `Arc<dyn ...>`.
    Hand-written allowlists are not used; the struct is parsed directly.
    """
    path = root / "crates" / "orbisync-transport-http" / "src" / "lib.rs"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    # Find `pub struct HttpState { ... }`
    m = re.search(r"pub struct HttpState\s*\{(.*?)\n\}", text, re.S)
    if not m:
        return []
    block = m.group(1)
    # Collapse `field:\n  Type` continuations (e.g. identity_admin) so line parser sees `field: Type`
    block = re.sub(r":\s*\n\s*", ": ", block)
    fields: list[tuple[str, str]] = []
    for line in block.splitlines():
        # Strip comments
        stripped = line.split("//")[0].strip()
        if not stripped or stripped.startswith("#"):
            continue
        # Match `field: Type`
        fm = re.match(r"(\w+)\s*:\s*(.+?)\s*,?\s*$", stripped)
        if not fm:
            continue
        name = fm.group(1)
        ty = fm.group(2).strip()
        # Consider dependency if type contains Arc<
        if "Arc<" in ty:
            fields.append((name, ty))
    return fields


def parse_http_state_with_methods(root: Path = ROOT) -> dict[str, str]:
    """Derive mapping field -> with_* method from impl HttpState block.

    Scans `impl HttpState` for `pub fn with_*` and extracts which field is set
    via `Self { field: ...`.
    Returns dict field_name -> method_name (e.g. "idempotency_store" -> "with_idempotency_store").
    """
    path = root / "crates" / "orbisync-transport-http" / "src" / "lib.rs"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return {}
    # Find impl HttpState block(s) – search globally for with_* patterns
    mapping: dict[str, str] = {}
    # Pattern: pub fn with_xxx(self ... Self { field: ...
    # Need to capture method name and field name assigned inside body.
    # We scan for each `pub fn with_\w+` and then look ahead for `Self {` with field.
    for mm in re.finditer(r"pub fn (with_\w+)\s*\(\s*self[^{]*\{", text, re.S):
        method = mm.group(1)
        # Take substring from this method start to next ~500 chars to find field assignment
        start = mm.end()
        snippet = text[start : start + 800]
        # Look for `field_name: Some(` or `field_name:`
        fm = re.search(r"Self\s*\{\s*(\w+)\s*:", snippet)
        if fm:
            field = fm.group(1)
            mapping[field] = method
        else:
            # Fallback: try to find `with_` method's target via simple heuristic
            # For some methods like `with_allow_stub_bearer` field is `allow_stub_bearer` directly
            # The method name is `with_<field>` so we can infer.
            inferred = method[5:]  # strip "with_"
            # If inferred field exists in struct, map it
            # We'll add anyway; caller will verify existence
            if inferred not in mapping:
                mapping[inferred] = method
    # Additional direct mapping for allow_stub_bearer which uses `mut self` pattern not `Self {`
    # Handle `pub fn with_allow_stub_bearer(mut self, allow: bool) -> Self { self.allow_stub_bearer = allow;`
    # This won't be captured above; fallback via name.
    for mm in re.finditer(r"pub fn (with_\w+)\s*\(\s*self", text):
        method = mm.group(1)
        inferred = method[5:]
        if inferred not in mapping:
            # Check if inferred field exists as HttpState field
            # We'll add if not already
            mapping[inferred] = method
    # Special case: `worlds` field is wired via `with_world_directory`
    # Our generic inferred would map `world_directory` -> field `world_directory` not `worlds`.
    # The above `Self { worlds:` capture already handles it, so keep that mapping.
    # Ensure worlds mapping is correct
    if "worlds" not in mapping:
        # Look for with_world_directory explicitly
        if "with_world_directory" in text:
            mapping["worlds"] = "with_world_directory"
    return mapping


def parse_http_state_new_params(root: Path = ROOT) -> list[str]:
    """Derive param names of `HttpState::new` from source."""
    path = root / "crates" / "orbisync-transport-http" / "src" / "lib.rs"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    m = re.search(r"pub fn new\s*\((.*?)\)\s*->\s*Self", text, re.S)
    if not m:
        return []
    block = m.group(1)
    params: list[str] = []
    for part in block.split(","):
        part = part.strip()
        if not part:
            continue
        # Each param is `name: Type`
        pm = re.match(r"(\w+)\s*:", part)
        if pm:
            params.append(pm.group(1))
    return params


def check_wiring(root: Path = ROOT) -> tuple[list[str], dict]:
    """Check each derived field is wired in main.rs.

    Returns (errors, info) where info contains scan counts.
    """
    fields = parse_http_state_fields(root)
    with_map = parse_http_state_with_methods(root)
    new_params = parse_http_state_new_params(root)
    main_path = root / "crates" / "orbisync-server" / "src" / "main.rs"
    try:
        main_text = main_path.read_text(encoding="utf-8")
    except OSError as e:
        return [f"cannot read {main_path}: {e}"], {"fields": 0, "checked": 0}

    errors: list[str] = []
    scanned_fields = len(fields)
    # Zero-scan handled by caller, but also record here
    for field_name, ty in fields:
        wired = False
        expected = ""
        # Priority: with_* method if exists
        if field_name in with_map:
            expected = with_map[field_name]
            # Check main.rs contains the method call (e.g. .with_idempotency_store(
            if expected in main_text and f".{expected}" in main_text:
                wired = True
            # Also consider HttpState::new arg for fields like clock which have both
            if not wired and field_name in new_params:
                # For new-wired fields, presence of HttpState::new is enough, but also check param appears
                if "HttpState::new" in main_text:
                    # Heuristic: check that field name appears near HttpState::new or with method
                    # For required fields, just ensure HttpState::new present
                    wired = True
                    expected = f"{expected} or HttpState::new"
        elif field_name in new_params:
            expected = "HttpState::new"
            if "HttpState::new" in main_text:
                # Ensure the field's config value appears? For TTL keys we can check,
                # but for generic we just ensure new is called.
                wired = True
        else:
            # Fallback for fields without explicit with_map (should not happen)
            inferred = f"with_{field_name}"
            expected = inferred
            if inferred in main_text:
                wired = True

        if not wired:
            # Provide direct field name in error for mutation test M1
            if field_name in with_map:
                expected = with_map[field_name]
            elif field_name in new_params:
                expected = "HttpState::new"
            else:
                expected = f"with_{field_name}"
            errors.append(
                f"HttpState field '{field_name}' is not wired in crates/orbisync-server/src/main.rs "
                f"(expected '{expected}' call or HttpState::new arg) [IDEM-1]"
            )

    info = {
        "fields": scanned_fields,
        "with_methods": len(with_map),
        "new_params": len(new_params),
        "fields_list": [f[0] for f in fields],
    }
    return errors, info


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON report")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()
    root = args.root.resolve()

    fields = parse_http_state_fields(root)
    with_map = parse_http_state_with_methods(root)
    new_params = parse_http_state_new_params(root)

    scanned_fields = len(fields)
    scanned_methods = len(with_map)
    scanned_new = len(new_params)

    print(f"http state wiring scan: {scanned_fields} fields, {scanned_methods} with_* methods, {scanned_new} new params")

    zero: list[str] = []
    if scanned_fields == 0:
        zero.append(
            "http state wiring gate: no HttpState fields were scanned — expected >0 fields (check parse_http_state_fields path logic)"
        )
    if scanned_methods == 0:
        zero.append(
            "http state wiring gate: no with_* methods were scanned — expected >0 methods (check parse_http_state_with_methods path logic)"
        )
    # new params may be 0 if parsing fails, but we expect >0
    if scanned_new == 0:
        zero.append(
            "http state wiring gate: no HttpState::new params were scanned — expected >0 params (check parse_http_state_new_params path logic)"
        )

    errors, info = check_wiring(root)

    if zero:
        for prob in zero:
            print(prob, file=sys.stderr)
        errors = zero + errors
        if args.json:
            print(json.dumps({"errors": errors, "zero_scan": zero, "info": info}, indent=2))
        return 1

    if args.json:
        report = {
            "scanned_fields": scanned_fields,
            "scanned_with_methods": scanned_methods,
            "scanned_new_params": scanned_new,
            "fields": [f[0] for f in fields],
            "with_map": with_map,
            "new_params": new_params,
            "errors": errors,
            "info": info,
        }
        print(json.dumps(report, indent=2))

    if errors:
        print("HttpState wiring check FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        print(f"\nerrors={len(errors)}", file=sys.stderr)
        return 1

    print(f"All {scanned_fields} HttpState dependencies are wired in crates/orbisync-server/src/main.rs.")
    print("errors=0")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
