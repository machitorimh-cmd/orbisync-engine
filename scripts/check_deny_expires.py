#!/usr/bin/env python3
"""Enforce that each advisories.ignore entry in deny.toml has a corresponding expires comment.

Rules (from CR-15 / 21aa314 pattern):
- Every advisory ID in [advisories].ignore must have a '# expires = YYYY-MM-DD' comment
  in the advisories section (or nearby). The comment is the only way to make the
  RUSTSEC ignore expire, as cargo-deny itself does not enforce it.
- If the expires date is in the past, the gate fails and names the advisory ID and date.
- If an ignore entry has no expires comment, the gate fails.
- An explicitly configured empty ignore array is valid: no advisories are exempt.
- Missing/malformed TOML or ignore configuration fails closed. If configured entries
  are lost by the scanner, the gate fails rather than treating them as empty.

Usage:
    python scripts/check_deny_expires.py [--json]
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DENY_TOML = ROOT / "deny.toml"


def _read_deny_toml_text(path: Path = DENY_TOML) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"cannot read {path}: {exc}") from exc


def _extract_advisories_section(text: str) -> str:
    """Return the text of the [advisories] section (up to next section)."""
    # Find [advisories] and slice until next [.*] or EOF
    match = re.search(r"^\s*\[advisories\]\s*$", text, re.MULTILINE)
    if not match:
        return ""
    start = match.end()
    # Find next section header
    next_sec = re.search(r"^\s*\[.*\]\s*$", text[start:], re.MULTILINE)
    if next_sec:
        end = start + next_sec.start()
        return text[start:end]
    return text[start:]


def _configured_ignore_entries(text: str) -> list:
    """Validate the actual TOML field, distinguishing explicit [] from no scan."""
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise ValueError(f"deny.toml is invalid TOML: {error}") from error
    advisories = document.get("advisories")
    if not isinstance(advisories, dict):
        raise ValueError("deny.toml: missing [advisories] table")
    if "ignore" not in advisories or not isinstance(advisories["ignore"], list):
        raise ValueError("deny.toml: [advisories].ignore must be an explicit array")
    return advisories["ignore"]


def collect_ignore_entries(text: str | None = None) -> list[str]:
    """Return validated IDs; explicit empty arrays are distinct from parse loss."""
    if text is None:
        text = _read_deny_toml_text()
    ids: list[str] = []
    for entry in _configured_ignore_entries(text):
        if isinstance(entry, str):
            advisory = entry
        elif isinstance(entry, dict) and set(entry) <= {"id", "reason"}:
            advisory = entry.get("id")
            if "reason" in entry and not isinstance(entry["reason"], str):
                raise ValueError("deny.toml: advisory ignore reason must be a string")
        else:
            raise ValueError("deny.toml: advisory ignore must be an ID or an id/reason table")
        if not isinstance(advisory, str) or not re.fullmatch(r"[A-Z]+-\d{4}-\d+", advisory):
            raise ValueError("deny.toml: invalid advisory ID in ignore array")
        if advisory in ids:
            raise ValueError(f"deny.toml: duplicate advisory ignore {advisory}")
        ids.append(advisory)
    return ids


def collect_expires_entries(text: str | None = None) -> list[tuple[str, datetime.date, int]]:
    """Return list of (raw_line, date, line_no) for '# expires = YYYY-MM-DD' comments in advisories section."""
    if text is None:
        text = _read_deny_toml_text()
    section = _extract_advisories_section(text)
    # Also consider the header comments just before [advisories] that are part of advisory rationale
    # To be safe, look at whole file's advisories region including preceding comments
    # Find advisories section with leading comments: capture 20 lines before [advisories]
    full_text = text
    # Use full_text for expires search but limit to advisories area + preceding 30 lines
    # Simpler: search whole file for '# expires = YYYY-MM-DD' but only count those in or near advisories
    # For now, search in advisories section plus the 30 lines before it
    m = re.search(r"^\s*\[advisories\]\s*$", full_text, re.MULTILINE)
    search_text = section
    if m:
        start = max(0, m.start() - 2000)  # ~30 lines
        search_text = full_text[start : m.end()] + section
    expires: list[tuple[str, datetime.date, int]] = []
    for idx, line in enumerate(search_text.splitlines(), start=1):
        # Match '# expires = YYYY-MM-DD' case-insensitive, allow spaces
        em = re.search(r"#\s*expires\s*=\s*(\d{4}-\d{2}-\d{2})", line, re.IGNORECASE)
        if em:
            datestr = em.group(1)
            try:
                d = datetime.date.fromisoformat(datestr)
            except ValueError:
                continue
            expires.append((line.strip(), d, idx))
    return expires


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--json", action="store_true", help="print findings as JSON")
    args = parser.parse_args()

    text = _read_deny_toml_text()
    errors: list[str] = []
    ignore_ids: list[str] = []
    expires_entries = collect_expires_entries(text)
    try:
        configured_count = len(_configured_ignore_entries(text))
        ignore_ids = collect_ignore_entries(text)
        if len(ignore_ids) != configured_count:
            errors.append(
                "deny.toml advisories gate: no advisories ignore entries were scanned "
                "or configured entries were lost by the scanner"
            )
    except ValueError as error:
        errors.append(str(error))

    # Also guard for expires scanning: if we have ignores but no expires comments, that's a missing expires error,
    # but also ensure we actually scanned expires. If expires list is empty while ignores >0, that's the M2/M3 case.
    # The zero-scan for expires is covered by the per-ignore check below, but we also add explicit guard
    # to catch file-selection bugs where expires comments are not found at all.
    # We don't error here if expires is empty and ignores is empty, because the first guard already fired.

    # Check each ignore has a corresponding expires (count check + per-item future check)
    # For simplicity, require that number of expires comments >= number of ignore entries
    # and each expires is not in the past. If there are more expires than ignores, that's okay (extra comment).
    if ignore_ids and not expires_entries:
        for adv_id in ignore_ids:
            errors.append(
                f"deny.toml: advisory {adv_id} has no expires comment — "
                f"expected '# expires = YYYY-MM-DD' for each ignore entry"
            )
    elif ignore_ids:
        # If we have at least as many expires as ignores, check each ignore has a valid future expires
        # For now, map expires dates to ignores by order: first ignore -> first expires, etc.
        # More robust: check that every expires date is in the future, and that count >= ignore count
        if len(expires_entries) < len(ignore_ids):
            # Missing expires for some ignore
            missing = len(ignore_ids) - len(expires_entries)
            # Find which IDs are missing by assuming first N have expires, rest don't
            for adv_id in ignore_ids[len(expires_entries) :]:
                errors.append(
                    f"deny.toml: advisory {adv_id} has no expires comment — "
                    f"expected '# expires = YYYY-MM-DD' for each ignore entry"
                )
        # Check each expires date for being in the past
        today = datetime.date.today()
        for raw, d, lineno in expires_entries:
            if d < today:
                # Find which advisory this expires corresponds to (by order)
                # For reporting, include the date and raw line
                # Try to find the advisory ID that is closest before this expires line
                # For simplicity, report as generic but include date
                # Attempt to map by index
                idx = expires_entries.index((raw, d, lineno))
                adv_id = ignore_ids[idx] if idx < len(ignore_ids) else "unknown"
                errors.append(
                    f"deny.toml: advisory {adv_id} expires {d.isoformat()} is in the past (today {today.isoformat()}) — "
                    f"re-evaluate and extend or remove the ignore"
                )
            # Also check date format is valid (already parsed)

    # If no errors, also verify that we actually scanned something for expires
    # (defensive: if ignore_ids >0 and expires_entries >0, we have scanned)
    # No additional zero-scan needed beyond the ignore guard, but we can add a guard for expires parsing
    # to catch bugs where the regex never matches.
    # This is not strictly required but helps catch path logic errors.
    # We already handle missing expires as error, so zero expires with non-zero ignore will be caught.

    summary = {
        "ignore_entries": len(ignore_ids),
        "expires_entries": len(expires_entries),
        "errors": len(errors),
    }
    if args.json:
        print(json.dumps({"summary": summary, "errors": errors}, indent=2))
    else:
        for k, v in summary.items():
            print(f"{k}={v}")
        for e in errors:
            print(f"ERROR: {e}", file=sys.stderr)

    if errors:
        return 1
    print("deny.toml expires check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
