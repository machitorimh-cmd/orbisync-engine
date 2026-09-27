import contextlib
import io
import json
import os
import tempfile
import unittest
from datetime import date
from pathlib import Path
from unittest import mock

from scripts.check_npm_audit import check, load_json, validate_cli_paths


class CheckNpmAuditTests(unittest.TestCase):
    def run_check(self, report: dict, policy: dict) -> int:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report_path = root / "audit.json"
            policy_path = root / "policy.json"
            report_path.write_text(json.dumps(report), encoding="utf-8")
            policy_path.write_text(json.dumps(policy), encoding="utf-8")
            return check(report_path, policy_path, today=date(2026, 9, 2))

    @staticmethod
    def policy(exceptions: list[dict] | None = None) -> dict:
        return {"version": 1, "audit_level": "high", "exceptions": exceptions or []}

    @staticmethod
    def metadata(*severities: str) -> dict:
        counts = {severity: severities.count(severity) for severity in ("info", "low", "moderate", "high", "critical")}
        counts["total"] = len(severities)
        return {"vulnerabilities": counts}

    @classmethod
    def v2_report(cls, *, package: str = "example", severity: str = "high", source: int = 1234, title: str = "Example", via: list | None = None) -> dict:
        advisory = {
            "source": source,
            "name": package,
            "dependency": package,
            "title": title,
            "url": "https://github.com/advisories/1234",
            "severity": severity,
            "range": "<1.2.3",
        }
        return {
            "auditReportVersion": 2,
            "vulnerabilities": {
                package: {
                    "name": package,
                    "severity": severity,
                    "isDirect": False,
                    "via": via if via is not None else [advisory],
                    "effects": [],
                    "range": "<1.2.3",
                    "nodes": [f"node_modules/{package}"],
                }
            },
            "metadata": cls.metadata(severity),
        }

    def test_empty_v2_report_passes(self) -> None:
        report = {"auditReportVersion": 2, "vulnerabilities": {}, "metadata": self.metadata()}
        self.assertEqual(self.run_check(report, self.policy()), 0)

    def test_v3_report_is_supported(self) -> None:
        report = self.v2_report()
        report["auditReportVersion"] = 3
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_v1_high_advisory_fails(self) -> None:
        report = {
            "auditReportVersion": 1,
            "advisories": {
                "1234": {
                    "id": 1234,
                    "module_name": "example",
                    "severity": "high",
                    "title": "Example",
                    "url": "https://github.com/advisories/1234",
                    "findings": [{"version": "1.0.0", "paths": ["example"]}],
                }
            },
            "metadata": self.metadata("high"),
        }
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_high_advisory_fails(self) -> None:
        self.assertEqual(self.run_check(self.v2_report(), self.policy()), 1)

    def test_unexpired_matching_exception_passes(self) -> None:
        exception = {
            "id": "1234",
            "package": "example",
            "expires_on": "2026-12-31",
            "reason": "Upstream fix is scheduled for the next release.",
        }
        self.assertEqual(self.run_check(self.v2_report(), self.policy([exception])), 0)

    def test_expired_exception_fails(self) -> None:
        exception = {
            "id": "1234",
            "package": "example",
            "expires_on": "2026-09-01",
            "reason": "Temporary exception.",
        }
        report = {"auditReportVersion": 2, "vulnerabilities": {}, "metadata": self.metadata()}
        self.assertEqual(self.run_check(report, self.policy([exception])), 1)

    def test_error_null_fails_closed(self) -> None:
        report = {"error": None, "auditReportVersion": 2, "vulnerabilities": {}, "metadata": self.metadata()}
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_missing_or_unknown_report_format_fails_closed(self) -> None:
        self.assertEqual(self.run_check({}, self.policy()), 1)
        self.assertEqual(self.run_check({"auditReportVersion": 4, "vulnerabilities": {}, "metadata": self.metadata()}, self.policy()), 1)
        self.assertEqual(self.run_check({"advisories": {}, "vulnerabilities": {}, "metadata": self.metadata()}, self.policy()), 1)

    def test_malformed_vulnerability_entry_fails_closed(self) -> None:
        report = self.v2_report()
        report["vulnerabilities"]["example"] = "malformed"
        self.assertEqual(self.run_check(report, self.policy()), 1)
        report = self.v2_report()
        del report["vulnerabilities"]["example"]["severity"]
        self.assertEqual(self.run_check(report, self.policy()), 1)
        report = self.v2_report(via=[])
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_name_and_advisory_package_must_match(self) -> None:
        report = self.v2_report()
        report["vulnerabilities"]["example"]["name"] = "other"
        self.assertEqual(self.run_check(report, self.policy()), 1)
        report = self.v2_report()
        report["vulnerabilities"]["example"]["via"][0]["dependency"] = "other"
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_duplicate_advisory_is_rejected(self) -> None:
        report = self.v2_report()
        advisory = dict(report["vulnerabilities"]["example"]["via"][0])
        report["vulnerabilities"]["example"]["via"].append(advisory)
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_v1_id_must_match_advisory_key(self) -> None:
        report = {
            "advisories": {
                "1234": {
                    "id": 5678,
                    "module_name": "example",
                    "severity": "high",
                    "title": "Example",
                    "url": "https://github.com/advisories/1234",
                    "findings": [{"version": "1.0.0", "paths": ["example"]}],
                }
            },
            "metadata": self.metadata("high"),
        }
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_report_version_is_required_and_integer(self) -> None:
        v1 = {
            "auditReportVersion": 1,
            "advisories": {},
            "metadata": self.metadata(),
        }
        for version in (None, 1.0):
            report = dict(v1)
            if version is None:
                del report["auditReportVersion"]
            else:
                report["auditReportVersion"] = version
            with self.subTest(version=version):
                self.assertEqual(self.run_check(report, self.policy()), 1)

        for version in (None, 2.0):
            report = self.v2_report()
            if version is None:
                del report["auditReportVersion"]
            else:
                report["auditReportVersion"] = version
            with self.subTest(version=version):
                self.assertEqual(self.run_check(report, self.policy()), 1)

        policy = self.policy()
        policy["version"] = 1.0
        self.assertEqual(self.run_check({"auditReportVersion": 2, "vulnerabilities": {}, "metadata": self.metadata()}, policy), 1)

    def test_metadata_counts_must_match_report_entries(self) -> None:
        report = self.v2_report(severity="high")
        report["metadata"] = self.metadata("low")
        self.assertEqual(self.run_check(report, self.policy()), 1)

        report = {
            "auditReportVersion": 1,
            "advisories": {
                "1234": {
                    "id": 1234,
                    "module_name": "example",
                    "severity": "high",
                    "title": "Example",
                    "url": "https://github.com/advisories/1234",
                    "findings": [{"version": "1.0.0", "paths": ["example"]}],
                }
            },
            "metadata": self.metadata("low"),
        }
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_nested_allowlists_and_nulls_fail_closed(self) -> None:
        mutations = []

        report = self.v2_report()
        report["vulnerabilities"]["example"]["unknown"] = True
        mutations.append(report)

        report = self.v2_report()
        report["vulnerabilities"]["example"]["via"][0]["unknown"] = True
        mutations.append(report)

        report = self.v2_report()
        report["vulnerabilities"]["example"]["fixAvailable"] = {"unknown": True}
        mutations.append(report)

        report = self.v2_report()
        report["vulnerabilities"]["example"]["via"][0]["cvss"] = None
        mutations.append(report)

        report = self.v2_report()
        report["vulnerabilities"]["example"]["via"][0]["cvss"] = {
            "score": 5.0,
            "vectorString": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:L/A:N",
            "unknown": True,
        }
        mutations.append(report)

        report = self.v2_report()
        report["vulnerabilities"]["example"]["fixAvailable"] = None
        mutations.append(report)

        report = self.v2_report()
        report["metadata"]["unknown"] = True
        mutations.append(report)

        report = self.v2_report()
        report["actions"] = [{
            "action": "install",
            "module": "example",
            "target": "1.2.3",
            "isMajor": False,
            "resolves": [{"id": 1234, "path": "example", "dev": False, "optional": False, "bundled": False}],
            "unknown": True,
        }]
        mutations.append(report)

        report = self.v2_report()
        report["muted"] = [None]
        mutations.append(report)

        report = {
            "auditReportVersion": 1,
            "advisories": {
                "1234": {
                    "id": 1234,
                    "module_name": "example",
                    "severity": "high",
                    "title": "Example",
                    "url": "https://github.com/advisories/1234",
                    "findings": [{"version": "1.0.0", "paths": ["example"], "unknown": True}],
                }
            },
            "metadata": self.metadata("high"),
        }
        mutations.append(report)

        report = {
            "auditReportVersion": 1,
            "advisories": {
                "1234": {
                    "id": 1234,
                    "module_name": "example",
                    "severity": "high",
                    "title": "Example",
                    "url": "https://github.com/advisories/1234",
                    "findings": [{"version": "1.0.0", "paths": ["example"]}],
                    "unknown": True,
                }
            },
            "metadata": self.metadata("high"),
        }
        mutations.append(report)

        for report in mutations:
            with self.subTest(report=report):
                self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_metadata_count_sum_mismatch_fails_closed(self) -> None:
        report = self.v2_report()
        report["metadata"]["vulnerabilities"]["total"] = 0
        self.assertEqual(self.run_check(report, self.policy()), 1)

    def test_policy_types_and_canonical_expiry_are_strict(self) -> None:
        report = {"auditReportVersion": 2, "vulnerabilities": {}, "metadata": self.metadata()}
        invalid = [
            {"version": True, "audit_level": "high", "exceptions": []},
            {"version": 1, "audit_level": "high", "exceptions": [{"id": 1234, "package": "example", "expires_on": "2026-12-31", "reason": "x"}]},
            {"version": 1, "audit_level": "high", "exceptions": [{"id": "1234", "package": "example", "expires_on": "20260902", "reason": "x"}]},
            {"version": 1, "audit_level": "high", "exceptions": [{"id": "1234", "package": "example", "expires_on": "2026-12-31", "reason": "x", "extra": False}]},
        ]
        for policy in invalid:
            with self.subTest(policy=policy):
                self.assertEqual(self.run_check(report, policy), 1)

    def test_duplicate_policy_ids_are_rejected(self) -> None:
        exception = {"id": "1234", "package": "example", "expires_on": "2026-12-31", "reason": "x"}
        other_package = dict(exception, package="other")
        report = {"auditReportVersion": 2, "vulnerabilities": {}, "metadata": self.metadata()}
        self.assertEqual(self.run_check(report, self.policy([exception, other_package])), 1)

    def test_annotation_escapes_percent_and_line_endings(self) -> None:
        report = self.v2_report(title="bad%title\r\n::warning::injected")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(self.run_check(report, self.policy()), 1)
        output = stderr.getvalue()
        self.assertIn("bad%25title%0D%0A::warning::injected", output)
        self.assertNotIn("bad%title\r\n", output)

    def test_duplicate_json_keys_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            path.write_text('{"auditReportVersion":2,"auditReportVersion":2}', encoding="utf-8")
            with self.assertRaises(ValueError):
                load_json(path)

    def test_cli_path_restriction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            outside = Path(directory) / "audit.json"
            outside.write_text("{}", encoding="utf-8")
            policy = Path(__file__).resolve().parents[1] / "security/npm-audit-policy.json"
            with mock.patch.dict(os.environ, {}, clear=True):
                with self.assertRaises(ValueError):
                    validate_cli_paths(outside, policy)

    def test_runner_temp_is_an_allowed_report_root(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "audit.json"
            report.write_text("{}", encoding="utf-8")
            policy = Path(__file__).resolve().parents[1] / "security/npm-audit-policy.json"
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": directory}, clear=True):
                resolved_report, resolved_policy = validate_cli_paths(report, policy)
            self.assertEqual(resolved_report, report.resolve())
            self.assertEqual(resolved_policy, policy.resolve())


if __name__ == "__main__":
    unittest.main()
