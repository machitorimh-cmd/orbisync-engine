#!/usr/bin/env python3
"""Real-binary local recovery verification using only owned temporary fixtures.

Requires the binary's bundled runtime/postgres. No external DATABASE_URL, SDK,
Node, Rust build or existing installation is used. Partial-setup verification
empties only a newly created fixture cluster after its engine child is stopped.
"""
import argparse
import ctypes
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

if __package__:
    from .private_fixture import private_temporary_directory
    from .verify_web_admin import Launcher
else:
    from private_fixture import private_temporary_directory
    from verify_web_admin import Launcher

STAGE = "initialization"


def check(condition, code):
    if not condition:
        raise AssertionError(code)


def api(app, path, method="GET", body=None, expected=200):
    return app.call(path, method, body, expected=expected, timeout=180)


def probe(app):
    result = api(app, "/admin/probe", "POST", {})
    check(result.get("ok") is True, "probe_failed")
    checks = {item.get("step"): item.get("status") for item in result.get("checks", [])}
    check(checks == {name: "passed" for name in ("http", "websocket", "join", "sync")}, "probe_steps_incomplete")


def engine_port(directory):
    config = tomllib.loads((directory / "orbisync.toml").read_text(encoding="utf-8"))
    return int(config["server"]["bind"].rsplit(":", 1)[1])


def listen(port):
    server = socket.socket()
    try:
        # Unix retains closed engine/database connections in TIME_WAIT. Reuse
        # those tuples without permitting a second live listener (SO_REUSEPORT).
        # Windows SO_REUSEADDR can steal a live listener, so keep it disabled.
        if os.name != "nt":
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", port))
        server.listen(4)
    except BaseException:
        server.close()
        raise
    return server


def assert_data(app, user_id, world_id, role_id):
    users = api(app, "/api/v1/users?limit=200")["items"]
    worlds = api(app, "/api/v1/worlds?limit=200")["items"]
    check(any(item["id"] == user_id and item["display_name"] == "Preserved fixture user" for item in users), "user_data_lost")
    check(any(item["id"] == world_id and item["name"] == "Preserved fixture world" for item in worlds), "world_data_lost")
    assigned = api(app, f"/api/v1/users/{user_id}/roles")
    check(role_id in assigned.get("role_ids", []), "role_assignment_lost")


def linux_owned_engine_matches(pid, parent_pid, binary):
    process = Path("/proc") / str(pid)
    try:
        parent = next((line.partition(":")[2].strip() for line in (process / "status").read_text().splitlines()
                       if line.startswith("PPid:")), None)
        return (parent == str(parent_pid)
                and (process / "exe").resolve(strict=True) == binary.resolve()
                and b"managed-serve" in (process / "cmdline").read_bytes().split(b"\0"))
    except (FileNotFoundError, ProcessLookupError):
        return False


def linux_owned_engine_candidates(parent_pid, binary):
    # Linux tracks children on their spawning thread, including Tokio workers.
    # Aggregate every task, then verify the process-wide parent and identity.
    children = set()
    for task in (Path("/proc") / str(parent_pid) / "task").iterdir():
        try:
            children.update(int(child) for child in (task / "children").read_text().split())
        except (FileNotFoundError, ProcessLookupError):
            continue
    return sorted(child for child in children if linux_owned_engine_matches(child, parent_pid, binary))


def terminate_owned_engine(app, binary):
    """Terminate only this fixture launcher's exact managed-serve child."""
    if os.name == "nt":
        command = (
            f"@(Get-CimInstance Win32_Process -Filter 'ParentProcessId = {app.process.pid}' | "
            "Where-Object { $_.Name -eq 'orbisync-server.exe' -and $_.CommandLine -match '(?:^|\\s)managed-serve(?:\\s|$)' } | "
            "ForEach-Object { $_.ProcessId }) | ConvertTo-Json -Compress"
        )
        result = subprocess.run(["powershell", "-NoProfile", "-Command", command], capture_output=True, text=True,
                                creationflags=subprocess.CREATE_NO_WINDOW, timeout=30)
        check(result.returncode == 0, "child_inventory_failed")
        candidates = json.loads(result.stdout or "[]")
        if isinstance(candidates, int):
            candidates = [candidates]
        check(len(candidates) == 1, "owned_engine_not_unique")
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
        kernel.OpenProcess.restype = ctypes.c_void_p
        kernel.QueryFullProcessImageNameW.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_uint32)]
        kernel.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        handle = kernel.OpenProcess(0x0001 | 0x1000 | 0x00100000, False, candidates[0])
        check(bool(handle), "owned_engine_handle_failed")
        try:
            image = ctypes.create_unicode_buffer(32768)
            length = ctypes.c_uint32(len(image))
            check(kernel.QueryFullProcessImageNameW(handle, 0, image, ctypes.byref(length)), "owned_engine_image_failed")
            check(Path(image.value).resolve() == binary.resolve(), "owned_engine_image_mismatch")
            check(kernel.TerminateProcess(handle, 91), "owned_engine_termination_failed")
            check(kernel.WaitForSingleObject(handle, 10_000) == 0, "owned_engine_exit_timeout")
        finally:
            kernel.CloseHandle(handle)
    else:
        candidates = linux_owned_engine_candidates(app.process.pid, binary)
        check(len(candidates) == 1, "owned_engine_not_unique")
        if hasattr(os, "pidfd_open"):
            descriptor = os.pidfd_open(candidates[0])
            try:
                check(linux_owned_engine_matches(candidates[0], app.process.pid, binary), "owned_engine_identity_changed")
                signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                check(bool(select.select([descriptor], [], [], 10)[0]), "owned_engine_exit_timeout")
            finally:
                os.close(descriptor)
        else:
            check(linux_owned_engine_matches(candidates[0], app.process.pid, binary), "owned_engine_identity_changed")
            os.kill(candidates[0], signal.SIGKILL)
            deadline = time.monotonic() + 10
            process = Path("/proc") / str(candidates[0])
            while process.exists():
                try:
                    if "State:\tZ" in (process / "status").read_text():
                        break
                except FileNotFoundError:
                    break
                check(time.monotonic() < deadline, "owned_engine_exit_timeout")
                time.sleep(.02)


def fake_http(port):
    captured = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def respond(self):
            count = min(int(self.headers.get("Content-Length", "0")), 1024 * 1024)
            body = self.rfile.read(count) if count else b""
            captured.append((self.command, self.path, self.headers.get("Authorization"), body))
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{}')

        do_GET = respond
        do_POST = respond

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, captured


def fixture_sql(binary, directory, root, sql):
    check(directory.resolve().is_relative_to(root.resolve()), "fixture_directory_mismatch")
    manifest = json.loads((directory / "local-postgres/managed.json").read_text(encoding="utf-8"))
    metadata = json.loads((directory / "local-trial.json").read_text(encoding="utf-8"))
    check(manifest.get("postgres_major") == 17 and metadata.get("complete") is True, "fixture_database_identity_unconfirmed")
    client = binary.parent / "runtime/postgres/bin" / ("psql.exe" if os.name == "nt" else "psql")
    check(client.is_file(), "fixture_psql_missing")
    env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    env.update({"PGPASSWORD": manifest["owner_password"], "PGCONNECT_TIMEOUT": "5", "PGSSLMODE": "disable"})
    if os.name != "nt":
        env["LD_LIBRARY_PATH"] = str(binary.parent / "runtime/postgres/lib")
    result = subprocess.run([str(client), "-X", "-q", "-v", "ON_ERROR_STOP=1", "-h", "127.0.0.1", "-p", str(manifest["port"]),
                             "-U", "orbisync_local_owner", "-d", "orbisync", "-c", sql], env=env, capture_output=True,
                            creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0, timeout=30)
    check(result.returncode == 0, "fixture_sql_failed")


def main():
    global STAGE
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--skip-partial", action="store_true", help="Run main recovery cases without the separate partial-setup fixture")
    args = parser.parse_args()
    binary = args.binary.resolve()
    check(binary.is_file() and (binary.parent / "runtime/postgres/BUILD.json").is_file(), "bundled_runtime_required")
    apps, sockets, servers = [], [], []
    with private_temporary_directory(prefix="orbisync-local-trial-recovery-") as temporary:
        root = Path(temporary).resolve()
        check(root.parent == Path(tempfile.gettempdir()).resolve() and root.name.startswith("orbisync-local-trial-recovery-"), "unsafe_fixture_root")
        try:
            STAGE = "automatic_trial"
            directory = root / "main"
            app = Launcher(binary, directory, root / "main.log"); apps.append(app)
            created = api(app, "/admin/local-trial", "POST", {})
            check(created.get("ok") and created.get("initial_password") and created.get("access_token"), "initial_trial_credentials_missing")
            password = created["initial_password"]
            app.token = created["access_token"]
            probe(app)
            user = api(app, "/api/v1/users", "POST", {"login_id": "recovery-fixture", "display_name": "Preserved fixture user"}, 201)
            world = api(app, "/api/v1/worlds", "POST", {"name": "Preserved fixture world", "capacity": 8}, 201)
            role = api(app, "/api/v1/roles", "POST", {"name": "Preserved fixture role", "permissions": ["admin.users.read"]}, 201)
            api(app, f"/api/v1/users/{user['id']}/roles", "PUT", {"role_ids": [role["id"]]})
            identifiers = (user["id"], world["id"], role["id"])
            STAGE = "stop_start_restart"
            api(app, "/admin/stop", "POST", {})
            check(api(app, "/admin/status")["ready"] is False, "stop_still_ready")
            api(app, "/admin/start", "POST", {}); probe(app); assert_data(app, *identifiers)
            api(app, "/admin/restart", "POST", {}); probe(app); assert_data(app, *identifiers)
            STAGE = "resume_credentials"
            resumed = api(app, "/admin/local-trial", "POST", {})
            check("initial_password" not in resumed and resumed.get("access_token"), "initial_password_repeated")
            app.token = resumed["access_token"]
            check((directory / "initial-admin-password.txt").read_text(encoding="utf-8") == password, "private_password_file_stale")
            STAGE = "engine_port_conflict"
            api(app, "/admin/stop", "POST", {})
            old_engine = engine_port(directory); foreign = listen(old_engine); sockets.append(foreign)
            failure = api(app, "/admin/start", "POST", {}, 400)
            check(failure.get("code") == "engine_port_in_use", "engine_conflict_code")
            diagnostic = api(app, "/admin/diagnostics")
            check({"action": "choose_engine_port"} in diagnostic["actions"], "engine_repair_missing")
            api(app, "/admin/repair", "POST", {"action": "choose_engine_port"})
            check(engine_port(directory) != old_engine and foreign.fileno() >= 0, "engine_foreign_listener_changed")
            probe(app); assert_data(app, *identifiers)
            STAGE = "database_port_conflict"
            api(app, "/admin/stop", "POST", {})
            old_db = api(app, "/admin/status")["managed_db"]["port"]
            foreign_db = listen(old_db); sockets.append(foreign_db)
            failure = api(app, "/admin/start", "POST", {}, 400)
            check(failure.get("code") == "port_in_use", "database_conflict_code")
            diagnostic = api(app, "/admin/diagnostics")
            check({"action": "choose_database_port"} in diagnostic["actions"], "database_repair_missing")
            api(app, "/admin/repair", "POST", {"action": "choose_database_port"})
            check(api(app, "/admin/status")["managed_db"]["port"] != old_db and foreign_db.fileno() >= 0, "database_foreign_listener_changed")
            probe(app); assert_data(app, *identifiers)
            STAGE = "unknown_repair"
            before = (directory / "orbisync.toml").read_bytes()
            failure = api(app, "/admin/repair", "POST", {"action": "delete_database"}, 400)
            check(failure.get("code") == "unsupported_action", "unknown_repair_accepted")
            check((directory / "orbisync.toml").read_bytes() == before, "unknown_repair_mutated_config")
            STAGE = "foreign_http_secret_fence"
            old_engine = engine_port(directory); terminate_owned_engine(app, binary)
            foreign_http, captured = fake_http(old_engine); servers.append(foreign_http)
            check(api(app, "/admin/status")["ready"] is False, "foreign_http_marked_ready")
            result = api(app, "/admin/probe", "POST", {})
            check(result.get("ok") is False, "foreign_http_probe_passed")
            api(app, "/api/v1/auth/login", "POST", {"login_id": "local-admin", "password": password}, 400)
            check(not captured, "foreign_http_received_request")
            api(app, "/admin/stop", "POST", {})
            app.close(); apps.remove(app)
            foreign_http.shutdown(); foreign_http.server_close(); servers.remove(foreign_http)
            STAGE = "launcher_reload"
            app = Launcher(binary, directory, root / "resume.log"); apps.append(app)
            resumed = api(app, "/admin/local-trial", "POST", {})
            check("initial_password" not in resumed, "reload_password_repeated")
            app.token = resumed["access_token"]; probe(app); assert_data(app, *identifiers)

            if not args.skip_partial:
                STAGE = "partial_fixture"
                partial_dir = root / "partial"
                partial = Launcher(binary, partial_dir, root / "partial.log"); apps.append(partial)
                api(partial, "/admin/local-trial", "POST", {})
                terminate_owned_engine(partial, binary)
                # This is a fresh fixture cluster only. Recreate the state just
                # before first migration; no real database is ever selected.
                fixture_sql(binary, partial_dir, root, "DROP SCHEMA audit_operations CASCADE; DROP SCHEMA public CASCADE; CREATE SCHEMA public AUTHORIZATION orbisync_local_owner; GRANT USAGE ON SCHEMA public TO orbisync;")
                api(partial, "/admin/stop", "POST", {})
                partial.close(); apps.remove(partial)
                manifest = json.loads((partial_dir / "local-postgres/managed.json").read_text(encoding="utf-8"))
                config = partial_dir / "orbisync.toml"
                check(config.resolve().is_relative_to(root), "unsafe_partial_config")
                config.unlink()
                (partial_dir / "initializing").write_text("Fixture: interrupted before config creation.\n", encoding="utf-8")
                (partial_dir / "local-trial.json").write_text(json.dumps({"complete": False, "world_id": None, "instance_id": None}), encoding="utf-8")
                STAGE = "partial_recovery"
                partial = Launcher(binary, partial_dir, root / "partial-resume.log"); apps.append(partial)
                recovered = api(partial, "/admin/local-trial", "POST", {})
                partial.token = recovered["access_token"]; probe(partial)
                after = json.loads((partial_dir / "local-postgres/managed.json").read_text(encoding="utf-8"))
                check(after["installation_id"] == manifest["installation_id"], "partial_cluster_replaced")
                check(any(partial_dir.glob("incomplete-setup-*")), "partial_files_not_preserved")
                check(not (partial_dir / "initializing").exists(), "partial_marker_not_completed")
            STAGE = "complete"
            print("PASS: real local trial, four probe stages, lifecycle/data retention, password-once resume, engine/database port repair, unknown action rejection, foreign HTTP secret fence, reload" + (" and pre-config partial recovery." if not args.skip_partial else "."), flush=True)
        finally:
            for app in reversed(apps):
                if app.process.poll() is None:
                    try:
                        api(app, "/admin/stop", "POST", {})
                    except Exception:
                        pass
                app.close()
            for server in servers:
                server.shutdown(); server.server_close()
            for server in sockets:
                server.close()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Values, URLs, token/password contents and private log fragments are
        # deliberately excluded from stdout/stderr, including failed fixtures.
        code = str(error)
        safe = code if code and len(code) <= 120 and all(character.isalnum() or character in "_/-" for character in code) else type(error).__name__
        print(f"FAIL: {STAGE}/{safe}", file=sys.stderr)
        raise SystemExit(1) from None
