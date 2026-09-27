#!/usr/bin/env python3
"""Gate: the published OpenAPI contract must be the truth about the real router.

Hardened W-26: replaces the previous textual derivation (grepping .route("..."))
with a live probe.  The real Axum Router is built via
`orbisync_transport_http::router(HttpState::new(...))` and every operation
declared in openapi/orbisync-v1.yaml is probed with tower::ServiceExt::oneshot.
Routable means status != 404 && != 405.  401/403/400/422 all count as served.

Dead code (helper function never called, shadowed Router, cfg-gated, comment)
is not routable and therefore correctly fails.

A Rust integration test under tests/integration/tests/openapi_route_gate.rs
implements the live probe.  This Python wrapper preserves the non-live checks
(implemented + planned == 32 paths / 41 ops, both directions via live probe of
planned, method distinction) and invokes the Rust test via `cargo test` so CI
still runs `python scripts/validate_openapi_routes.py`.

Usage:
  python scripts/validate_openapi_routes.py
Exit 0 on truth, 1 on divergence.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

try:
    import yaml
except ImportError as error:
    print(
        f"ERROR: PyYAML is required to validate OpenAPI routes: {error}",
        file=sys.stderr,
    )
    raise SystemExit(1)

ROOT = Path(__file__).resolve().parents[1]
IMPLEMENTED = ROOT / "openapi" / "orbisync-v1.yaml"
PLANNED = ROOT / "openapi" / "orbisync-v1-planned.yaml"

# Original reviewed design size.  Only change this when a new operation is
# deliberately added to the design (planned -> implemented or brand new path).
# Implemented + planned must always sum to this so no design is lost.
# Original routes plus authentication, admin permission probe, diagnostics and extension commands.
ORIGINAL_PATHS = 37
ORIGINAL_OPS = 46


def load_yaml_operations(path: Path) -> set[tuple[str, str]]:
    text = path.read_text(encoding="utf-8")
    if yaml is not None:
        data = yaml.safe_load(text)
        ops: set[tuple[str, str]] = set()
        for route, methods in (data.get("paths") or {}).items():
            if not isinstance(methods, dict):
                continue
            for method in ("get", "post", "put", "patch", "delete"):
                if method in methods:
                    ops.add((method, route))
        return ops
    ops = set()
    current_path = None
    for line in text.splitlines():
        pm = re.match(r"^  (/\S+):\s*$", line)
        if pm:
            current_path = pm.group(1)
            continue
        mm = re.match(r"^    (get|post|put|patch|delete):\s*$", line)
        if current_path and mm:
            ops.add((mm.group(1), current_path))
    return ops


def load_yaml_paths(path: Path) -> set[str]:
    text = path.read_text(encoding="utf-8")
    if yaml is not None:
        data = yaml.safe_load(text)
        return set((data.get("paths") or {}).keys())
    paths = set()
    for line in text.splitlines():
        pm = re.match(r"^  (/\S+):\s*$", line)
        if pm:
            paths.add(pm.group(1))
    return paths


def find_refs(obj) -> set[str]:
    """Recursively collect $ref strings that point to #/components/."""
    refs: set[str] = set()
    if isinstance(obj, dict):
        for key, value in obj.items():
            if key == "$ref" and isinstance(value, str) and value.startswith("#/components/"):
                refs.add(value)
            else:
                refs.update(find_refs(value))
    elif isinstance(obj, list):
        for item in obj:
            refs.update(find_refs(item))
    return refs


def parse_component_ref(ref: str) -> tuple[str | None, str | None]:
    """Split '#/components/<type>/<name>' into (type, name)."""
    prefix = "#/components/"
    if not ref.startswith(prefix):
        return (None, None)
    rest = ref[len(prefix):]
    parts = rest.split("/", 1)
    if len(parts) != 2:
        return (None, None)
    return (parts[0], parts[1])


def check_component_reachability(path: Path, data: dict) -> int:
    """V-06: every declared schema / parameter must be reachable from an operation.

    Reachability is transitive via $ref.  An operation references schemas,
    parameters and responses; those components may themselves reference further
    schemas.  Anything not transitively reachable is dead declaration (usually
    leftover from W-23 duplication) and must be removed.
    Returns number of errors found.
    """
    if yaml is None:
        print(f"WARNING: pyyaml not available, skipping reachability check for {path.name}", file=sys.stderr)
        return 0
    if not isinstance(data, dict):
        return 0
    components = data.get("components") or {}
    if not isinstance(components, dict):
        return 0
    schemas = components.get("schemas") or {}
    parameters = components.get("parameters") or {}
    responses = components.get("responses") or {}
    # securitySchemes are referenced via security requirements, not $ref.
    # We check them separately but only report if they are clearly dead.
    security_schemes = components.get("securitySchemes") or {}

    # Collect initial refs from all operations (paths)
    paths = data.get("paths") or {}
    initial_refs: set[str] = set()
    security_names_used: set[str] = set()
    # global security
    for entry in data.get("security") or []:
        if isinstance(entry, dict):
            security_names_used.update(entry.keys())
    if isinstance(paths, dict):
        for path_item in paths.values():
            if not isinstance(path_item, dict):
                continue
            # path-level parameters
            if "parameters" in path_item:
                initial_refs.update(find_refs(path_item["parameters"]))
            # path-level security
            if "security" in path_item:
                for entry in path_item["security"] or []:
                    if isinstance(entry, dict):
                        security_names_used.update(entry.keys())
            for method, op in path_item.items():
                if method.startswith("x-"):
                    continue
                if method not in ("get", "post", "put", "patch", "delete"):
                    continue
                if not isinstance(op, dict):
                    continue
                initial_refs.update(find_refs(op))
                for entry in op.get("security") or []:
                    if isinstance(entry, dict):
                        security_names_used.update(entry.keys())

    # BFS over component graph
    from collections import deque

    reachable_schemas: set[str] = set()
    reachable_params: set[str] = set()
    reachable_responses: set[str] = set()
    queue: deque[tuple[str, str]] = deque()

    for ref in initial_refs:
        typ, name = parse_component_ref(ref)
        if typ == "schemas" and name in schemas and name not in reachable_schemas:
            reachable_schemas.add(name)
            queue.append(("schemas", name))
        elif typ == "parameters" and name in parameters and name not in reachable_params:
            reachable_params.add(name)
            queue.append(("parameters", name))
        elif typ == "responses" and name in responses and name not in reachable_responses:
            reachable_responses.add(name)
            queue.append(("responses", name))
        elif typ == "securitySchemes":
            # $ref to securityScheme is rare; treat as used
            pass

    # Also consider that responses/Error may be referenced via $ref to schemas inside it
    while queue:
        typ, name = queue.popleft()
        if typ == "schemas":
            obj = schemas.get(name)
        elif typ == "parameters":
            obj = parameters.get(name)
        elif typ == "responses":
            obj = responses.get(name)
        else:
            obj = None
        if obj is None:
            continue
        for ref in find_refs(obj):
            t, n = parse_component_ref(ref)
            if t == "schemas" and n in schemas and n not in reachable_schemas:
                reachable_schemas.add(n)
                queue.append((t, n))
            elif t == "parameters" and n in parameters and n not in reachable_params:
                reachable_params.add(n)
                queue.append((t, n))
            elif t == "responses" and n in responses and n not in reachable_responses:
                reachable_responses.add(n)
                queue.append((t, n))

    errors = 0
    # Schemas
    if isinstance(schemas, dict):
        unreachable_schemas = sorted(set(schemas.keys()) - reachable_schemas)
        if unreachable_schemas:
            print(
                f"ERROR: {path.relative_to(ROOT)}: unreachable schemas ({len(unreachable_schemas)}): {', '.join(unreachable_schemas)}",
                file=sys.stderr,
            )
            print(
                f"  reachable schemas ({len(reachable_schemas)}): {', '.join(sorted(reachable_schemas))}",
                file=sys.stderr,
            )
            print(
                "  HINT: remove dead schema declarations (W-23 duplicated all schemas to both files) or wire an operation that references them.",
                file=sys.stderr,
            )
            errors += 1
        else:
            print(f"  schema reachability: OK ({len(reachable_schemas)}/{len(schemas)} schemas reachable)")
    # Parameters
    if isinstance(parameters, dict):
        unreachable_params = sorted(set(parameters.keys()) - reachable_params)
        if unreachable_params:
            print(
                f"ERROR: {path.relative_to(ROOT)}: unreachable parameters ({len(unreachable_params)}): {', '.join(unreachable_params)}",
                file=sys.stderr,
            )
            print(
                f"  reachable params ({len(reachable_params)}): {', '.join(sorted(reachable_params)) if reachable_params else '(none)'}",
                file=sys.stderr,
            )
            print(
                "  HINT: remove dead parameter declarations or wire an operation that references them.",
                file=sys.stderr,
            )
            errors += 1
        else:
            print(f"  parameter reachability: OK ({len(reachable_params)}/{len(parameters)} params reachable)")
    # SecuritySchemes (informational, not required for V-06 but helpful)
    if isinstance(security_schemes, dict) and security_schemes:
        unreachable_sec = sorted(set(security_schemes.keys()) - security_names_used)
        if unreachable_sec:
            print(
                f"ERROR: {path.relative_to(ROOT)}: unreachable securitySchemes: {', '.join(unreachable_sec)}",
                file=sys.stderr,
            )
            errors += 1

    return errors


def check_status_markers(path: Path, data: dict | None, must_be_planned: bool) -> int:
    """CR-12: enforce x-orbisync-status: planned contract.

    - implemented (must_be_planned=False): no operation may carry x-orbisync-status: planned
    - planned (must_be_planned=True): every operation must carry it
    Returns number of errors.
    """
    errors = 0
    rel = path.relative_to(ROOT).as_posix() if path.is_absolute() else path.as_posix()
    if yaml is not None and data is not None:
        paths = data.get("paths") or {}
        if not isinstance(paths, dict):
            return 0
        for route, item in paths.items():
            if not isinstance(item, dict):
                continue
            # path-level parameters/security are ignored; only method ops
            for method in ("get", "post", "put", "patch", "delete"):
                op = item.get(method)
                if not isinstance(op, dict):
                    continue
                status = op.get("x-orbisync-status")
                has_planned = status == "planned"
                if must_be_planned:
                    if not has_planned:
                        print(f"ERROR: {rel}: operation {method.upper()} {route} is missing x-orbisync-status: planned (planned contract must mark every operation)", file=sys.stderr)
                        errors += 1
                    # if value exists but not "planned", also error (already handled as missing)
                    elif status != "planned":
                        print(f"ERROR: {rel}: operation {method.upper()} {route} has unexpected x-orbisync-status: {status!r} (expected 'planned')", file=sys.stderr)
                        errors += 1
                else:
                    if has_planned:
                        print(f"ERROR: {rel}: operation {method.upper()} {route} must not have x-orbisync-status: planned (implemented contract must not contain planned marker)", file=sys.stderr)
                        errors += 1
                    elif status is not None:
                        # Any other x-orbisync-status value on implemented is also unexpected
                        print(f"ERROR: {rel}: operation {method.upper()} {route} has unexpected x-orbisync-status: {status!r} (implemented contract must not contain planned marker)", file=sys.stderr)
                        errors += 1
        if must_be_planned:
            # count ops with marker for info
            total = sum(1 for item in paths.values() if isinstance(item, dict) for m in ("get", "post", "put", "patch", "delete") if isinstance(item.get(m), dict))
            with_marker = total - errors if errors == 0 or True else 0
            if errors == 0:
                print(f"  x-orbisync-status: OK ({total} planned ops all marked planned)")
        else:
            if errors == 0:
                print(f"  x-orbisync-status: OK (no implemented operation is marked planned)")
        return errors
    # Fallback without yaml: scan text per operation
    text = path.read_text(encoding="utf-8")
    current_path = None
    current_method = None
    current_has_marker = False
    # Track per operation: (method, route, has_marker)
    ops: list[tuple[str, str, bool]] = []
    lines = text.splitlines()
    # Simple state machine: when we see a path header, reset; when we see method header, start new op; when we see x-orbisync-status: planned within op, mark.
    # We need to know when an operation ends (next method or next path). Use indentation heuristic.
    pending_op: tuple[str, str] | None = None
    has_marker_for_pending = False
    for line in lines:
        pm = re.match(r"^  (/\S+):\s*$", line)
        if pm:
            if pending_op is not None:
                ops.append((pending_op[0], pending_op[1], has_marker_for_pending))
                pending_op = None
                has_marker_for_pending = False
            current_path = pm.group(1)
            continue
        mm = re.match(r"^    (get|post|put|patch|delete):\s*$", line)
        if current_path and mm:
            if pending_op is not None:
                ops.append((pending_op[0], pending_op[1], has_marker_for_pending))
            pending_op = (mm.group(1), current_path)
            has_marker_for_pending = False
            continue
        if pending_op is not None and re.match(r"^\s+x-orbisync-status:\s*planned\s*$", line):
            has_marker_for_pending = True
    if pending_op is not None:
        ops.append((pending_op[0], pending_op[1], has_marker_for_pending))
    for method, route, has_marker in ops:
        if must_be_planned and not has_marker:
            print(f"ERROR: {rel}: operation {method.upper()} {route} is missing x-orbisync-status: planned (planned contract must mark every operation)", file=sys.stderr)
            errors += 1
        if not must_be_planned and has_marker:
            print(f"ERROR: {rel}: operation {method.upper()} {route} must not have x-orbisync-status: planned (implemented contract must not contain planned marker)", file=sys.stderr)
            errors += 1
    if errors == 0:
        if must_be_planned:
            print(f"  x-orbisync-status: OK ({len(ops)} planned ops all marked planned)")
        else:
            print(f"  x-orbisync-status: OK (no implemented operation is marked planned)")
    return errors


def run_live_probe() -> tuple[bool, str, str]:
    """Invoke the Rust live probe test. Returns (ok, stdout, stderr)."""
    cmd = ["cargo", "test", "-p", "orbisync-integration-tests", "--test", "openapi_route_gate", "--", "--nocapture"]
    try:
        result = subprocess.run(
            cmd,
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            timeout=300,
            encoding="utf-8",
            errors="replace",
        )
        return (result.returncode == 0, result.stdout, result.stderr)
    except FileNotFoundError as e:
        return (False, "", f"cargo not found: {e}")
    except subprocess.TimeoutExpired:
        return (False, "", "cargo test timed out after 300s")


def main() -> int:
    errors = 0

    if not IMPLEMENTED.exists():
        print(f"ERROR: missing {IMPLEMENTED.relative_to(ROOT)}", file=sys.stderr)
        return 1

    implemented_ops = load_yaml_operations(IMPLEMENTED)
    implemented_paths = load_yaml_paths(IMPLEMENTED)

    print(f"implemented file: {IMPLEMENTED.relative_to(ROOT)}: {len(implemented_paths)} paths, {len(implemented_ops)} ops")
    print(f"  implemented ops detail: {sorted(implemented_ops)}")

    # 3. Planned preservation checks (non-live, still required)
    if PLANNED.exists():
        planned_ops = load_yaml_operations(PLANNED)
        planned_paths = load_yaml_paths(PLANNED)
        print(f"planned file: {PLANNED.relative_to(ROOT)}: {len(planned_paths)} paths, {len(planned_ops)} ops")
        # Operation-level disjoint (paths overlap for /v1/users, /v1/worlds, /v1/instances)
        inter_ops = implemented_ops & planned_ops
        if inter_ops:
            print(f"ERROR: implemented and planned must be disjoint at operation level: ops inter={inter_ops}", file=sys.stderr)
            errors += 1
        combined_paths = implemented_paths | planned_paths
        combined_ops = implemented_ops | planned_ops
        if len(combined_paths) != ORIGINAL_PATHS:
            print(f"ERROR: implemented + planned paths = {len(combined_paths)} != original {ORIGINAL_PATHS} (design lost or duplicated)", file=sys.stderr)
            errors += 1
        if len(combined_ops) != ORIGINAL_OPS:
            print(f"ERROR: implemented + planned ops = {len(combined_ops)} != original {ORIGINAL_OPS} (design lost or duplicated)", file=sys.stderr)
            errors += 1
        print(f"combined: {len(combined_paths)} paths, {len(combined_ops)} ops (original {ORIGINAL_PATHS} paths / {ORIGINAL_OPS} ops)")
        # Do not hardcode implemented or planned counts separately — the directional
        # live probe already ensures correctness, and a fixed 8 vs 10 expectation
        # would break on every legitimate endpoint addition (W-24: 8 -> 10).
        if errors == 0:
            print("planned preservation: OK (no design deleted)")
    else:
        print(f"ERROR: {PLANNED.relative_to(ROOT)} must exist to preserve the reviewed design", file=sys.stderr)
        errors += 1

    # 4. Metrics reporting — via live probe of planned /metrics
    # We report NOT SERVED if live probe says planned /metrics is not routable (expected).
    # The Rust test already asserts planned ops not routable, which includes /metrics.
    if PLANNED.exists():
        planned_ops_check = load_yaml_operations(PLANNED)
        if ("get", "/metrics") in planned_ops_check:
            print("metrics endpoint: declared in planned (x-orbisync-status: planned) - expected NOT SERVED")
        else:
            print("WARNING: /metrics not found in planned file", file=sys.stderr)

    # 4a. CR-12: x-orbisync-status contract enforcement
    print("\n--- x-orbisync-status contract: implemented must not be planned, planned must be all planned ---")
    if yaml is not None:
        try:
            impl_status_data = yaml.safe_load(IMPLEMENTED.read_text(encoding="utf-8"))
            print(f"checking {IMPLEMENTED.relative_to(ROOT)} ...")
            errors += check_status_markers(IMPLEMENTED, impl_status_data, must_be_planned=False)
        except Exception as e:
            print(f"ERROR: failed to load {IMPLEMENTED.relative_to(ROOT)} for status check: {e}", file=sys.stderr)
            errors += 1
        if PLANNED.exists():
            try:
                planned_status_data = yaml.safe_load(PLANNED.read_text(encoding="utf-8"))
                print(f"checking {PLANNED.relative_to(ROOT)} ...")
                errors += check_status_markers(PLANNED, planned_status_data, must_be_planned=True)
            except Exception as e:
                print(f"ERROR: failed to load {PLANNED.relative_to(ROOT)} for status check: {e}", file=sys.stderr)
                errors += 1
    else:
        # pyyaml unavailable: use text scan fallback
        print(f"checking {IMPLEMENTED.relative_to(ROOT)} (text fallback) ...")
        errors += check_status_markers(IMPLEMENTED, None, must_be_planned=False)
        if PLANNED.exists():
            print(f"checking {PLANNED.relative_to(ROOT)} (text fallback) ...")
            errors += check_status_markers(PLANNED, None, must_be_planned=True)

    # 4b. V-06: schema / parameter reachability - detect leftover declarations from W-23 duplication.
    print("\n--- component reachability: every declared schema/parameter must be reachable from an operation ---")
    if yaml is not None:
        try:
            impl_data = yaml.safe_load(IMPLEMENTED.read_text(encoding="utf-8"))
            print(f"checking {IMPLEMENTED.relative_to(ROOT)} ...")
            errors += check_component_reachability(IMPLEMENTED, impl_data)
        except Exception as e:
            print(f"ERROR: failed to load {IMPLEMENTED.relative_to(ROOT)} for reachability: {e}", file=sys.stderr)
            errors += 1
        if PLANNED.exists():
            try:
                planned_data = yaml.safe_load(PLANNED.read_text(encoding="utf-8"))
                print(f"checking {PLANNED.relative_to(ROOT)} ...")
                errors += check_component_reachability(PLANNED, planned_data)
            except Exception as e:
                print(f"ERROR: failed to load {PLANNED.relative_to(ROOT)} for reachability: {e}", file=sys.stderr)
                errors += 1
    else:
        print("WARNING: pyyaml not available, skipping reachability check (install pyyaml to enforce V-06)", file=sys.stderr)

    # 5. Live probe - the hardened derivation
    print("\n--- live probe: building real Router and probing each declared operation ---")
    print("  (routable = not 404 && not 405; 401/403/400/422 count as served)")
    ok, stdout, stderr = run_live_probe()
    # Always print the Rust test output for machine-checkable summary
    if stdout:
        print(stdout)
    if stderr:
        print(stderr, file=sys.stderr)
    if not ok:
        print("ERROR: live probe failed - declared contract is not truthful (see Rust test output above)", file=sys.stderr)
        # The Rust test already prints which op failed and whether it's dead code.
        # We surface it as gate failure.
        errors += 1
        # Also hint at dead code failure
        print("HINT: If you added a .route() inside a helper function that is never called,", file=sys.stderr)
        print("      or behind cfg/comment/shadowed Router, the live probe correctly sees it as 404", file=sys.stderr)
        print("      and fails. The old grep-based gate would have been fooled.", file=sys.stderr)
    else:
        print("live probe: OK - every implemented op is routable, no planned op is routable, method distinction holds")

    if errors:
        print(f"validate_openapi_routes: FAILED with {errors} error(s)", file=sys.stderr)
        return 1
    print("validate_openapi_routes: OK - published contract is truthful and design is preserved (hardened live probe).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
