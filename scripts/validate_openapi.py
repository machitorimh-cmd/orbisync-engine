#!/usr/bin/env python3
"""Validate the OpenAPI documents against the OpenAPI 3.1 schema.

`scripts/validate_design.py` checks that the documents agree with the design
(operation coverage, error registry, examples) without any third party package.
This script performs the complementary check: that the documents are valid
OpenAPI. It therefore requires `openapi-spec-validator` and runs in its own CI
job so the dependency-free property of `validate_design.py` is preserved.

    pip install openapi-spec-validator pyyaml
    python scripts/validate_openapi.py
"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCUMENTS = [
    ROOT / "openapi" / "orbisync-v1.yaml",
    ROOT / "openapi" / "orbisync-v1-planned.yaml",
]

# `openapi/errors.yaml` is a registry, not an OpenAPI document. It is validated
# structurally here and semantically by `validate_design.py`.
REGISTRIES = [ROOT / "openapi" / "errors.yaml"]


def main() -> int:
    try:
        import yaml
        from openapi_spec_validator import validate
        from openapi_spec_validator.readers import read_from_filename
    except ImportError as error:  # pragma: no cover - depends on the environment
        print(
            "error: install the validator first: "
            "pip install openapi-spec-validator pyyaml",
            file=sys.stderr,
        )
        print(f"error: {error}", file=sys.stderr)
        return 2

    errors = 0
    for document in DOCUMENTS:
        relative = document.relative_to(ROOT).as_posix()
        try:
            spec, base_uri = read_from_filename(str(document))
            validate(spec, base_uri=base_uri)
        except Exception as error:  # noqa: BLE001 - the validator raises many types
            print(f"ERROR: {relative}: {error}", file=sys.stderr)
            errors += 1
            continue
        version = spec.get("openapi", "")
        if not version.startswith("3.1"):
            print(
                f"ERROR: {relative}: OpenAPI 3.1 is the source of truth (ADR-003), "
                f"found {version!r}",
                file=sys.stderr,
            )
            errors += 1
            continue
        print(f"{relative}: valid OpenAPI {version}")
        # CR-18: every object-type schema in components.schemas must explicitly declare additionalProperties
        # Intentional free-form objects (e.g., details/metadata with additionalProperties: true) are allowed;
        # missing declaration is not allowed.
        schemas = (spec.get("components") or {}).get("schemas") or {}
        if isinstance(schemas, dict):
            missing = []
            for name, schema in schemas.items():
                if not isinstance(schema, dict):
                    continue
                typ = schema.get("type")
                is_object = typ == "object" or (isinstance(typ, list) and "object" in typ)
                if is_object and "additionalProperties" not in schema:
                    print(
                        f"ERROR: {relative}: schema {name} is object type but missing additionalProperties (must be explicit true/false) [CR-18]",
                        file=sys.stderr,
                    )
                    errors += 1
                    missing.append(name)
            if not missing:
                obj_count = sum(
                    1
                    for s in schemas.values()
                    if isinstance(s, dict)
                    and (s.get("type") == "object" or (isinstance(s.get("type"), list) and "object" in s.get("type")))
                )
                print(f"  additionalProperties: OK ({obj_count} object schemas all explicit)")

    for registry in REGISTRIES:
        relative = registry.relative_to(ROOT).as_posix()
        try:
            content = yaml.safe_load(registry.read_text(encoding="utf-8"))
        except Exception as error:  # noqa: BLE001 - YAML raises many types
            print(f"ERROR: {relative}: {error}", file=sys.stderr)
            errors += 1
            continue
        entries = content.get("errors") if isinstance(content, dict) else None
        if not isinstance(entries, list) or not entries:
            print(f"ERROR: {relative}: `errors` must be a non-empty list", file=sys.stderr)
            errors += 1
            continue
        for entry in entries:
            missing = {"code", "status", "retryable"} - set(entry)
            if missing:
                print(
                    f"ERROR: {relative}: entry {entry!r} is missing "
                    + ", ".join(sorted(missing)),
                    file=sys.stderr,
                )
                errors += 1
        print(f"{relative}: {len(entries)} error codes")

    print(f"errors={errors}")
    if errors:
        return 1
    print("OrbiSync OpenAPI validation passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
