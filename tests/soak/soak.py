#!/usr/bin/env python3
"""Run a bounded, observable soak test against the public load-generator.

The default is deliberately the specification's 24 hours.  Short runs are
supported for development and for the one-hour acceptance exercise, but a
short run is never described as having completed the 24-hour E-1 criterion.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes
import json
import math
import os
import platform
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

METRICS = (
    "process_resident_memory_bytes",
    "process_cpu_seconds_total",
    "instance_command_queue_depth",
    "instance_mailbox_saturated_total",
    "instance_mailbox_dropped_total",
    "state_updates_dropped_total",
    "outbound_queue_depth",
    "outbound_queue_depth_max",
    "outbound_queue_bytes",
    "outbound_queue_bytes_max",
    "websocket_connections_current",
    "websocket_connections_total",
    "http_requests_total",
    "tick_duration_seconds",
    # The server may not expose these yet.  Keeping them in the report makes
    # the missing part of the soak verdict visible instead of silently passing.
    "tokio_tasks_current",
    "db_pool_connections_in_use",
    "db_pool_connections_waiting",
    "gc_pause_seconds",
    "error_rate",
)

UNMEASURED_ITEMS = (
    "tokio task count",
    "DB connection count",
    "GC/allocation activity",
    "tick processing duration",
)
ERROR_STATUS_RE = re.compile(r'(?:^|,)status="([^"]+)"')


def duration_seconds(value: str) -> int:
    """Parse a positive duration such as ``24h``, ``10m`` or ``30s``."""
    units = {"s": 1, "m": 60, "h": 3600, "d": 86400}
    if not value or value[-1].lower() not in units:
        raise argparse.ArgumentTypeError("duration must end in s, m, h, or d")
    try:
        seconds = int(value[:-1]) * units[value[-1].lower()]
    except ValueError as error:
        raise argparse.ArgumentTypeError("duration must contain an integer") from error
    if seconds <= 0:
        raise argparse.ArgumentTypeError("duration must be positive")
    return seconds


def parse_metrics(body: str) -> dict[str, float]:
    """Parse Prometheus text while retaining label identity in the key."""
    result: dict[str, float] = {}
    for line in body.splitlines():
        fields = line.strip().split()
        if len(fields) < 2 or fields[0].startswith("#"):
            continue
        name = fields[0].split("{", 1)[0]
        if name not in METRICS:
            continue
        try:
            value = float(fields[1])
        except ValueError:
            continue
        if math.isfinite(value):
            result[fields[0]] = value
    return result


def scrape(url: str | None) -> tuple[bool, dict[str, float], str | None]:
    if not url:
        return False, {}, "metrics URL was not configured"
    try:
        with urllib.request.urlopen(url, timeout=10) as response:  # noqa: S310
            return True, parse_metrics(response.read().decode("utf-8")), None
    except (OSError, urllib.error.URLError, UnicodeError) as error:
        return False, {}, f"metrics scrape failed: {error}"


def process_rss_bytes(pid: int) -> int | None:
    """Return RSS where the host exposes it; unavailable is explicit."""
    if platform.system() == "Linux":
        try:
            for line in Path(f"/proc/{pid}/status").read_text(encoding="utf-8").splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
        except (OSError, ValueError):
            return None
    if platform.system() == "Windows":
        class Counters(ctypes.Structure):
            _fields_ = [("cb", ctypes.wintypes.DWORD), ("page_fault_count", ctypes.wintypes.DWORD),
                        ("peak_working_set", ctypes.c_size_t), ("working_set", ctypes.c_size_t),
                        ("quota_peak_paged_pool", ctypes.c_size_t), ("quota_paged_pool", ctypes.c_size_t),
                        ("quota_peak_non_paged_pool", ctypes.c_size_t), ("quota_non_paged_pool", ctypes.c_size_t),
                        ("pagefile_usage", ctypes.c_size_t), ("peak_pagefile_usage", ctypes.c_size_t)]
        try:
            process = ctypes.windll.kernel32.OpenProcess(0x0400 | 0x0010, False, pid)
            if not process:
                return None
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            ok = ctypes.windll.psapi.GetProcessMemoryInfo(
                process, ctypes.byref(counters), counters.cb)
            ctypes.windll.kernel32.CloseHandle(process)
            return counters.working_set if ok else None
        except (AttributeError, OSError):
            return None
    # A Windows/macOS implementation can be added without changing the report
    # contract.  Do not substitute zero: zero would hide a missing measurement.
    return None


def snapshot(server_pid: int | None, metrics_url: str | None, started: float) -> dict[str, object]:
    ok, values, error = scrape(metrics_url)
    unavailable = sorted({name for name in METRICS if not any(
        key.split("{", 1)[0] == name for key in values
    )})
    rss, rss_source = select_rss(values, server_pid)
    return {
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "scrape_ok": ok,
        "metrics": values,
        "unavailable_metrics": unavailable,
        "server_rss_bytes": rss if rss is not None else "unavailable",
        "rss_source": rss_source,
        "error": error,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration", type=duration_seconds, default=24 * 3600,
                        help="test window (default: 24h; e.g. --duration 1h)")
    parser.add_argument("--interval", type=duration_seconds, default=60,
                        help="snapshot interval (default: 60s)")
    parser.add_argument("--metrics-url", help="Prometheus endpoint")
    parser.add_argument("--server-url", default="http://127.0.0.1:8080")
    parser.add_argument("--instance-id", default="")
    parser.add_argument("--users", type=int, default=50)
    parser.add_argument("--hz", type=float, default=10.0)
    parser.add_argument("--scenario", default="transform",
                        help="load-generator scenario (default: transform)")
    parser.add_argument("--login-concurrency", type=int, default=4,
                        help="maximum login requests in flight")
    parser.add_argument("--placement", choices=("grid", "origin"), default="grid")
    parser.add_argument("--placement-spacing", type=float, default=7.5)
    parser.add_argument("--login-id", default="admin")
    parser.add_argument("--password-file")
    parser.add_argument("--load-generator", default="apps/load-generator/target/release/orbisync-load-generator")
    parser.add_argument("--server-pid", type=int,
                        help="server PID for direct RSS sampling (otherwise RSS is unavailable)")
    parser.add_argument("--rss-threshold", type=float, default=0.05,
                        help="maximum RSS growth ratio (default: 0.05; unchanged for short runs)")
    parser.add_argument("--output", type=Path, default=Path("artifacts/soak-report.json"))
    parser.add_argument("--commit", default=os.environ.get("GIT_COMMIT", "unknown"))
    return parser


def select_rss(metrics: dict[str, float], server_pid: int | None) -> tuple[int | float | None, str]:
    """Prefer the server's exported RSS, then fall back to an explicit PID."""
    for key, value in metrics.items():
        if key.split("{", 1)[0] == "process_resident_memory_bytes":
            return value, "metrics"
    if server_pid is not None:
        rss = process_rss_bytes(server_pid)
        if rss is not None:
            return rss, "server_pid"
    return None, "unavailable"


def metric_values(metrics: dict[str, float], name: str) -> list[float]:
    """Return every sample for a metric family, including labelled series."""
    return [value for key, value in metrics.items() if key.split("{", 1)[0] == name]


def websocket_connections(sample: dict[str, object]) -> float:
    """Return the aggregate active WebSocket count from a metrics snapshot."""
    metrics = sample.get("metrics", {})
    if not isinstance(metrics, dict):
        return 0.0
    return sum(metric_values(metrics, "websocket_connections_current"))


def http_error_rate(metrics: dict[str, float]) -> float | None:
    """Calculate the observed 4xx/5xx share from cumulative HTTP counters."""
    total = 0.0
    errors = 0.0
    for key, value in metrics.items():
        if key.split("{", 1)[0] != "http_requests_total":
            continue
        status = ERROR_STATUS_RE.search(key)
        if status is None:
            continue
        total += value
        if status.group(1).startswith(("4", "5")):
            errors += value
    return errors / total if total > 0 else None


def error_rate_observations(samples: list[dict[str, object]]) -> list[tuple[float, float]]:
    """Return elapsed/error-rate pairs for samples with status counters."""
    observations = []
    for sample in samples:
        metrics = sample.get("metrics", {})
        if not isinstance(metrics, dict):
            continue
        rate = http_error_rate(metrics)
        elapsed = sample.get("elapsed_seconds")
        if rate is not None and isinstance(elapsed, (int, float)):
            observations.append((float(elapsed), rate))
    return observations


def evaluate_error_rate(samples: list[dict[str, object]]) -> dict[str, object]:
    """Judge error-rate trend without treating unavailable counters as zero.

    Cumulative counters can hide a recent regression, so the comparison uses
    the first and last observed cumulative rates and states that limitation in
    the report.  With fewer than two observations the trend is explicitly not
    evaluated.
    """
    observations = error_rate_observations(samples)
    if len(observations) < 2:
        return {
            "status": "not_evaluated",
            "reason": "fewer than two samples contained labelled HTTP counters",
            "first_rate": observations[0][1] if observations else None,
            "last_rate": observations[-1][1] if observations else None,
        }
    first = observations[0][1]
    last = observations[-1][1]
    if last > first:
        return {
            "status": "failed",
            "reason": "the cumulative 4xx/5xx share increased between the first and last observations",
            "first_rate": first,
            "last_rate": last,
        }
    return {
        "status": "met",
        "reason": "the cumulative 4xx/5xx share did not increase between the first and last observations",
        "first_rate": first,
        "last_rate": last,
    }


def evaluate_rss(rss_values: list[int | float], threshold: float) -> dict[str, object]:
    """Apply the same RSS threshold to long and short runs."""
    growth = rss_growth_ratio(rss_values)
    if growth is None:
        return {"status": "not_evaluated", "growth_ratio": None,
                "reason": "fewer than two valid RSS samples"}
    if growth > threshold:
        return {"status": "failed", "growth_ratio": growth,
                "reason": f"RSS growth exceeded the configured {threshold:.2%} threshold"}
    return {"status": "met", "growth_ratio": growth,
            "reason": f"RSS growth stayed within the configured {threshold:.2%} threshold"}


def queue_summary(samples: list[dict[str, object]]) -> dict[str, object]:
    """Summarize queue depth/bytes while retaining raw time series in samples."""
    names = ("outbound_queue_depth", "outbound_queue_depth_max",
             "outbound_queue_bytes", "outbound_queue_bytes_max")
    result: dict[str, object] = {}
    for name in names:
        values = [value for sample in samples
                  if isinstance(sample.get("metrics"), dict)
                  for value in metric_values(sample["metrics"], name)]
        result[name] = {
            "samples": len(values),
            "max": max(values) if values else "unavailable",
            "last": values[-1] if values else "unavailable",
        }
    return result


def websocket_cleanup(samples: list[dict[str, object]]) -> dict[str, object]:
    """Check that active connections return to zero after the child exits."""
    if not samples:
        return {"status": "not_evaluated", "reason": "no samples"}
    final = websocket_connections(samples[-1])
    has_signal = any(metric_values(sample.get("metrics", {}),
                                   "websocket_connections_current")
                     for sample in samples
                     if isinstance(sample.get("metrics"), dict))
    if not has_signal:
        return {"status": "not_evaluated", "final": "unavailable",
                "reason": "websocket_connections_current was not scraped"}
    if final != 0:
        return {"status": "failed", "final": final,
                "reason": "active WebSocket connections did not return to zero"}
    return {"status": "met", "final": 0,
            "reason": "active WebSocket connections returned to zero"}


def start_output_reader(child: subprocess.Popen[str]) -> tuple[threading.Thread, list[str]]:
    """Drain child output while it runs so a verbose child cannot block on a full pipe."""
    lines: list[str] = []

    def drain() -> None:
        if child.stdout is not None:
            for line in child.stdout:
                lines.append(line)

    reader = threading.Thread(target=drain, name="soak-load-generator-output", daemon=True)
    reader.start()
    return reader, lines


def run(args: argparse.Namespace) -> int:
    if (args.users <= 0 or args.hz <= 0 or args.login_concurrency <= 0
            or args.placement_spacing <= 0 or args.rss_threshold < 0):
        raise SystemExit("users, hz, login-concurrency, and placement-spacing must be positive; rss-threshold must not be negative")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    command = [args.load_generator, "--server-url", args.server_url, "--users", str(args.users),
               "--hz", str(args.hz), "--duration", str(args.duration), "--login-id", args.login_id,
               "--login-concurrency", str(args.login_concurrency), "--placement", args.placement,
               "--placement-spacing", str(args.placement_spacing), "--scenario", args.scenario]
    if args.instance_id:
        command += ["--instance-id", args.instance_id]
    if args.password_file:
        command += ["--password-file", args.password_file]
    if args.metrics_url:
        command += ["--metrics-url", args.metrics_url, "--metrics-interval", str(args.interval)]

    started = time.monotonic()
    started_at = datetime.now(timezone.utc).isoformat()
    child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             text=True, encoding="utf-8", errors="replace")
    output_reader, output_lines = start_output_reader(child)
    samples = [snapshot(args.server_pid, args.metrics_url, started)]
    while child.poll() is None:
        try:
            child.wait(timeout=args.interval)
        except subprocess.TimeoutExpired:
            samples.append(snapshot(args.server_pid, args.metrics_url, started))
    output_reader.join()
    output = "".join(output_lines)
    samples.append(snapshot(args.server_pid, args.metrics_url, started))
    rss_values = rss_values_after_connection(samples)
    rss_sources = sorted({sample["rss_source"] for sample in samples
                          if sample["rss_source"] != "unavailable"})
    actual_duration = time.monotonic() - started
    rss_verdict = evaluate_rss(rss_values, args.rss_threshold)
    error_verdict = evaluate_error_rate(samples)
    queue_verdict = queue_summary(samples)
    websocket_verdict = websocket_cleanup(samples)
    completed = child.returncode == 0 and actual_duration >= args.duration
    e1_status = evaluate_e1(args.duration, completed, rss_values,
                            actual_duration=actual_duration,
                            rss_threshold=args.rss_threshold)
    failed_checks = [name for name, verdict in (
        ("rss", rss_verdict), ("error_rate", error_verdict),
        ("websocket_cleanup", websocket_verdict))
        if verdict["status"] == "failed"]
    overall_status = "completed" if not failed_checks and child.returncode == 0 else "failed"
    commit = args.commit
    if commit == "unknown":
        try:
            commit = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL
            ).strip()
        except (OSError, subprocess.SubprocessError):
            pass
    report = {
        "tool": "orbisync-soak",
        "status": overall_status,
        "e1_24h_soak": e1_status,
        "server_pid": args.server_pid if args.server_pid is not None else "unavailable",
        "server_rss_growth_ratio": rss_growth_ratio(rss_values),
        "rss_source": rss_sources[0] if len(rss_sources) == 1 else (
            "mixed" if rss_sources else "unavailable"),
        "rss_baseline_policy": (
            "first_and_last_samples_with_active_websockets"
            if any(websocket_connections(sample) > 0 for sample in samples)
            else "all_successful_samples_no_websocket_signal"
        ),
        "duration_seconds": args.duration,
        "actual_duration_seconds": round(actual_duration, 3),
        "interval_seconds": args.interval,
        "users": args.users,
        "hz": args.hz,
        "scenario": args.scenario,
        "rss_threshold": args.rss_threshold,
        "server_url": args.server_url,
        "commit": commit,
        "started_at": started_at,
        "ended_at": datetime.now(timezone.utc).isoformat(),
        "child_exit_code": child.returncode,
        "checks": {
            "rss": rss_verdict,
            "error_rate": error_verdict,
            "queue": queue_verdict,
            "websocket_cleanup": websocket_verdict,
        },
        "unmeasured_items": list(UNMEASURED_ITEMS),
        "unmeasured_item_count": len(UNMEASURED_ITEMS),
        "samples": samples,
        "load_generator_output": output,
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print("Soak test summary")
    print(f"  status: {report['status']} (child_exit={child.returncode}, actual={actual_duration:.1f}s)")
    print(f"  E-1 (24h): {e1_status} - a short run is smoke validation, not E-1 evidence")
    print(f"  RSS: {rss_verdict['status']} ({rss_verdict['reason']})")
    print(f"  error-rate trend: {error_verdict['status']} ({error_verdict['reason']})")
    print(f"  WebSocket cleanup: {websocket_verdict['status']} ({websocket_verdict['reason']})")
    print(f"  queue time series: {len(samples)} snapshots (see JSON checks.queue)")
    print(f"  unmeasured ({len(UNMEASURED_ITEMS)}): {', '.join(UNMEASURED_ITEMS)}")
    print(f"  JSON report: {args.output}")
    return child.returncode or (1 if overall_status == "failed" else 0)


def evaluate_e1(duration: int, completed: bool, rss_values: list[int | float],
                actual_duration: float | None = None, rss_threshold: float = 0.05) -> str:
    """Evaluate E-1 from the actual window and server RSS observations only."""
    growth = rss_growth_ratio(rss_values)
    observed_duration = duration if actual_duration is None else actual_duration
    if duration < 24 * 3600 or observed_duration < duration or growth is None:
        return "not_evaluated"
    if not completed:
        return "failed"
    return "met" if growth <= rss_threshold else "failed"


def rss_growth_ratio(rss_values: list[int | float]) -> float | None:
    """Compute first-to-last RSS growth, or None when it cannot be measured."""
    if len(rss_values) < 2 or rss_values[0] <= 0:
        return None
    return (rss_values[-1] - rss_values[0]) / rss_values[0]


def rss_values_after_connection(samples: list[dict[str, object]]) -> list[int | float]:
    """Use RSS samples after connections exist, excluding login warm-up RSS.

    Argon2's configured concurrency intentionally affects transient memory while
    login is in progress. The soak baseline must represent the connected,
    steady-state workload rather than that pre-connection spike.
    """
    connected = [
        sample for sample in samples
        if websocket_connections(sample) > 0
        and isinstance(sample.get("server_rss_bytes"), (int, float))
    ]
    selected = connected or samples
    return [
        sample["server_rss_bytes"]
        for sample in selected
        if isinstance(sample.get("server_rss_bytes"), (int, float))
    ]


if __name__ == "__main__":
    parser = build_parser()
    sys.exit(run(parser.parse_args()))
