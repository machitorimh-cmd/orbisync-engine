from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import check_metrics_wiring as checker

SOURCE_ROOT = Path(__file__).resolve().parents[1]
REAL_ROOT = SOURCE_ROOT


def _write_metrics(root: Path, counter_variants: list[str], histogram_variants: list[str]) -> None:
    path = root / "crates" / "orbisync-application" / "src" / "metrics.rs"
    path.parent.mkdir(parents=True, exist_ok=True)
    # Build enum definitions
    counter_body = ",\n    ".join(f"{v}" for v in counter_variants)
    if counter_body:
        counter_body += ","
    hist_body = ",\n    ".join(f"{v}" for v in histogram_variants)
    if hist_body:
        hist_body += ","
    # Minimal metrics file with required enums and traits
    content = f"""
use std::collections::HashMap;
pub enum HttpMethod {{ Get, Post }}
impl HttpMethod {{ pub const fn as_str(self) -> &'static str {{ "GET" }} }}
pub enum HttpStatusClass {{ Success }}
impl HttpStatusClass {{ pub const fn as_str(self) -> &'static str {{ "2xx" }} pub const fn from_status(code: u16) -> Self {{ Self::Success }} }}
pub enum RateLimitScope {{ User }}
impl RateLimitScope {{ pub const fn as_str(self) -> &'static str {{ "user" }} }}
pub enum Counter {{
    {counter_body}
}}
pub enum Histogram {{
    {hist_body}
}}
pub enum Gauge {{}}
pub trait MetricsRecorder: Send + Sync + 'static {{ fn incr(&self, counter: Counter); fn add(&self, counter: Counter, n: u64); fn set(&self, gauge: Gauge, value: i64); fn observe(&self, histogram: Histogram, value: f64); }}
pub trait MetricsExporter: Send + Sync + 'static {{ fn render(&self) -> String; }}
"""
    path.write_text(content, encoding="utf-8")


def _write_production_file(root: Path, crate: str, filename: str, content: str) -> Path:
    path = root / "crates" / crate / "src" / filename
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return path


def _minimal_design_doc(root: Path) -> None:
    doc_path = root / "docs" / "design" / "observability-and-config.md"
    doc_path.parent.mkdir(parents=True, exist_ok=True)
    doc_path.write_text(
        "### 3.1\n```text\nhttp_requests_total\nhttp_request_duration_seconds\nauth_login_failures_total\ndb_query_duration_seconds\n```\n",
        encoding="utf-8",
    )


class MetricsWiringTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name).resolve()
        # Create minimal crates layout
        (self.root / "crates" / "orbisync-observability" / "src").mkdir(parents=True, exist_ok=True)
        (self.root / "crates" / "orbisync-observability" / "src" / "metrics.rs").write_text(
            'use prometheus_client::registry::Registry; pub fn dummy() {} // http_requests, auth_login_failures, rate_limit_rejected, http_request_duration_seconds, db_query_duration_seconds, process_cpu_seconds_total, process_resident_memory_bytes',
            encoding="utf-8",
        )
        _minimal_design_doc(self.root)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_parse_variants(self) -> None:
        _write_metrics(self.root, ["A", "B", "C"], ["H1", "H2"])
        variants = checker.parse_metrics_variants(self.root)
        self.assertEqual(variants["Counter"], ["A", "B", "C"])
        self.assertEqual(variants["Histogram"], ["H1", "H2"])
        self.assertEqual(variants["Gauge"], [])

    def test_wiring_fails_when_variant_not_constructed(self) -> None:
        _write_metrics(self.root, ["HttpRequests", "AuthLoginFailures"], ["HttpRequestDuration"])
        # Only wire HttpRequests, not AuthLoginFailures
        _write_production_file(
            self.root,
            "orbisync-transport-http",
            "lib.rs",
            "use crate::metrics::Counter; fn f() { let _ = Counter::HttpRequests; }\n",
        )
        errors, _ = checker.check_variant_wiring(self.root)
        self.assertTrue(any("AuthLoginFailures" in e for e in errors))
        self.assertFalse(any("HttpRequests" in e for e in errors))

    def test_wiring_passes_when_all_wired(self) -> None:
        _write_metrics(self.root, ["HttpRequests", "AuthLoginFailures"], ["HttpRequestDuration", "DbQueryDuration"])
        _write_production_file(
            self.root,
            "orbisync-transport-http",
            "lib.rs",
            "use orbisync_application::metrics::{Counter, Histogram}; fn f() { let _ = Counter::HttpRequests; let _ = Counter::AuthLoginFailures; let _ = Histogram::HttpRequestDuration; }",
        )
        _write_production_file(
            self.root,
            "orbisync-storage-postgres",
            "lib.rs",
            "use orbisync_application::metrics::Histogram; fn f() { let _ = Histogram::DbQueryDuration; }",
        )
        errors, info = checker.check_variant_wiring(self.root)
        self.assertEqual(errors, [], f"expected no errors, got {errors} info={info}")

    def test_cfg_test_block_is_excluded(self) -> None:
        _write_metrics(self.root, ["AuthLoginFailures"], [])
        _write_production_file(
            self.root,
            "orbisync-transport-http",
            "lib.rs",
            "#[cfg(test)]\nmod tests { use super::*; fn f() { let _ = Counter::AuthLoginFailures; } }\n",
        )
        errors, _ = checker.check_variant_wiring(self.root)
        self.assertTrue(any("AuthLoginFailures" in e for e in errors))

    def test_string_assembly_detects_format_with_placeholder_before_suffix(self) -> None:
        _write_metrics(self.root, ["A"], [])
        _write_production_file(
            self.root,
            "orbisync-observability",
            "bad.rs",
            'fn f(name: &str) { let _ = format!("{}_total", name); }\n',
        )
        errors = checker.check_string_assembly(self.root)
        self.assertTrue(len(errors) > 0)
        self.assertTrue(any("_total" in e for e in errors))

    def test_string_assembly_allows_static_format_with_value_after(self) -> None:
        _write_metrics(self.root, ["A"], [])
        _write_production_file(
            self.root,
            "orbisync-observability",
            "ok.rs",
            'fn f(cpu: f64) { let _ = format!("process_cpu_seconds_total {cpu}\\n"); }\n',
        )
        errors = checker.check_string_assembly(self.root)
        self.assertEqual(errors, [])

    def test_mutation_adding_variant_without_wiring_fails(self) -> None:
        # Baseline: 2 variants, both wired -> green
        _write_metrics(self.root, ["A", "B"], [])
        _write_production_file(
            self.root,
            "orbisync-transport-http",
            "lib.rs",
            "use orbisync_application::metrics::Counter; fn f() { let _ = Counter::A; let _ = Counter::B; }",
        )
        errors, _ = checker.check_variant_wiring(self.root)
        self.assertEqual(errors, [])
        # Mutate: add variant C without wiring -> red
        _write_metrics(self.root, ["A", "B", "C"], [])
        errors, _ = checker.check_variant_wiring(self.root)
        self.assertTrue(any("C" in e for e in errors))
        # Restore wiring -> green again
        _write_production_file(
            self.root,
            "orbisync-transport-http",
            "lib.rs",
            "use orbisync_application::metrics::Counter; fn f() { let _ = Counter::A; let _ = Counter::B; let _ = Counter::C; }",
        )
        errors, _ = checker.check_variant_wiring(self.root)
        self.assertEqual(errors, [])

    def test_real_workspace_is_green(self) -> None:
        # The real workspace should have all mailbox and baseline variants wired.
        variants = checker.parse_metrics_variants(REAL_ROOT)
        self.assertEqual(len(variants["Counter"]), 24)
        self.assertEqual(len(variants["Gauge"]), 9)
        self.assertEqual(len(variants["Histogram"]), 5)
        errors, _ = checker.check_variant_wiring(REAL_ROOT)
        self.assertEqual(errors, [], f"gate must be green on real workspace, got {errors}")
        # String assembly should be clean
        string_errors = checker.check_string_assembly(REAL_ROOT)
        self.assertEqual(string_errors, [], f"string assembly should be clean, got {string_errors}")

    def test_zero_scan_guard(self) -> None:
        # Empty metrics file should trigger zero-scan error
        _write_metrics(self.root, [], [])
        errors, info = checker.check_variant_wiring(self.root)
        self.assertTrue(len(errors) > 0)
        self.assertIn("no Counter", errors[0] if errors else "")


if __name__ == "__main__":
    unittest.main()
