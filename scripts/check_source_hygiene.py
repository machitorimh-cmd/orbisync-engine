#!/usr/bin/env python3
"""Source hygiene gate for worker output.

Two classes of damage have reached the repository from agent workers and were
only caught by a human reading the diff. Both compile cleanly, so neither
`cargo clippy` nor `cargo test` notices them. This script turns "someone has to
spot it" into "the acceptance gate rejects it".

1. Mojibake. A worker rewrote `realtime_ws.rs` with a broken encoding: en
   dashes became U+2001 followed by `E`, and `5x5` became `5<U+00C1>E`. Inside
   string literals and comments that is invisible to the compiler.

2. Non-UTF-8 files. Anything the Rust toolchain would reject outright, caught
   here with a clearer message.

Run from the repository root:

    python scripts/check_source_hygiene.py

Exit code 0 when clean, 1 when a problem is found.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

# Extensions worth scanning: source and docs that workers edit.
SCANNED_SUFFIXES = {".rs", ".py", ".ts", ".toml", ".sql", ".proto", ".md"}

# Characters that are legitimate in this repository. Everything else in the
# Latin-1 supplement / general punctuation range is suspect.
ALLOWED = {
    "§",  # SECTION SIGN — used throughout for spec references (§3.2)
    "–",  # EN DASH — used in tracing messages
    "—",  # EM DASH
    "×",  # MULTIPLICATION SIGN — used for grid sizes (5×5)
    "→",  # RIGHTWARDS ARROW
    "…",  # HORIZONTAL ELLIPSIS
    "°",  # DEGREE SIGN
    "µ",  # MICRO SIGN
    "©",  # COPYRIGHT SIGN — README licence line
    "±",  # PLUS-MINUS SIGN — tolerance bounds (transform.rs, review docs)
    "²",  # SUPERSCRIPT TWO — complexity notation (O(N²))
}

# Forbidden runtime-unsafe pattern: env!("CARGO_MANIFEST_DIR") embeds a compile-time
# absolute path into the binary, which breaks when CARGO_TARGET_DIR is shared
# across checkouts. Use std::env::var("CARGO_MANIFEST_DIR") at runtime instead.
# Built without spelling the literal so this file does not trip its own check.
FORBIDDEN_CARGO_MANIFEST_RE = re.compile(r'env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)')

# Sequences observed from actual corruption. Each maps to what it should be,
# so the report tells the author how to repair it.
KNOWN_MOJIBAKE = [
    # Patterns are built with chr() on purpose: spelling the corrupt
    # characters literally would make this file trip its own check.
    (
        re.compile(chr(0x2001) + "(?=[A-Za-z])"),
        "U+2001 before a letter",
        "en dash followed by a space",
    ),
    (
        re.compile(chr(0x00C1) + "E"),
        "U+00C1 followed by E",
        "a multiplication-sign sequence such as 5x5",
    ),
    (
        re.compile(chr(0xFFFD)),
        "U+FFFD replacement character",
        "the original character",
    ),
    (
        re.compile(chr(0x00E3) + chr(0x0081)),
        "UTF-8 Japanese read as Latin-1",
        "the original Japanese text",
    ),
    (
        re.compile(chr(0x00E2) + chr(0x0080)),
        "UTF-8 punctuation read as Latin-1",
        "the original punctuation",
    ),
]


def tracked_files() -> list[Path]:
    """Returns repository-tracked files with a scanned suffix."""
    result = subprocess.run(
        ["git", "ls-files"], capture_output=True, text=True, check=True
    )
    return [
        Path(line)
        for line in result.stdout.splitlines()
        if Path(line).suffix in SCANNED_SUFFIXES
    ]


# External review documents are exempt from the hygiene check. They are
# historical records imported verbatim; rewriting smart quotes etc. to
# satisfy the gate would destroy the record. The gate's purpose is to
# prevent homoglyph/mojibake in source and design docs, not to rewrite
# external review prose. Only docs/reviews/ is exempt — docs/design/ etc.
# remain scanned.
REVIEW_EXEMPT_PREFIX = "docs/reviews/"


def manifest_check_files() -> list[Path]:
    """Returns Rust files to scan for forbidden CARGO_MANIFEST_DIR usage."""
    result = subprocess.run(
        ["git", "ls-files"], capture_output=True, text=True, check=True
    )
    return [Path(line) for line in result.stdout.splitlines() if Path(line).suffix == ".rs"]


def check_cargo_manifest(path: Path) -> list[str]:
    """Returns problems for forbidden env!(CARGO_MANIFEST_DIR) in path."""
    problems: list[str] = []
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError as error:
        # Already reported by check(), but surface here too for manifest scan.
        return [f"{path}: not valid UTF-8 ({error})"]
    except OSError as error:
        return [f"{path}: cannot read ({error})"]
    for line_number, line in enumerate(text.splitlines(), start=1):
        if FORBIDDEN_CARGO_MANIFEST_RE.search(line):
            problems.append(
                f"{path}:{line_number}: forbidden env!(\"CARGO_MANIFEST_DIR\") — "
                f"use std::env::var(\"CARGO_MANIFEST_DIR\") at runtime"
            )
    return problems


def check(path: Path) -> list[str]:
    """Returns a list of human-readable problems found in `path`."""
    problems: list[str] = []
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        return [f"{path}: not valid UTF-8 ({error})"]

    for line_number, line in enumerate(text.splitlines(), start=1):
        for pattern, what, expected in KNOWN_MOJIBAKE:
            if pattern.search(line):
                problems.append(
                    f"{path}:{line_number}: {what} — expected {expected}\n"
                    f"    {line.strip()[:100]}"
                )
        # Japanese and other scripts are fine; the narrow suspect ranges are
        # the Latin-1 supplement, general punctuation, and halfwidth katakana
        # (U+FF61-FF9F), where mojibake lands. Halfwidth katakana is the
        # result of UTF-8 bytes for characters like "§" (C2 A7) being
        # misread as Shift-JIS/cp932, whose single-byte range 0xA1-0xDF maps
        # to halfwidth katakana; this repository's Japanese text uses
        # full-width kana/kanji, so halfwidth katakana is never legitimate
        # here.
        for char in line:
            code = ord(char)
            suspect = (
                0x00A0 <= code <= 0x00FF
                or 0x2000 <= code <= 0x206F
                or 0xFF61 <= code <= 0xFF9F
            )
            if suspect and char not in ALLOWED:
                problems.append(
                    f"{path}:{line_number}: unexpected U+{code:04X} ({char!r})\n"
                    f"    {line.strip()[:100]}"
                )
    return problems



FORBIDDEN_ENCRYPTED_SECRET_RE = re.compile(r'EncryptedResponse::new.*temporary' + '_password')

def check_encrypted_response_secret(path: Path) -> list[str]:
    """AUD-C1: EncryptedResponse must not be constructed with temporary_password secret."""
    problems: list[str] = []
    try:
        text = path.read_text(encoding="utf-8")
    except:
        return []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if FORBIDDEN_ENCRYPTED_SECRET_RE.search(line):
            problems.append(
                f"{path}:{line_number}: EncryptedResponse must not contain temporary_password (AUD-C1: secrets must not be persisted) — store only non-secret data"
            )
    return problems


def main() -> int:
    # Windows consoles default to cp932 here, which cannot print the very
    # characters this script reports on.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")

    files = tracked_files()
    # Exclude external review documents (see REVIEW_EXEMPT_PREFIX comment).
    hygiene_files = [p for p in files if not p.as_posix().startswith(REVIEW_EXEMPT_PREFIX)]
    existing = [p for p in hygiene_files if p.exists()]
    problems: list[str] = []
    # Self-check: the hygiene gate must actually scan something.
    if not existing:
        problems.append(
            "source hygiene gate: no files were scanned — "
            "expected >0 files with suffix .rs/.py/.ts/.toml/.sql/.proto/.md (check tracked_files path logic)"
        )
    for path in hygiene_files:
        if not path.exists():
            continue
        problems.extend(check(path))
        problems.extend(check_encrypted_response_secret(path))

    # Forbidden CARGO_MANIFEST_DIR check (V-13 follow-up).
    manifest_files = manifest_check_files()
    manifest_existing = [p for p in manifest_files if p.exists()]
    if not manifest_existing:
        problems.append(
            "cargo manifest check: no files were scanned — "
            "expected >0 Rust files with suffix .rs (check manifest_check_files path logic)"
        )
    else:
        for path in manifest_files:
            if not path.exists():
                continue
            problems.extend(check_cargo_manifest(path))

    print(f"source hygiene scan: {len(existing)} files")
    print(f"cargo manifest check: {len(manifest_existing)} files")
    if problems:
        print("Source hygiene check FAILED:\n")
        for problem in problems:
            print(f"  {problem}")
        print(f"\nerrors={len(problems)}")
        print(
            "\nIf a character is legitimate, add it to ALLOWED in "
            "scripts/check_source_hygiene.py with a note on why."
        )
        return 1

    print("errors=0")
    print("OrbiSync source hygiene check passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
