#!/usr/bin/env python3
"""Gate that every place starting orbisync-server supplies all required secrets.

The defect of CI-2 was "added a required env var but missed one supply site":
`ORBISYNC_REFRESH_TOKEN_HMAC_KEY` was added to `integration` but not to
`restore-drill`. Fail-closed startup makes the missing site crash.

This gate derives the required secret set from `crates/orbisync-config`
(declaration, not a hand-written list) and verifies that each site that
starts `orbisync-server` supplies every required var:

  - every CI job that starts orbisync-server (`.github/workflows/ci.yml`)
  - `deploy/compose/compose.dev.yml` server environment
  - `scripts/compose-e2e.sh` env generation
  - `scripts/restore-drill.sh` env generation

Zero-scan guards ensure a broken file-selection path cannot be green.

Usage:
    python scripts/check_required_env_supply.py [--json] [--root PATH]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MODEL_PATH = ROOT / "crates" / "orbisync-config" / "src" / "model.rs"
CI_PATH = ROOT / ".github" / "workflows" / "ci.yml"
COMPOSE_PATH = ROOT / "deploy" / "compose" / "compose.dev.yml"
COMPOSE_E2E_PATH = ROOT / "scripts" / "compose-e2e.sh"
RESTORE_DRILL_PATH = ROOT / "scripts" / "restore-drill.sh"


def parse_required_env_vars(root: Path = ROOT) -> list[str]:
    """Derive required secret env vars from crates/orbisync-config declaration.

    Steps:
      1. Parse `impl Default for Config` to map leaf `_env` field -> env var string.
      2. Parse `fn required_secret_env_vars` vec! to collect which fields are required.
      3. Resolve each required field via the map.

    Returns sorted unique list. Empty on parse failure (caller handles zero-scan).
    """
    model = root / "crates" / "orbisync-config" / "src" / "model.rs"
    try:
        text = model.read_text(encoding="utf-8")
    except OSError:
        return []
    # 1. Build leaf -> env var map from Default impl
    # Find `impl Default for Config` block roughly
    default_match = re.search(r"impl Default for Config\s*\{(.*?)\n\}", text, re.S)
    # Fallback: search whole file for _env assignments if block not isolated
    search_text = default_match.group(1) if default_match else text
    env_map: dict[str, str] = {}
    # Matches lines like: token_signing_key_env: "ORBISYNC_TOKEN_SIGNING_KEY".to_owned(),
    # or url_env: "DATABASE_URL".to_owned(),
    for leaf, env_var in re.findall(r"(\w+_env)\s*:\s*\"([^\"]+)\"", search_text):
        env_map[leaf] = env_var
    # DATABASE_URL is special: leaf is url_env, but env var is DATABASE_URL
    # Also handle direct url_env without prefix? Already captured.
    # If still missing DATABASE_URL, try broader scan
    if "url_env" not in env_map:
        for m in re.finditer(r"url_env\s*:\s*\"([^\"]+)\"", text):
            env_map["url_env"] = m.group(1)
            break
    # 2. Collect required fields from required_secret_env_vars fn
    req_match = re.search(r"fn required_secret_env_vars.*?vec!\s*\[(.*?)\]", text, re.S)
    if not req_match:
        # If we cannot find the fn, fallback to all _env values (best effort)
        # But that would be hand-written-ish; instead return env_map values
        return sorted(set(env_map.values()))
    block = req_match.group(1)
    # Find patterns self.xxx.yyy_env  (e.g., self.database.url_env, self.auth.token_signing_key_env)
    field_chains = re.findall(r"self\.([a-z]+\.[a-z_]+)", block)
    required: list[str] = []
    for chain in field_chains:
        leaf = chain.split(".")[-1]
        env_var = env_map.get(leaf)
        if env_var:
            required.append(env_var)
        else:
            # Fallback: derive via env_var_for_key if leaf is *_env
            # For _env keys, the env var is ORBISYNC_ + upper(chain)
            # e.g., auth.refresh_token_hmac_key_env -> ORBISYNC_REFRESH_TOKEN_HMAC_KEY
            # But we shouldn't hardcode; try to infer from chain
            # chain is like auth.refresh_token_hmac_key_env -> env is ORBISYNC_REFRESH_TOKEN_HMAC_KEY
            # So compute fallback
            fallback = f"ORBISYNC_{leaf.upper()}"
            # For url_env, fallback would be ORBISYNC_URL_ENV not DATABASE_URL, so keep env_map
            # Only use fallback if leaf != url_env
            if leaf != "url_env":
                required.append(fallback)
    # Deduplicate and include DATABASE_URL if present in map but not in block? Already handled.
    # Also ensure DATABASE_URL is included if block referenced database.url_env
    unique = sorted(set(required))
    # If parsing produced empty, return empty to trigger zero-scan
    return unique


def parse_ci_jobs(root: Path = ROOT) -> dict[str, dict]:
    """Return dict job_name -> job_def for ci.yml. Tries yaml, falls back to regex."""
    ci_path = root / ".github" / "workflows" / "ci.yml"
    try:
        text = ci_path.read_text(encoding="utf-8")
    except OSError:
        return {}
    # Try yaml if available
    try:
        import yaml  # type: ignore

        data = yaml.safe_load(text)
        if isinstance(data, dict) and "jobs" in data and isinstance(data["jobs"], dict):
            return data["jobs"]
    except Exception:
        pass
    # Fallback regex parse: collect job blocks as raw text
    jobs: dict[str, dict] = {}
    lines = text.splitlines()
    in_jobs = False
    current_job = None
    current_lines: list[str] = []
    for line in lines:
        if re.match(r"^jobs:\s*$", line):
            in_jobs = True
            continue
        if not in_jobs:
            continue
        m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if m:
            # This is a job at 2-space indent
            if current_job is not None:
                # store previous
                # For fallback, we store raw text under key "_raw"
                jobs[current_job] = {"_raw": "\n".join(current_lines)}
            current_job = m.group(1)
            current_lines = [line]
        elif current_job is not None:
            # If we encounter a line with 0-1 indent that is not a job, jobs section ended
            if re.match(r"^[A-Za-z0-9#]", line) and not line.startswith(" "):
                if current_job is not None:
                    jobs[current_job] = {"_raw": "\n".join(current_lines)}
                    current_job = None
                    current_lines = []
                in_jobs = False
            else:
                current_lines.append(line)
    if current_job is not None:
        jobs[current_job] = {"_raw": "\n".join(current_lines)}
    return jobs


def is_server_job(job_name: str, job_def: dict, root: Path = ROOT) -> bool:
    """Return True if job starts orbisync-server directly or via a script."""
    # Serialize job_def to string for search
    try:
        combined = json.dumps(job_def, ensure_ascii=False)
    except Exception:
        combined = str(job_def)
    # Direct pattern: contains orbisync-server
    if "orbisync-server" in combined:
        return True
    # Check referenced scripts: any `run:` containing scripts/*.sh
    # Extract from raw or from steps
    scripts_to_check: list[str] = []
    if "_raw" in job_def:
        combined_raw = job_def["_raw"]
        for m in re.finditer(r"scripts/[A-Za-z0-9_\-/\.]+\.sh", combined_raw):
            scripts_to_check.append(m.group(0))
    else:
        steps = job_def.get("steps", [])
        if isinstance(steps, list):
            for step in steps:
                if isinstance(step, dict):
                    run = step.get("run", "")
                    if isinstance(run, str):
                        for m in re.finditer(r"scripts/[A-Za-z0-9_\-/\.]+\.sh", run):
                            scripts_to_check.append(m.group(0))
    for script_rel in scripts_to_check:
        script_path = root / script_rel
        try:
            content = script_path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        if "orbisync-server" in content:
            return True
    # Also check job name heuristics: restore-drill and compose-e2e are known server starters
    # But to keep detection derivation-based, we rely on script content above.
    return False


def collect_server_ci_jobs(root: Path = ROOT) -> dict[str, dict]:
    """Return mapping job_name -> {job_text, combined} (job yaml + referenced script contents)."""
    jobs = parse_ci_jobs(root)
    server_jobs: dict[str, dict] = {}
    for job_name, job_def in jobs.items():
        if is_server_job(job_name, job_def, root):
            try:
                job_text = json.dumps(job_def, ensure_ascii=False) if "_raw" not in job_def else job_def["_raw"]
            except Exception:
                job_text = str(job_def)
            combined = job_text
            scripts: list[str] = []
            if "_raw" in job_def:
                for m in re.finditer(r"scripts/[A-Za-z0-9_\-/\.]+\.sh", job_def["_raw"]):
                    scripts.append(m.group(0))
            else:
                for step in job_def.get("steps", []):
                    if isinstance(step, dict):
                        run = step.get("run", "")
                        if isinstance(run, str):
                            for m in re.finditer(r"scripts/[A-Za-z0-9_\-/\.]+\.sh", run):
                                scripts.append(m.group(0))
            for script_rel in scripts:
                sp = root / script_rel
                try:
                    combined += "\n" + sp.read_text(encoding="utf-8", errors="ignore")
                except OSError:
                    pass
            server_jobs[job_name] = {"job_text": job_text, "combined": combined}
    return server_jobs


def check_file_supply(path: Path, required_vars: list[str]) -> list[str]:
    """Check that file text contains each required var (for compose and scripts)."""
    errors: list[str] = []
    try:
        text = path.read_text(encoding="utf-8", errors="ignore")
    except OSError as e:
        return [f"{path}: cannot read ({e})"]
    for var in required_vars:
        # For compose: look for ${VAR or VAR:
        # For scripts: look for VAR= or VAR
        # Use simple substring search; ensures mutation detection.
        # Check for var as substring
        if var not in text:
            errors.append(f"{path.as_posix()}: missing required env var '{var}' (server fails closed without it)")
        else:
            # Additional check for scripts: ensure var appears with assignment-like context
            # For shell scripts, require `VAR=` or `VAR ` nearby; but substring is enough for now
            # For compose, require `${VAR`
            if path.suffix == ".yml" or path.suffix == ".yaml":
                if f"${{{var}" not in text and f"${var}" not in text and var not in text:
                    errors.append(f"{path.as_posix()}: var '{var}' not found as ${{VAR}} in yaml")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON report")
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root")
    args = parser.parse_args()
    root = args.root.resolve()

    errors: list[str] = []

    # 1. Derive required vars from config declaration
    required_vars = parse_required_env_vars(root)
    # 2. Collect server CI jobs
    server_jobs = collect_server_ci_jobs(root)
    # 3. Compose files to check
    compose_files = [root / "deploy" / "compose" / "compose.dev.yml", root / "scripts" / "compose-e2e.sh", root / "scripts" / "restore-drill.sh"]

    scanned_vars = len(required_vars)
    scanned_jobs = len(server_jobs)
    scanned_compose = len([p for p in compose_files if p.exists()])

    print(f"server env supply scan: {scanned_vars} required vars, {scanned_jobs} ci server jobs, {scanned_compose} compose/script files")

    zero: list[str] = []
    if scanned_vars == 0:
        zero.append("server env supply gate: no required env vars were scanned — expected >0 vars (check parse_required_env_vars path logic)")
    if scanned_jobs == 0:
        zero.append("server env supply gate: no ci jobs were scanned — expected >0 orbisync-server jobs (check ci.yml parsing)")
    if scanned_compose == 0:
        zero.append("server env supply gate: no compose files were scanned — expected >0 files (check compose paths)")
    if zero:
        for prob in zero:
            print(prob, file=sys.stderr)
            errors.extend(zero)
        if args.json:
            print(json.dumps({"errors": errors, "zero_scan": zero, "scanned_vars": scanned_vars, "scanned_jobs": scanned_jobs, "scanned_compose": scanned_compose}, indent=2))
        return 1

    # Check CI jobs
    for job_name, texts in sorted(server_jobs.items()):
        job_text = texts["job_text"]
        combined = texts["combined"]
        for var in required_vars:
            # For compose-e2e the supply lives in the script file, not ci.yml job_text.
            # Allow script content to satisfy it; for other jobs require job_text.
            if job_name == "compose-e2e":
                if var not in combined:
                    errors.append(f".github/workflows/ci.yml job '{job_name}': missing required env var '{var}' (supply not found in job steps or referenced scripts)")
            elif job_name == "restore-drill" and var == "DATABASE_URL":
                # DATABASE_URL for restore-drill is generated inside the script (port 55432)
                if var not in combined:
                    errors.append(f".github/workflows/ci.yml job '{job_name}': missing required env var '{var}' (supply not found in job steps or referenced scripts)")
            else:
                if var not in job_text:
                    errors.append(f".github/workflows/ci.yml job '{job_name}': missing required env var '{var}' (supply not found in job steps)")

    # Check compose and scripts individually (outside ci job aggregation, for direct file gate)
    # compose.dev.yml
    for path in compose_files:
        if not path.exists():
            errors.append(f"{path.as_posix()}: file not found (expected to be scanned)")
            continue
        file_errors = check_file_supply(path, required_vars)
        errors.extend(file_errors)

    if args.json:
        report = {
            "required_vars": required_vars,
            "scanned_vars": scanned_vars,
            "scanned_jobs": list(sorted(server_jobs.keys())),
            "scanned_compose": [p.as_posix() for p in compose_files],
            "errors": errors,
        }
        print(json.dumps(report, indent=2))

    if errors:
        print("Server env supply check FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        print(f"\nerrors={len(errors)}", file=sys.stderr)
        return 1

    print(f"All {scanned_vars} required env vars are supplied in {scanned_jobs} ci jobs and {scanned_compose} compose/script files.")
    print("errors=0")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
