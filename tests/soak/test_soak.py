#!/usr/bin/env python3
"""Mutation-sensitive checks for the soak verdict rules."""

import sys
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import soak  # noqa: E402


class E1VerdictTests(unittest.TestCase):
    def test_short_run_is_not_evaluated(self) -> None:
        self.assertEqual(soak.evaluate_e1(23 * 3600 + 59 * 60, True, [100, 101]),
                         "not_evaluated")

    def test_missing_server_rss_is_not_evaluated(self) -> None:
        self.assertEqual(soak.evaluate_e1(24 * 3600, True, []), "not_evaluated")

    def test_exact_threshold_cases(self) -> None:
        self.assertEqual(soak.evaluate_e1(24 * 3600, True, [100, 104.9]), "met")
        self.assertEqual(soak.evaluate_e1(24 * 3600, True, [100, 105]), "met")
        self.assertEqual(soak.evaluate_e1(24 * 3600, True, [100, 105.1]), "failed")

    def test_incomplete_run_cannot_pass(self) -> None:
        self.assertEqual(soak.evaluate_e1(24 * 3600, False, [100, 101]), "failed")

    def test_rss_source_prefers_metrics_over_pid(self) -> None:
        value, source = soak.select_rss({"process_resident_memory_bytes": 123}, 999999)
        self.assertEqual((value, source), (123, "metrics"))

    def test_rss_source_without_observation_is_unavailable(self) -> None:
        value, source = soak.select_rss({}, None)
        self.assertEqual((value, source), (None, "unavailable"))

    def test_rss_threshold_is_applied_to_short_runs(self) -> None:
        self.assertEqual(soak.evaluate_rss([100, 101], 0.05)["status"], "met")
        self.assertEqual(soak.evaluate_rss([100, 106], 0.05)["status"], "failed")

    def test_unmeasured_items_are_explicit(self) -> None:
        self.assertEqual(len(soak.UNMEASURED_ITEMS), 4)
        self.assertEqual(
            soak.websocket_cleanup([
                {"metrics": {"websocket_connections_current": 2}},
                {"metrics": {"websocket_connections_current": 0}},
            ])["status"],
            "met",
        )

    def test_run_drains_verbose_child_output_before_collecting_report(self) -> None:
        class FakeChild:
            def __init__(self) -> None:
                self.stdout = io.StringIO("child output\n")
                self.returncode = 0
                self.polls = 0

            def poll(self) -> int | None:
                self.polls += 1
                return None if self.polls == 1 else self.returncode

            def wait(self, timeout: int) -> int:
                return self.returncode

            def communicate(self) -> tuple[str, None]:
                raise AssertionError("run must drain stdout concurrently, not call communicate")

        args = soak.build_parser().parse_args([
            "--duration", "1s", "--interval", "1s", "--users", "1", "--hz", "1",
            "--load-generator", "fake-generator", "--output",
            "artifacts/test-soak-output-drain.json", "--commit", "test",
        ])
        child = FakeChild()
        sample = {
            "metrics": {
                "process_resident_memory_bytes": 100,
                "websocket_connections_current": 0,
            },
            "server_rss_bytes": 100,
            "rss_source": "metrics",
        }
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(soak.subprocess, "Popen", return_value=child), \
                patch.object(soak, "snapshot", return_value=sample), \
                patch.object(soak.time, "monotonic", side_effect=[0.0, 1.0]):
            args.output = Path(directory) / "report.json"
            self.assertEqual(soak.run(args), 0)
            report = json.loads(args.output.read_text(encoding="utf-8"))
        self.assertEqual(report["load_generator_output"], "child output\n")


if __name__ == "__main__":
    unittest.main()
