#!/usr/bin/env python3
"""Gate that every scripts gate is wired in CI.

Enumerates:
  scripts/check_*.py / scripts/test_*.py / scripts/validate_*.py
and checks that each is referenced from at least one workflow file
under .github/workflows/*.yml (including reusable workflows).

If any script is not referenced -> red.
Intentionally unwired scripts must be in ALLOWLIST with a reason and
"# expires = YYYY-MM-DD". Expired entries are red.

Zero-scan guards ensure the gate does not silently pass when file
selection is broken.

Usage:
    python scripts/check_ci_gate_coverage.py [--json] [--root PATH]
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS_DIR = ROOT / "scripts"
WORKFLOWS_DIR = ROOT / ".github" / "workflows"

# Allowlist for scripts intentionally not run in CI.
# Key: relative path "scripts/xxx.py"
# Value: comment containing reason and "# expires = YYYY-MM-DD" (required, must be future)
# Example: "scripts/foo.py": "temporarily not wired – reason # expires = 2026-12-31"
ALLOWLIST: dict[str, str] = {
    # No entries: all gates must be wired. Add with expiry if needed.
}

EXPIRES_RE = re.compile(r"#\s*expires\s*=\s*(\d{4}-\d{2}-\d{2})", re.IGNORECASE)


def collect_gate_scripts(root: Path = ROOT) -> list[Path]:
    scripts_dir = root / "scripts"
    patterns = ["check_*.py", "test_*.py", "validate_*.py"]
    # Exclude this gate's self-test? No, include everything matching pattern.
    found: list[Path] = []
    for pat in patterns:
        found.extend(scripts_dir.glob(pat))
    # Filter to files only
    files = [p for p in found if p.is_file()]
    return sorted(files)


def collect_workflow_files(root: Path = ROOT) -> list[Path]:
    wf_dir = root / ".github" / "workflows"
    if not wf_dir.exists():
        return []
    ymls = sorted(wf_dir.glob("*.yml"))
    yamls = sorted(wf_dir.glob("*.yaml"))
    # yaml alias is not used but include
    all_files = sorted(set(ymls + yamls))
    return [p for p in all_files if p.is_file()]


def collect_workflow_texts(root: Path = ROOT) -> dict[Path, str]:
    result: dict[Path, str] = {}
    for path in collect_workflow_files(root):
        try:
            result[path] = path.read_text(encoding="utf-8")
        except OSError:
            result[path] = ""
    return result


def is_script_covered(script_path: Path, workflow_texts: dict[Path, str]) -> bool:
    name = script_path.name
    posix = f"scripts/{name}"
    # Also check full relative path from ROOT
    rel = script_path.relative_to(ROOT).as_posix() if ROOT in script_path.parents or script_path == ROOT else posix
    for text in workflow_texts.values():
        if name in text or posix in text or rel in text:
            return True
    return False


def parse_expires(value: str) -> datetime.date | None:
    m = EXPIRES_RE.search(value)
    if not m:
        return None
    try:
        return datetime.date.fromisoformat(m.group(1))
    except ValueError:
        return None


def check_coverage(root: Path = ROOT) -> tuple[list[str], dict]:
    scripts = collect_gate_scripts(root)
    workflow_files = collect_workflow_files(root)
    workflow_texts = collect_workflow_texts(root)

    info = {
        "scripts": len(scripts),
        "workflows": len(workflow_files),
        "script_list": [p.relative_to(root).as_posix() for p in scripts],
        "workflow_list": [p.relative_to(root).as_posix() for p in workflow_files],
    }

    errors: list[str] = []
    zero: list[str] = []

    if len(scripts) == 0:
        zero.append(
            "gate coverage: no gate scripts were scanned - expected >0 scripts (check collect_gate_scripts path logic)"
        )
    if len(workflow_files) == 0:
        zero.append(
            "gate coverage: no workflow files were scanned - expected >0 workflows (check collect_workflow_files path logic)"
        )
    if zero:
        return zero, info

    # Check each script for coverage or allowlist
    today = datetime.date.today()
    for script_path in scripts:
        rel = script_path.relative_to(root).as_posix()
        covered = is_script_covered(script_path, workflow_texts)
        if covered:
            # Still validate allowlist expiry if present but not required to be red
            if rel in ALLOWLIST:
                entry = ALLOWLIST[rel]
                expires = parse_expires(entry)
                if expires is None:
                    errors.append(
                        f"{rel} is allowlisted but missing '# expires = YYYY-MM-DD' - entry: {entry!r}"
                    )
                elif expires < today:
                    errors.append(
                        f"{rel} allowlist expired {expires.isoformat()} < today {today.isoformat()} - entry: {entry!r}"
                    )
            continue

        # Not covered
        if rel in ALLOWLIST:
            entry = ALLOWLIST[rel]
            expires = parse_expires(entry)
            if expires is None:
                errors.append(
                    f"{rel} is not wired in any workflow and allowlist entry is missing '# expires = YYYY-MM-DD' - entry: {entry!r}"
                )
            elif expires < today:
                errors.append(
                    f"{rel} is not wired in any workflow and allowlist expired {expires.isoformat()} < today {today.isoformat()} - entry: {entry!r}"
                )
            else:
                # Allowlisted and not expired -> intentionally not wired, pass
                pass
        else:
            errors.append(
                f"{rel} is not referenced from any workflow in .github/workflows/*.yml - wire it in CI or add to ALLOWLIST with '# expires = YYYY-MM-DD'"
            )

    # Validate allowlist entries that refer to non-existent scripts or have bad format
    for rel, entry in ALLOWLIST.items():
        # Must have expires
        expires = parse_expires(entry)
        if expires is None:
            if rel not in [s.relative_to(root).as_posix() for s in scripts]:
                # Still report missing expires even if script doesn't exist
                errors.append(f"ALLOWLIST {rel} missing '# expires = YYYY-MM-DD' - entry: {entry!r}")
            else:
                # Already handled above; but if script is covered, we still need to report
                if f"{rel} allowlist expired" not in " ".join(errors) and f"{rel} is allowlisted but missing" not in " ".join(errors):
                    # Avoid duplicate if already added
                    if not any(rel in e and "missing '# expires" in e for e in errors):
                        errors.append(f"ALLOWLIST {rel} missing '# expires = YYYY-MM-DD' - entry: {entry!r}")
            continue
        if expires < today:
            # Already reported above for each script; but if script is covered, need to report expiry
            if not any(rel in e and "expired" in e for e in errors):
                errors.append(f"ALLOWLIST {rel} expired {expires.isoformat()} < today {today.isoformat()} - entry: {entry!r}")
        # Check reason non-empty (before expires)
        # Ensure entry has some reason text besides expires comment
        reason_part = entry.split("#")[0].strip()
        if not reason_part and rel not in errors:
            # If entry is only expires comment, require reason
            # But we allow empty reason as long as expires present? Require at least some text
            # Enforce non-empty reason implicitly by requiring entry length > expires match
            if len(entry.strip()) <= len(f"# expires = {expires.isoformat()}"):
                errors.append(f"ALLOWLIST {rel} must have a reason before '# expires = YYYY-MM-DD' - entry: {entry!r}")

        # If allowlist references a script that doesn't match pattern (not in enumeration), still check but don't require wiring
        # If script not found at all, warn
        script_path = root / rel
        if not script_path.exists():
            errors.append(f"ALLOWLIST {rel} refers to non-existent file")

    return errors, info


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()
    root = args.root.resolve()

    scripts = collect_gate_scripts(root)
    workflows = collect_workflow_files(root)

    print(f"gate coverage scan: {len(scripts)} gate scripts, {len(workflows)} workflow files")

    errors, info = check_coverage(root)

    if args.json:
        print(json.dumps({"info": info, "errors": errors}, indent=2, ensure_ascii=True))
    else:
        for k in info.get("script_list", []):
            # verbose list for debugging? Keep short
            pass
        # Print coverage details
        if info.get("scripts", 0) == 0 or info.get("workflows", 0) == 0:
            for e in errors:
                print(f"ERROR: {e}", file=sys.stderr)
            return 1

        # Show uncovered for human
        if errors:
            for e in errors:
                print(f"ERROR: {e}", file=sys.stderr)
            print(f"errors={len(errors)}", file=sys.stderr)
            return 1

    if errors:
        # json already printed; for non-json ensure stderr
        if not args.json:
            print(f"errors={len(errors)}", file=sys.stderr)
        return 1

    print("All gate scripts are wired in CI.")
    print("errors=0")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
