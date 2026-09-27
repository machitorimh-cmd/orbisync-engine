#!/usr/bin/env python3
"""Run the restore drill and append a durable JSONL history record.

The runner intentionally does not schedule itself. Use cron, a systemd timer,
or the platform scheduler and point ``--history`` at persistent operator
storage. GitHub Actions scheduling is not required.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Sequence


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_HISTORY = ROOT / "var" / "restore-verification" / "history.jsonl"
DEFAULT_DRILL = ROOT / "scripts" / "restore-drill.sh"
BACKUP_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/@+\-]{0,199}$")


def utc_now() -> str:
    """Return a stable RFC 3339 UTC timestamp."""

    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace(
        "+00:00", "Z"
    )


@dataclass(frozen=True)
class VerificationRecord:
    schema_version: int
    backup_id: str
    verification_mode: str
    started_at: str
    completed_at: str
    duration_ms: int
    status: str
    exit_code: int


def append_record(path: Path, record: VerificationRecord) -> None:
    """Append one compact record using a single OS write."""

    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    payload = (json.dumps(asdict(record), separators=(",", ":")) + "\n").encode(
        "utf-8"
    )
    flags = os.O_APPEND | os.O_CREAT | os.O_WRONLY
    fd = os.open(path, flags, 0o600)
    try:
        written = os.write(fd, payload)
        if written != len(payload):
            raise OSError("short write while appending restore verification history")
        os.fsync(fd)
    finally:
        os.close(fd)


def validate_inputs(args: argparse.Namespace) -> None:
    if not BACKUP_ID.fullmatch(args.backup_id):
        raise ValueError(
            "backup ID must be 1-200 characters using letters, digits, '.', '_', ':', '/', '@', '+', or '-'"
        )
    if args.backup_sha256 and not re.fullmatch(r"[0-9a-fA-F]{64}", args.backup_sha256):
        raise ValueError("backup SHA-256 must contain exactly 64 hexadecimal characters")
    if args.backup_file:
        if not args.backup_file.is_file():
            raise ValueError(f"backup file does not exist: {args.backup_file}")
        if not args.login_id or not args.password_file:
            raise ValueError(
                "external backup verification requires --login-id and --password-file"
            )
        if not args.password_file.is_file():
            raise ValueError(f"password file does not exist: {args.password_file}")
    elif args.backup_sha256 or args.login_id or args.password_file:
        raise ValueError(
            "--backup-sha256, --login-id, and --password-file require --backup-file"
        )
    if not args.drill_script.is_file():
        raise ValueError(f"restore drill script does not exist: {args.drill_script}")


def run_verification(
    args: argparse.Namespace,
    command_runner: Callable[..., subprocess.CompletedProcess[bytes]] = subprocess.run,
) -> int:
    """Execute one drill, record its result, and return its exit code."""

    validate_inputs(args)
    started_at = utc_now()
    started = time.monotonic()
    env = os.environ.copy()
    env["RESTORE_DRILL_BACKUP_ID"] = args.backup_id
    mode = "external_backup" if args.backup_file else "self_contained"
    if args.backup_file:
        env["RESTORE_DRILL_BACKUP_FILE"] = str(args.backup_file.resolve())
        env["RESTORE_DRILL_LOGIN_ID"] = args.login_id
        env["RESTORE_DRILL_PASSWORD_FILE"] = str(args.password_file.resolve())
        if args.backup_sha256:
            env["RESTORE_DRILL_BACKUP_SHA256"] = args.backup_sha256.lower()

    exit_code = 127
    try:
        result = command_runner(
            [args.bash, str(args.drill_script.resolve())],
            cwd=ROOT,
            env=env,
            check=False,
        )
        exit_code = int(result.returncode)
    except KeyboardInterrupt:
        exit_code = 130
    except OSError as error:
        print(f"error: cannot start restore drill: {error}", file=sys.stderr)

    duration_ms = max(0, round((time.monotonic() - started) * 1000))
    record = VerificationRecord(
        schema_version=1,
        backup_id=args.backup_id,
        verification_mode=mode,
        started_at=started_at,
        completed_at=utc_now(),
        duration_ms=duration_ms,
        status="passed" if exit_code == 0 else "failed",
        exit_code=exit_code,
    )
    try:
        append_record(args.history, record)
    except OSError as error:
        print(f"error: cannot append restore verification history: {error}", file=sys.stderr)
        return exit_code if exit_code != 0 else 1

    print(
        f"restore verification {record.status}: backup_id={record.backup_id} "
        f"duration_ms={record.duration_ms} history={args.history}"
    )
    return exit_code


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="run scripts/restore-drill.sh and append JSONL history"
    )
    result.add_argument("--backup-id", required=True, help="non-secret backup/catalog ID")
    result.add_argument(
        "--backup-file",
        type=Path,
        help="existing custom-format pg_dump file; omit for a self-contained drill",
    )
    result.add_argument(
        "--backup-sha256", help="expected SHA-256 for --backup-file (recommended)"
    )
    result.add_argument("--login-id", help="smoke-test administrator for external backup")
    result.add_argument(
        "--password-file",
        type=Path,
        help="file containing the smoke-test password; contents are never logged",
    )
    result.add_argument(
        "--history",
        type=Path,
        default=Path(os.environ.get("RESTORE_VERIFICATION_HISTORY", DEFAULT_HISTORY)),
        help="persistent JSONL history path",
    )
    result.add_argument(
        "--drill-script", type=Path, default=DEFAULT_DRILL, help=argparse.SUPPRESS
    )
    result.add_argument(
        "--bash",
        default=os.environ.get("RESTORE_DRILL_BASH", "bash"),
        help=argparse.SUPPRESS,
    )
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        return run_verification(args)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
