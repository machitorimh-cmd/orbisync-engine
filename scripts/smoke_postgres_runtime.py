"""Relocate and execute the bundled PostgreSQL without a system PostgreSQL."""

import argparse
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile


def smoke(runtime):
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        raise RuntimeError("PostgreSQL smoke must run as an unprivileged user")
    with tempfile.TemporaryDirectory(prefix="orbisync-pg-relocated-") as temporary:
        temporary = Path(temporary)
        moved = temporary / "runtime with spaces" / "postgres"
        shutil.copytree(runtime, moved)
        env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
        env["LD_LIBRARY_PATH"] = str(moved / "lib")
        env["PATH"] = str(moved / "bin") + os.pathsep + os.defpath
        suffix = ".exe" if os.name == "nt" else ""
        def execute(program, *arguments):
            # pg_ctl launches a long-lived Windows descendant. Capturing its
            # pipes can keep communicate() waiting for inherited handles even
            # after pg_ctl itself has exited. Only query/version output is read.
            capture = program in ("postgres", "psql")
            result = subprocess.run([str(moved / "bin" / (program + suffix)), *map(str, arguments)],
                                    check=True, stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
                                    stderr=subprocess.PIPE if capture else subprocess.DEVNULL,
                                    text=True, env=env, timeout=60)
            return (result.stdout or "").strip()
        assert "17.11" in execute("postgres", "--version")
        data = temporary / "data with spaces"
        execute("initdb", "-D", data, "-L", moved / "share", "--locale=C", "--encoding=UTF8",
                "--username=runtime_smoke", "--auth=trust", "--no-instructions")
        with socket.socket() as free_port:
            free_port.bind(("127.0.0.1", 0))
            port = free_port.getsockname()[1]
        # Loopback TCP only; no global socket path, service registration, or global environment changes.
        (data / "postgresql.conf").write_text(
            f"listen_addresses='127.0.0.1'\nport={port}\nunix_socket_directories=''\n", encoding="utf-8")
        started = False
        try:
            execute("pg_ctl", "-D", data, "-l", temporary / "postgres.log", "-w", "start")
            started = True
            assert execute("psql", "-h", "127.0.0.1", "-p", port, "-U", "runtime_smoke", "-d", "postgres",
                           "-At", "-v", "ON_ERROR_STOP=1", "-c",
                           "SELECT 42; DO $$ BEGIN PERFORM 1; END $$;").splitlines()[0] == "42"
        finally:
            if started:
                execute("pg_ctl", "-D", data, "-m", "fast", "-w", "stop")
    print("PostgreSQL relocated initdb/start/SQL/plpgsql/stop: passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    smoke(parser.parse_args().runtime)
