"""Verify the real extracted consumer bundle, including managed DB lifecycle."""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import urllib.parse
import urllib.request
import urllib.error

if __package__:
    from . import postgres_runtime
    from .private_fixture import private_temporary_directory
else:
    import postgres_runtime
    from private_fixture import private_temporary_directory


def _http_failure(route, error):
    message = f"consumer request {route} returned HTTP {error.code}"
    try:
        payload = json.loads(error.read(16_384))
    except (ValueError, OSError):
        payload = None
    code = payload.get("code") if isinstance(payload, dict) else None
    if isinstance(code, str) and re.fullmatch(r"[A-Za-z0-9_]{1,80}", code):
        message += f" (code={code})"
    # Error URLs, reason strings, message fields and all other response data
    # can contain credentials. Only the bounded public code is emitted.
    return RuntimeError(message)


def smoke(bundle, target):
    postgres_runtime.validate_runtime(bundle / "runtime/postgres", target)
    suffix = ".exe" if "windows" in target else ""
    binary = bundle / ("orbisync-server" + suffix)
    with private_temporary_directory(prefix="orbisync-consumer-") as temporary:
        data = Path(temporary) / "private-install"
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("ORBISYNC_", "PG")) and key != "DATABASE_URL"}
        # A preinstalled PG, Cargo, Node, or developer utility cannot mask a
        # missing distribution file. Windows system DLLs remain discoverable.
        env["PATH"] = os.defpath
        if os.name == "nt":
            system = Path(os.environ["SystemRoot"]) / "System32"
            env["PATH"] = os.pathsep.join(map(str, (system, system / "WindowsPowerShell/v1.0")))
        with (Path(temporary) / "launcher.log").open("w", encoding="utf-8") as log:
            process = subprocess.Popen([str(binary.resolve()), "web-admin", "--no-browser", "--port", "0",
                                        "--data-dir", str(data)], cwd=bundle, env=env,
                                       stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 45
                launch_file = data / "launch-url.txt"
                while not launch_file.is_file():
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("packaged launcher did not become available")
                    time.sleep(0.1)
                url = launch_file.read_text(encoding="utf-8").strip()
                parsed = urllib.parse.urlsplit(url)
                origin = f"{parsed.scheme}://{parsed.netloc}"
                capability = urllib.parse.parse_qs(parsed.query).get("token", [parsed.fragment])[0]
                # Launcher URLs may use an explicitly named fragment parameter.
                if "=" in capability:
                    capability = urllib.parse.parse_qs(capability).get("token", [capability])[0]
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                def request(route, body=None, access=None):
                    headers = {"Origin": origin, "x-orbisync-local": capability}
                    if access:
                        headers["Authorization"] = "Bearer " + access
                    payload = None if body is None else json.dumps(body).encode()
                    if payload is not None:
                        headers["Content-Type"] = "application/json"
                    req = urllib.request.Request(origin + route, data=payload, headers=headers)
                    try:
                        with opener.open(req, timeout=120) as response:
                            return json.load(response)
                    except urllib.error.HTTPError as error:
                        raise _http_failure(route, error) from None
                setup = request("/admin/local-trial", {})
                if setup.get("ok") is not True or not setup.get("access_token"):
                    raise RuntimeError("consumer local trial did not initialize and authenticate")
                status = request("/admin/status")
                if status.get("ready") is not True or status.get("local_trial") is not True:
                    raise RuntimeError("consumer engine did not report managed readiness")
                probe = request("/admin/probe", {}, setup["access_token"])
                if probe.get("ok") is not True or {item["step"] for item in probe.get("checks", []) if item["status"] == "passed"} != {"http", "websocket", "join", "sync"}:
                    raise RuntimeError("consumer HTTP/WebSocket/join/sync probe failed")
                request("/admin/stop", {})
                stopped = request("/admin/status")
                if stopped.get("ready") is True:
                    raise RuntimeError("consumer stop left the engine ready")
                request("/admin/start", {})
                if request("/admin/status").get("ready") is not True:
                    raise RuntimeError("consumer persisted installation did not restart")
                # The original token must still authenticate after a restart:
                # readiness alone would not prove users/sessions/keys survived.
                for operation in (None, "/admin/restart"):
                    if operation:
                        request(operation, {})
                    repeated = request("/admin/probe", {}, setup["access_token"])
                    if repeated.get("ok") is not True or {item["step"] for item in repeated.get("checks", [])
                            if item.get("status") == "passed"} != {"http", "websocket", "join", "sync"}:
                        raise RuntimeError("consumer authentication or protocol state did not survive restart")
                request("/admin/stop", {})
            except Exception as error:
                # Never print launch URL, tokens, passwords, or private log contents.
                detail = str(error) if isinstance(error, RuntimeError) else type(error).__name__
                raise RuntimeError(f"distribution smoke failed: {detail}; private logs were not published") from None
            finally:
                if process.poll() is None:
                    if os.name == "nt":
                        # Controller also handles launcher EOF; closing the
                        # launcher process is only a last-resort test cleanup.
                        process.terminate()
                    else:
                        process.send_signal(signal.SIGINT)
                    try:
                        process.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=10)
    print("Consumer bundle local trial, HTTP/WS/join/sync, stop/restart: passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--target", choices=postgres_runtime.TARGETS, required=True)
    args = parser.parse_args()
    smoke(args.bundle.resolve(), args.target)
