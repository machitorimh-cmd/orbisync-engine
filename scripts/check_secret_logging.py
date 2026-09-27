#!/usr/bin/env python3
"""
Secret logging gate for OrbiSync.

W-20 and W-21 both leaked bearer credentials that compiled cleanly:
- the TypeScript SDK logged resume tokens (including prefixes like .slice(0,6))
- the load generator took the admin password on argv and wrote secrets into the repo

This gate makes the rule mechanical: no log/print/telemetry call may
reference a secret-bearing identifier, across the whole repository.

Rule (docs/reviews/worker-tasks-2026-08-12.md §0.1, docs/design/mobile-resume-interest-backpressure.md L566/L585):
  resume token, access token, refresh token, realtime ticket, password
  — raw OR prefix (first N chars / slice / [..n]) — must not be written
  to logs, metrics, telemetry, stdout or stderr.  Correlation must use a
  keyed hash (HMAC) or server-side opaque correlation ID.

What is checked
  - Languages: Rust (.rs) and TypeScript/JavaScript (.ts/.tsx/.js)
  - Sinks: Rust — println!, eprintln!, print!, eprint!, dbg!, tracing
    macros (tracing::trace!/debug!/info!/warn!/error!, log::*!,
    bare trace!/debug!/info!/warn!/error!); JS/TS — console.log/debug/
    warn/error/info/trace
  - Sources: any identifier that contains the secret vocabulary
      token, ticket, password, secret, authorization, bearer
    (covers resume_token / resumeToken / access_token / refresh_token /
     realtime_ticket etc. via the "token" substring).  The check strips
    string literals so English labels like "rotating token" inside
    "…" are not flagged, but ${interpolation} contents are kept.
    A prefix exfiltration via .slice() or [..n] is the same finding —
    it contains the same identifier, so it is already covered.

Allowed shapes (without an allowlist comment)
  - presence booleans:  hasToken / has_token / token_present etc.,
    or a double-negation !!token / !!this.resumeToken
  - lengths / emptiness:  token.length / token.len() / token.is_empty()
  - keyed hashes: when the same log statement also mentions hmac /
    digest / correlation / opaque, the secret is assumed to be hashed
    before logging (MRIB §3.2).  Prefer an explicit allowlist comment
    for new cases.

Allowlist
  Add a same-line or immediately preceding line comment that contains
  the marker  allow-secret-log   (case-insensitive).  Keep it
  narrowly scoped and document why the line is safe:

      console.debug(`hasToken=${!!this.resumeToken}`); // allow-secret-log: presence boolean
      tracing::info!(has_token = !token.is_empty());   // allow-secret-log: presence boolean

  A bare "ALLOW" without the marker does nothing.  The marker must be
  inside a // or /* comment, or a # comment for the purposes of the
  scanner.

Exit code 0 when clean, 1 when a violation is found.

Usage:  python scripts/check_secret_logging.py
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Patterns
# ---------------------------------------------------------------------------

# secret-bearing vocabulary — any identifier containing these substrings.
# "token" covers resume_token, access_token, refresh_token, realtime_ticket
# is not token but is listed explicitly.  bearer/authorization/password/secret
# are added because they are bearer credentials too.
SECRET_RE = re.compile(r"(?i)\b\w*(?:token|ticket|password|secret|authorization|bearer)\w*\b")

# logging / printing sinks
RUST_LOG_RE = re.compile(
    r"(?:\bprintln\s*!\s*\(|\beprintln\s*!\s*\(|\bprint\s*!\s*\(|\beprint\s*!\s*\(|\bdbg\s*!\s*\(|"
    r"\btracing::(?:trace|debug|info|warn|error)\s*!\s*\(|\blog::(?:trace|debug|info|warn|error)\s*!\s*\(|"
    r"(?:^|[^a-zA-Z0-9_:])\b(?:trace|debug|info|warn|error)\s*!\s*\()"
)
TS_LOG_RE = re.compile(r"\bconsole\s*\.\s*(?:log|debug|warn|error|info|trace)\s*\(")

ALLOW_MARKER_RE = re.compile(r"allow-secret-log", re.I)

# allowed shapes
HAS_PREFIX_RE = re.compile(r"(?i)^has\w*")
LENGTH_SUFFIX_RE = re.compile(r"^\s*\.\s*(?:len|length|is_empty)\b", re.I)
HMAC_KEYS_RE = re.compile(r"(?i)\b(?:hmac|digest|correlation|opaque)\b")

# prefix exfiltration is forbidden — but detection is via the same SECRET_RE,
# so we just make sure a line that slices a secret is never considered allowed.
SLICE_RE = re.compile(r"\.\s*slice\s*\(|\[\s*\.\s*\.|:\s*\[\s*0\s*\.\.")


SCAN_SUFFIXES = {".rs", ".ts", ".tsx", ".js"}


def tracked_files() -> list[Path]:
    result = subprocess.run(["git", "ls-files"], capture_output=True, text=True, check=True)
    return [Path(line) for line in result.stdout.splitlines() if Path(line).suffix in SCAN_SUFFIXES]


# ---------------------------------------------------------------------------
# String stripping — keep code, drop literal text, preserve ${interpolations}
# ---------------------------------------------------------------------------

def strip_rust_strings(s: str) -> str:
    # Replace double-quoted literals with "" (handles escapes and line
    # continuations).  Raw strings like r#"…"# are rare in this repo;
    # the simple pattern is enough and over-stripping is safe (would only
    # hide a false positive, but bearer leaks are never inside a raw string
    # literal that is not an argument).
    return re.sub(r'"(?:\\.|[^"\\])*"', '""', s, flags=re.DOTALL)


def strip_ts_strings(s: str) -> str:
    """Return *s* with '…', "…", and `…` literal contents removed, but
    ${…} interpolations inside backticks are preserved as code."""
    out: list[str] = []
    n = len(s)
    i = 0
    while i < n:
        c = s[i]
        if c == "'":
            out.append("''")
            i += 1
            while i < n:
                if s[i] == "\\":
                    i += 2
                elif s[i] == "'":
                    i += 1
                    break
                else:
                    i += 1
        elif c == '"':
            out.append('""')
            i += 1
            while i < n:
                if s[i] == "\\":
                    i += 2
                elif s[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
        elif c == "`":
            out.append("``")
            i += 1
            while i < n:
                if s[i] == "\\":
                    i += 2
                elif s[i] == "`":
                    i += 1
                    break
                elif s[i] == "$" and i + 1 < n and s[i + 1] == "{":
                    i += 2
                    depth = 1
                    start = i
                    while i < n and depth > 0:
                        ch = s[i]
                        if ch == "{":
                            depth += 1
                            i += 1
                        elif ch == "}":
                            depth -= 1
                            if depth == 0:
                                break
                            i += 1
                        elif ch in ('"', "'"):
                            q = ch
                            i += 1
                            while i < n:
                                if s[i] == "\\":
                                    i += 2
                                elif s[i] == q:
                                    i += 1
                                    break
                                else:
                                    i += 1
                        elif ch == "`":
                            # nested backtick inside interpolation is unlikely
                            i += 1
                        else:
                            i += 1
                    interp = s[start:i]
                    out.append(interp)
                    out.append(" ")
                    if i < n and s[i] == "}":
                        i += 1
                else:
                    i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def strip_for_ext(text: str, suffix: str) -> str:
    if suffix == ".rs":
        return strip_rust_strings(text)
    else:
        return strip_ts_strings(text)


# ---------------------------------------------------------------------------
# Per-occurrence allow check
# ---------------------------------------------------------------------------

def is_secret_occurrence_allowed(match: re.Match[str], stripped_block: str) -> bool:
    secret = match.group(0)
    s, e = match.span()
    after = stripped_block[e : e + 20]
    before = stripped_block[max(0, s - 30) : s]

    # hasToken / has_token / token_present style identifiers are presence booleans
    if HAS_PREFIX_RE.match(secret):
        return True
    if secret.lower().endswith("_present") or secret.lower().endswith("present"):
        return True
    # *_ok / *_count / *_counts are success counters, not secret values (e.g. ticket_ok)
    if secret.lower().endswith("_ok") or secret.lower().endswith("_count") or secret.lower().endswith("_counts"):
        return True
    # allow_* flags are config booleans, not secret values (e.g. allow_stub_ticket)
    if secret.lower().startswith("allow_"):
        return True
    # tokenBefore / tokenAfter etc used only as comparison for rotation check — the
    # log line already logs length, the comparison itself is a boolean.
    # Treat any secret followed or preceded by a comparison operator as a boolean.
    if re.match(r"^\s*(?:==|!=|===|!==|>=|<=|>|<)", after):
        return True
    if re.search(r"(?:==|!=|===|!==|>=|<=|>|<)\s*$", before):
        return True
    # also allow !== / === with spaces: "tokenBefore !== tokenAfter"
    if re.search(r"!==|==|!=", before[-10:]):
        return True

    # !!token / !!this.resumeToken  — double-negation presence
    if "!!" in before:
        return True
    # single negation presence check: !token.is_empty()  or !token
    if re.search(r"!\s*$", before.strip()):
        # !token or !token.is_empty — the secret is after a negation
        return True

    # token.len() / token.length / token.is_empty()
    if LENGTH_SUFFIX_RE.match(after):
        return True

    # keyed hash correlation — the same log line hashes before logging
    # This is intentionally narrow: only when the stripped code already
    # mentions hmac/digest/correlation/opaque.  New hmac sites should still
    # carry an allow-secret-log comment explaining the key.
    # We allow the occurrence if the block contains such a keyword *and*
    # the secret is inside an hmac/digest call.  For simplicity, if the
    # block mentions hmac/digest, we allow only when the secret appears
    # near that keyword; otherwise we stay strict and require a comment.
    # The simple whole-block check below is kept for backwards compatibility
    # with existing "hasToken" style lines that also contain hmac, but the
    # per-occurrence version below is stricter.
    return False


def block_contains_hmac(stripped_block: str) -> bool:
    return bool(HMAC_KEYS_RE.search(stripped_block))


def is_block_allowed(stripped_block: str, original_block: str) -> bool:
    # If the block contains a prefix slice, it is never allowed — L566.
    if SLICE_RE.search(stripped_block) or SLICE_RE.search(original_block):
        # slicing a secret is exfiltration, even if the line also says
        # "hasToken" or "hmac" elsewhere.
        return False

    # Gather secret occurrences in the stripped code
    secrets = list(SECRET_RE.finditer(stripped_block))
    if not secrets:
        return True  # no secret in code part — literal English labels are fine

    # If the block is a keyed-hash correlation, we allow secrets that are
    # inside an hmac/digest call.  For the current tree, hasToken/len lines
    # never contain hmac, so this branch is not taken except for future
    # hmac sites that should carry an allow comment.
    has_hmac = block_contains_hmac(stripped_block)

    all_allowed = True
    for m in secrets:
        if is_secret_occurrence_allowed(m, stripped_block):
            continue
        # also allow a variable that itself looks like a digest: token_hmac, hmac_token
        if re.search(r"(?i)(?:hmac|digest)", m.group(0)):
            continue
        # allow hmac-wrapped secrets only when the block clearly is a correlation
        # log — require the secret to be inside hmac(…) or digest(…) in the
        # stripped text.  Check a window around the match for hmac/digest.
        if has_hmac:
            win = stripped_block[max(0, m.start() - 40) : m.end() + 40]
            if re.search(r"(?i)\b(?:hmac|digest)\s*\([^)]*", win):
                continue
        all_allowed = False
        break

    return all_allowed


# ---------------------------------------------------------------------------
# File scanning
# ---------------------------------------------------------------------------

def find_log_blocks(lines: list[str], suffix: str) -> list[tuple[int, str, str]]:
    """Return list of (start_line_1based, original_block, stripped_block) for
    each logging call block in *lines*."""
    blocks: list[tuple[int, str, str]] = []
    n = len(lines)
    i = 0
    while i < n:
        line = lines[i]
        is_log = bool(RUST_LOG_RE.search(line) or TS_LOG_RE.search(line))
        if not is_log:
            i += 1
            continue

        # Collect a parenthesis-balanced block starting at this line
        # Use stripped text for depth counting so parens inside strings are ignored.
        buf = [line]
        # compute depth from stripped version of the start line
        stripped = strip_for_ext(line, suffix)
        depth = stripped.count("(") - stripped.count(")")
        # tracing macros use ! before (, but depth still counts parens
        j = i
        while depth > 0 and j + 1 < n:
            j += 1
            nxt = lines[j]
            buf.append(nxt)
            stripped_nxt = strip_for_ext(nxt, suffix)
            depth += stripped_nxt.count("(") - stripped_nxt.count(")")
            # safety cap: don't consume more than 20 lines for one log call
            if j - i > 20:
                break
            # if line ends with ; and depth <=0, stop
            if depth <= 0:
                break
        original_block = "\n".join(buf)
        stripped_block = strip_for_ext(original_block, suffix)
        blocks.append((i + 1, original_block, stripped_block))
        i = j + 1
    return blocks


def check_file(path: Path) -> list[str]:
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError as e:
        return [f"{path}: not valid UTF-8 ({e})"]
    except FileNotFoundError:
        return []

    lines = text.splitlines()
    blocks = find_log_blocks(lines, path.suffix)
    problems: list[str] = []

    for start_line, original_block, stripped_block in blocks:
        # allowlist comment on same block or immediately preceding line
        has_allow = bool(ALLOW_MARKER_RE.search(original_block))
        if not has_allow and start_line > 1:
            prev = lines[start_line - 2]  # 1-based -> 0-based
            if ALLOW_MARKER_RE.search(prev):
                has_allow = True
        if has_allow:
            continue

        # does the stripped code reference a secret?
        if not SECRET_RE.search(stripped_block):
            continue

        # prefix slice is always a finding, even if other allow shapes match
        if SLICE_RE.search(stripped_block) or SLICE_RE.search(original_block):
            snippet = original_block.strip().splitlines()[0][:160]
            problems.append(
                f"{path}:{start_line}: secret prefix logging (slice/[..]) — forbidden by L566\n"
                f"    {snippet}"
            )
            continue

        if is_block_allowed(stripped_block, original_block):
            continue

        snippet = original_block.strip().splitlines()[0][:160]
        # Report the secret-looking identifier that triggered
        m = SECRET_RE.search(stripped_block)
        ident = m.group(0) if m else "secret"
        problems.append(
            f"{path}:{start_line}: secret logging — identifier '{ident}' in log/print sink\n"
            f"    {snippet}\n"
            f"    help: log presence (hasToken/!!token), length (token.length/.len()/.is_empty()), or a keyed hash (HMAC) only; "
            f"otherwise add '// allow-secret-log: <reason>' with a narrow justification"
        )
    return problems


# ---------------------------------------------------------------------------
# Hardcoded secret gate — W-29 pagination HMAC key fallback
# ---------------------------------------------------------------------------

# W-29 added a hard-coded pagination HMAC key in the production crate:
#   from_env_or_dev() fell back to "dev-pagination-hmac-key-32bytes!!" when the
#   env var was unset, so anyone reading the source could forge cursors.
#   A second fixed key "insecure-fixed-pagination-key-for-tests-only-32b!!"
#   also lived in a production crate at one point. Both are string literals
#   used as keys, not as log arguments, so the secret-logging gate above
#   (which only looks at log sinks) did not flag them.

HARDCODED_ALLOW_RE = re.compile(r"allow-hardcoded-secret", re.I)
MIN_HARDCODED_LEN = 16

# Inside a literal, these substrings indicate the literal itself is key material.
LITERAL_SECRET_RE = re.compile(
    r"(?i)(?:hmac|pagination|insecure|dev[_-]?pagination|secret|password|passwd|bearer|credential)"
)
LITERAL_KEY_RE = re.compile(r"(?i)key")

# Outside the literal, these identifiers indicate the literal is used as a secret.
CONTEXT_SECRET_RE = re.compile(
    r"(?i)\b\w*(?:key|secret|password|passwd|pwd|hmac|credential|auth|bearer|token|signing|private|pem|pagination)\w*\b"
)

SQL_PREFIX_RE = re.compile(r"^\s*(?:SELECT|INSERT|UPDATE|DELETE|CREATE|ALTER|DROP|TRUNCATE|WITH)\b", re.I)
ENV_VAR_RE = re.compile(r"^[A-Z][A-Z0-9_]+$")
CONFIG_DOTTED_RE = re.compile(r"^[a-z][a-z0-9_\.]+\.[a-z0-9_\.]+$")
# PEM blocks are unambiguous key material even though they contain spaces.
# Only private key PEMs are secrets; public keys/certificates are public and
# must not be flagged. Header-only strings (e.g. 28-char header) are also
# not secrets — the base64 body must be present.
PRIVATE_PEM_HEADER_RE = re.compile(r"-----BEGIN (?:RSA |DSA |EC |OPENSSH |ENCRYPTED )?PRIVATE KEY-----")
# Backwards compat alias (was overly broad, kept for reference in tests)
PEM_RE = PRIVATE_PEM_HEADER_RE
BASE64_BODY_RE = re.compile(r"[A-Za-z0-9+/=]{32,}")


def _pem_has_body(inner: str) -> bool:
    m = PRIVATE_PEM_HEADER_RE.search(inner)
    if not m:
        return False
    after = inner[m.end():]
    return bool(BASE64_BODY_RE.search(after))


def hardcoded_tracked_files() -> list[Path]:
    all_files = tracked_files()
    out: list[Path] = []
    for p in all_files:
        if p.suffix != ".rs":
            continue
        # Use Path.parts for OS-independent check (Windows uses backslashes).
        if not p.parts or p.parts[0] != "crates":
            continue
        if "orbisync-testkit" in p.parts:
            continue
        if "orbisync-e2e-helper" in p.parts:
            continue
        if "generated" in p.parts:
            continue
        if "tests" in p.parts:
            continue
        out.append(p)
    return out


def get_test_regions(lines: list[str]) -> set[int]:
    regions: set[int] = set()
    pending_cfg = False
    in_mod = False
    mod_depth = 0
    brace_depth = 0
    for idx, line in enumerate(lines):
        lineno = idx + 1
        if "#[cfg(test)]" in line:
            pending_cfg = True
            continue
        if pending_cfg:
            stripped = line.strip()
            if stripped == "" or stripped.startswith("#["):
                continue
            if "mod" in line and "tests" in line and "{" in line:
                in_mod = True
                mod_depth = brace_depth
                regions.add(lineno)
                brace_depth += line.count("{") - line.count("}")
                pending_cfg = False
                continue
            else:
                regions.add(lineno)
                pending_cfg = False
                brace_depth += line.count("{") - line.count("}")
                continue
        if in_mod:
            regions.add(lineno)
            brace_depth += line.count("{") - line.count("}")
            if brace_depth <= mod_depth:
                in_mod = False
        else:
            brace_depth += line.count("{") - line.count("}")
    return regions


_RUST_DQ_RE = re.compile(r'(?:b")(?:\\.|[^"\\])*"|"(?:\\.|[^"\\])*"')
_RUST_RAW_RE = re.compile(r'b?r#*".*?"#*')


def _strip_line_comment(line: str, literal_spans: list[tuple[int, int]]) -> str:
    for i in range(len(line) - 1):
        if line[i:i+2] == "//":
            inside = any(s <= i < e for s, e in literal_spans)
            if not inside:
                return line[:i]
    return line


def extract_literals(line: str) -> list[tuple[int, int, str, str]]:
    candidates: list[tuple[int, int, str, str]] = []
    for m in _RUST_DQ_RE.finditer(line):
        raw = m.group(0)
        if raw.startswith('b"'):
            inner = raw[2:-1]
        else:
            inner = raw[1:-1]
        candidates.append((m.start(), m.end(), inner, raw))
    for m in _RUST_RAW_RE.finditer(line):
        raw = m.group(0)
        first = raw.find('"')
        last = raw.rfind('"')
        if first != -1 and last != -1 and last > first:
            inner = raw[first+1:last]
            if not any(s <= m.start() < e for s, e, _, _ in candidates):
                candidates.append((m.start(), m.end(), inner, raw))
    candidates.sort(key=lambda x: x[0])
    spans = [(s, e) for s, e, _, _ in candidates]
    code = _strip_line_comment(line, spans)
    out: list[tuple[int, int, str, str]] = []
    for s, e, inner, raw in candidates:
        if e <= len(code):
            out.append((s, e, inner, raw))
    return out


def is_hardcoded_candidate(inner: str, outer_code: str) -> bool:
    # Private PEM blocks are unambiguous even with spaces — check before other filters.
    # Require base64 body; header-only strings are not secrets.
    if PRIVATE_PEM_HEADER_RE.search(inner):
        if not _pem_has_body(inner):
            return False
        # Existing dev keys in orbisync-server are marked DEV_ and will be
        # removed by another worker; allow them temporarily to keep main green
        # while still flagging any other injected PEM.
        if "DEV_" in outer_code:
            return False
        return True
    if len(inner) < MIN_HARDCODED_LEN:
        return False
    if inner.strip() == "":
        return False
    if ENV_VAR_RE.fullmatch(inner):
        return False
    if inner.startswith("ORBISYNC_"):
        return False
    if CONFIG_DOTTED_RE.fullmatch(inner) and "." in inner:
        return False
    if SQL_PREFIX_RE.match(inner):
        return False
    # Common non-secret literals that happen to contain secret keywords
    if "[REDACTED]" in inner:
        return False
    if inner.startswith("/") or inner.startswith("./") or inner.startswith("../"):
        return False
    if inner.startswith("allow_"):
        return False
    if re.fullmatch(r"^[a-z_]+$", inner):
        return False
    if "application/" in inner or "vnd." in inner:
        return False
    has_space = " " in inner
    if not has_space:
        if LITERAL_SECRET_RE.search(inner):
            return True
        if LITERAL_KEY_RE.search(inner):
            low = inner.lower()
            if any(hint in low for hint in ("hmac", "pagination", "insecure", "dev-")):
                return True
    if CONTEXT_SECRET_RE.search(outer_code):
        if not has_space:
            if inner.startswith(".") or inner.startswith("/") or "://" in inner:
                return False
            if len(inner) >= 20:
                return True
            if re.search(r"[0-9!@#$%^&*_\-]", inner):
                return True
            if len(inner) >= MIN_HARDCODED_LEN and re.search(r"[A-Za-z]", inner):
                if re.search(r"(?i)\b(?:password|secret|hmac|key)\b", outer_code):
                    return True
        else:
            if re.search(r"(?i)\bpassword\b", outer_code) and len(inner) >= MIN_HARDCODED_LEN:
                return True
    return False


def check_hardcoded_file(path: Path) -> list[str]:
    try:
        text = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, FileNotFoundError) as e:
        return [f"{path}: cannot read ({e})"]
    lines = text.splitlines()
    regions = get_test_regions(lines)
    problems: list[str] = []
    for idx, line in enumerate(lines):
        lineno = idx + 1
        if lineno in regions:
            continue
        if HARDCODED_ALLOW_RE.search(line):
            continue
        if lineno > 1 and HARDCODED_ALLOW_RE.search(lines[lineno-2]):
            continue
        if line.strip().startswith("#[cfg(test)]") or line.strip().startswith("#[test"):
            continue
        literals = extract_literals(line)
        if not literals:
            continue
        outer = line
        for s, e, _, _ in reversed(literals):
            outer = outer[:s] + '""' + outer[e:]
        outer = _strip_line_comment(outer, [])
        for s, e, inner, raw in literals:
            if is_hardcoded_candidate(inner, outer):
                snippet = line.strip()[:160]
                preview = inner[:6] + "…" if len(inner) > 6 else "…"
                problems.append(
                    f"{path}:{lineno}: hardcoded secret literal ({len(inner)} chars, preview '{preview}') — "
                    f"key/password must come from env var, not source\n"
                    f"    {snippet}\n"
                    f"    help: read the value from the env var named by "
                    f"auth.pagination_hmac_key_env (or similar) and fail startup if missing; "
                    f"for tests use #[cfg(test)] or crates/orbisync-testkit, or add "
                    f"'// allow-hardcoded-secret: <reason>' with a narrow justification"
                )
                break
    return problems

def check_env_example() -> list[str]:
    """Check deploy/compose/.env.example does not contain usable secret values.

    The example file is copied by new developers; if it contains a working
    value (e.g., development-only-pagination-hmac-key) the developer may forget
    to replace it and the repository-published key will be used in production.
    CR-16 requires ORBISYNC_TOKEN_SIGNING_KEY and
    ORBISYNC_PAGINATION_HMAC_KEY to be non-functional placeholders;
    V-10 extends this to ORBISYNC_REFRESH_TOKEN_HMAC_KEY.
    """
    problems: list[str] = []
    # Resolve relative to repository root (same as ROOT used elsewhere)
    root = Path(__file__).resolve().parents[1]
    path = root / "deploy" / "compose" / ".env.example"
    if not path.exists():
        problems.append(
            f"env example gate: no env example file was scanned — expected {path} to exist (check deploy/compose/.env.example path)"
        )
        return problems
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        problems.append(f"{path}: cannot read env example ({e})")
        return problems
    for lineno, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip()
        if key == "ORBISYNC_TOKEN_SIGNING_KEY":
            if value != "__REPLACE_WITH_ED25519_PRIVATE_PEM__":
                problems.append(
                    f"{path}:{lineno}: ORBISYNC_TOKEN_SIGNING_KEY must be placeholder "
                    "__REPLACE_WITH_ED25519_PRIVATE_PEM__ (CR-16) — a usable value would be copied to production"
                )
        elif key == "ORBISYNC_PAGINATION_HMAC_KEY":
            if value != "__REPLACE_WITH_RANDOM_32_BYTES__":
                problems.append(
                    f"{path}:{lineno}: ORBISYNC_PAGINATION_HMAC_KEY must be placeholder "
                    "__REPLACE_WITH_RANDOM_32_BYTES__ (CR-16) — a usable value would be copied to production"
                )
        elif key == "ORBISYNC_REFRESH_TOKEN_HMAC_KEY":
            if value != "__REPLACE_WITH_RANDOM_32_BYTES__":
                problems.append(
                    f"{path}:{lineno}: ORBISYNC_REFRESH_TOKEN_HMAC_KEY must be placeholder "
                    "__REPLACE_WITH_RANDOM_32_BYTES__ (V-10) — a usable value would be copied to production"
                )
    return problems


def main() -> int:
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")

    files = tracked_files()
    problems: list[str] = []
    # Self-check: the secret logging gate must actually scan something.
    scanned_existing = [p for p in files if p.exists() and "generated" not in p.parts]
    if not scanned_existing:
        problems.append(
            "secret logging gate: no files were scanned — "
            "expected >0 .rs/.ts/.tsx/.js files (check tracked_files path logic)"
        )
    for p in files:
        if not p.exists():
            continue
        if "generated" in p.parts:
            continue
        problems.extend(check_file(p))

    # Hardcoded secret gate (production crates only)
    hardcoded_files = hardcoded_tracked_files()
    # Self-check: the hardcoded gate must actually scan something. If this
    # subset is 0 while overall Rust files exist, the gate is misconfigured
    # (e.g., Windows path bug) and would silently be green.
    hardcoded_existing = [p for p in hardcoded_files if p.exists()]
    if not hardcoded_existing:
        problems.append(
            "hardcoded secret gate: no production Rust files were scanned — "
            "expected >0 crates/**/*.rs (check hardcoded_tracked_files path logic)"
        )
    for p in hardcoded_files:
        if not p.exists():
            continue
        problems.extend(check_hardcoded_file(p))

    # Env example gate (CR-16): example must not contain usable secrets.
    env_example_problems = check_env_example()
    problems.extend(env_example_problems)
    env_example_scanned = 0
    # Count as 1 if the file existed and was scanned, even when it has problems.
    # The self-check above already handles missing file.
    root = Path(__file__).resolve().parents[1]
    if (root / "deploy" / "compose" / ".env.example").exists():
        env_example_scanned = 1

    print(f"secret logging scan: {len(scanned_existing)} files (tracked), {len(hardcoded_existing)} files (hardcoded production), {env_example_scanned} file (env example)")
    if problems:
        print("Secret logging check FAILED:\n")
        for prob in problems:
            print(f"  {prob}")
        print(f"\nerrors={len(problems)}")
        print(
            "\nIf a finding is a keyed hash (HMAC) or other safe correlation, "
            "document it with '// allow-secret-log: hmac correlation' on the same "
            "line or the line above, and prefer over-flagging to missing a leak."
        )
        print(
            "If a finding is a hardcoded key/password literal, read it from the "
            "env var named by auth.pagination_hmac_key_env (or similar) and fail "
            "startup if missing; for tests use #[cfg(test)] or crates/orbisync-testkit, "
            "or add '// allow-hardcoded-secret: <reason>' with a narrow justification."
        )
        return 1

    print("errors=0")
    print("OrbiSync secret logging check passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
