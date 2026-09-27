#!/usr/bin/env python3
"""Small destructive-to-fixture smoke test. Supply a NEW EMPTY database via
ORBISYNC_ADMIN_TEST_DATABASE_URL and a built binary with --binary. Never use a
live database: this initializes users, a world and an instance. Own processes
and temporary files are cleaned on exit; the caller owns database cleanup.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


def uuid7():
    value = bytearray(os.urandom(16))
    value[:6] = int(time.time() * 1000).to_bytes(6, "big")
    value[6] = (value[6] & 15) | 0x70
    value[8] = (value[8] & 63) | 0x80
    return str(uuid.UUID(bytes=bytes(value)))


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Launcher:
    def __init__(self, binary, directory, log, extra=()):
        self.token = None
        self.log = log
        self.stream = log.open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [str(binary), *extra, "web-admin", "--data-dir", str(directory), "--port", "0", "--no-browser"],
            stdout=self.stream, stderr=subprocess.STDOUT,
            creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
        )
        print(f"Owned launcher PID={self.process.pid} started={time.time():.0f} exe={binary}", flush=True)
        for _ in range(150):
            text = log.read_text(encoding="utf-8", errors="replace")
            link = directory / "launch-url.txt"
            private_url = link.read_text(encoding="utf-8") if "Administration:" in text and link.exists() else ""
            match = re.search(r"http://127\.0\.0\.1:\d+/#\S+", private_url)
            if match:
                url, self.capability = match.group(0).split("#")
                self.url = url.rstrip("/")
                assert self.capability not in text, "Private launch capability leaked to redirected logs"
                return
            if self.process.poll() is not None:
                self.close()
                raise AssertionError("Launcher failed before URL; " + text[-500:])
            time.sleep(.1)
        self.close()
        raise AssertionError("Launcher did not become available")

    def call(self, path, method="GET", body=None, expected=200, overrides=None):
        headers = {"x-orbisync-local": self.capability, "Origin": self.url}
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        if method != "GET":
            headers["Idempotency-Key"] = uuid7()
        if body is not None:
            headers["Content-Type"] = "application/json"
        if overrides:
            headers.update(overrides)
        request = urllib.request.Request(self.url + path, data=None if body is None else json.dumps(body).encode(), method=method, headers=headers)
        try:
            response = urllib.request.urlopen(request, timeout=40)
        except urllib.error.HTTPError as error:
            response = error
        data = response.read()
        assert response.status == expected, f"{method} {path}: expected {expected}, got {response.status}: {data[:300]!r}"
        assert response.headers.get("Cache-Control") == "no-store" or expected == 403
        return json.loads(data) if data else None

    def login(self, login, password):
        self.token = self.call("/api/v1/auth/login", "POST", {"login_id": login, "password": password})["access_token"]

    def start(self):
        self.call("/admin/start", "POST", {})
        for _ in range(150):
            if self.call("/admin/status")["ready"]:
                return
            time.sleep(.1)
        raise AssertionError("Engine did not become ready")

    def close(self):
        if self.process.poll() is None:
            # Fixture-only process termination. No live process is discovered or touched.
            self.process.terminate()
            self.process.wait(timeout=20)
        self.stream.close()
        print(f"Cleaned launcher PID={self.process.pid}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    db = os.environ["ORBISYNC_ADMIN_TEST_DATABASE_URL"]
    engine_port = free_port()
    children = []
    with tempfile.TemporaryDirectory(prefix="orbisync-admin-smoke-") as temporary:
        root = Path(temporary)
        directory = root / "installation"
        try:
            app = Launcher(binary, directory, root / "first.log")
            children.append(app)
            assert not app.call("/admin/status")["initialized"]
            app.call("/admin/status", expected=403, overrides={"x-orbisync-local": "invalid"})
            app.call("/admin/status", expected=403, overrides={"Origin": "https://foreign.example"})
            app.call("/admin/status", expected=403, overrides={"Host": "foreign.example"})
            setup = {"database_url": db, "local_development": True, "login_id": "operator", "display_name": "Operator", "engine_bind": f"127.0.0.1:{engine_port}"}
            app.call("/admin/check", "POST", {**setup, "database_url": "invalid"}, expected=400)
            app.call("/admin/initialize", "POST", {**setup, "local_development": False}, expected=400)
            app.call("/admin/initialize", "POST", {**setup, "engine_bind": "0.0.0.0:8080"}, expected=400)
            assert not (directory / "orbisync.toml").exists()
            app.call("/admin/check", "POST", setup)
            result = app.call("/admin/initialize", "POST", setup)
            initial_password = result["temporary_password"]
            assert app.call("/admin/status")["initialized"]
            snapshot = {p.name: p.read_bytes() for p in directory.iterdir() if p.is_file()}
            app.call("/admin/initialize", "POST", setup, expected=400)
            app.call("/admin/check", "POST", setup, expected=400)
            assert snapshot == {p.name: p.read_bytes() for p in directory.iterdir() if p.is_file()}
            app.start()
            app.call("/admin/settings", expected=401)
            app.login("operator", initial_password)
            app.call("/api/v1/auth/change-password", "POST", {"current_password": initial_password, "new_password": "Cedar!River7-Cosmos"}, expected=204)
            app.call("/api/v1/auth/me", expected=401)
            app.login("operator", "Cedar!River7-Cosmos")
            app.call("/api/v1/auth/administration-access", expected=204)
            user = app.call("/api/v1/users", "POST", {"login_id": "member", "display_name": "Member"}, expected=201, overrides={"Accept": "application/vnd.orbisync.user-credential+json"})
            uid = user["user"]["id"]
            app.call(f"/api/v1/users/{uid}/disable", "POST")
            app.call(f"/api/v1/users/{uid}/enable", "POST")
            role = app.call("/api/v1/roles", "POST", {"name": "Reader", "permissions": ["admin.users.read"]}, expected=201)
            app.call(f"/api/v1/users/{uid}/roles", "PUT", {"role_ids": [role["id"]]})
            app.call("/api/v1/users?limit=50")
            world = app.call("/api/v1/worlds", "POST", {"name": "Smoke world", "capacity": 8}, expected=201)
            instance = app.call("/api/v1/instances", "POST", {"world_id": world["id"]}, expected=201)
            app.call(f"/api/v1/instances/{instance['id']}/start", "POST", expected=202)
            app.call(f"/api/v1/instances/{instance['id']}/stop", "POST", expected=202)
            settings = app.call("/admin/settings")
            edit = {"values": settings["values"], "manifest": settings["manifest"]}
            app.call("/admin/settings", "POST", {**edit, "values": {"database.max_connections": "0"}}, expected=400)
            app.call("/admin/settings", "POST", {**edit, "manifest": {"version": 2, "rules": []}}, expected=400)
            edit["values"]["database.max_connections"] = "12"
            app.call("/admin/settings", "POST", edit)
            assert app.call("/admin/status")["restart_required"]
            saved = (directory / "orbisync.toml").read_bytes()
            operator_token = app.token
            app.login("member", user["temporary_password"])
            app.call("/admin/settings", expected=403)
            app.call("/admin/settings", "POST", edit, expected=403)
            assert (directory / "orbisync.toml").read_bytes() == saved
            app.token = operator_token
            app.call("/api/v1/auth/logout", "POST", expected=204)
            app.call("/admin/settings", expected=403)
            app.close()
            children.remove(app)
            app = Launcher(binary, directory, root / "restart.log")
            children.append(app)
            assert app.call("/admin/status")["initialized"]
            assert not app.call("/admin/status")["restart_required"]
            app.start()
            app.login("operator", "Cedar!River7-Cosmos")
            assert app.call("/admin/settings")["values"]["database.max_connections"] == "12"
            app.call("/api/v1/users?limit=50")
            # A different fresh setup directory must not adopt/reinitialize this DB.
            other = Launcher(binary, root / "other", root / "other.log")
            children.append(other)
            other.call("/admin/initialize", "POST", setup, expected=400)
            assert not (root / "other" / "orbisync.toml").exists()
            assert (directory / "orbisync.toml").read_bytes() == saved
            print("PASS: setup, origin/capability guards, password change/login, users/roles, instance start/stop, config validation, restart persistence, RBAC/session revocation, existing database preservation", flush=True)
        finally:
            for app in reversed(children):
                app.close()


if __name__ == "__main__":
    main()
