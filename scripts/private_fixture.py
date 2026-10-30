"""Private fixture directories readable by PostgreSQL's restricted token."""

from contextlib import contextmanager
import csv
import os
from pathlib import Path
import re
import subprocess
import tempfile

WINDOWS = os.name == "nt"


def _grant_current_user(path):
    system = Path(os.environ["SystemRoot"]) / "System32"
    if not system.is_absolute():
        raise RuntimeError("Windows system directory must be absolute")
    result = subprocess.run([str(system / "whoami.exe"), "/user", "/fo", "csv", "/nh"],
                            capture_output=True, check=True)
    rows = list(csv.reader(result.stdout.decode("utf-8", errors="replace").splitlines()))
    sid = rows[0][-1] if len(rows) == 1 and len(rows[0]) == 2 else ""
    if not re.fullmatch(r"S-1-(?:[0-9]+-)*[0-9]+", sid):
        raise RuntimeError("Cannot determine private fixture user SID")
    subprocess.run([str(system / "icacls.exe"), str(path), "/grant:r", f"*{sid}:(OI)(CI)F"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)


@contextmanager
def private_temporary_directory(*, prefix):
    """Grant only this newly created fixture's current user explicit access.

    Python 3.12.4+ protects Windows temporary directories using OWNER RIGHTS.
    With an elevated token their owner can be Administrators. PostgreSQL drops
    that group on re-exec, so it needs the caller's user SID to remain readable.
    Existing directories and ACLs are never accepted or rewritten here.
    """
    with tempfile.TemporaryDirectory(prefix=prefix) as directory:
        if WINDOWS:
            _grant_current_user(directory)
        yield directory
