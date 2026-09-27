#!/usr/bin/env python3
"""Apply the repository npm advisory policy to an ``npm audit --json`` report.

The npm CLI's exit status is intentionally not used as the policy result.  The
report is parsed strictly because accepting an unknown report shape would turn
an advisory check into a fail-open check.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any


SEVERITY = {"info": 0, "low": 1, "moderate": 2, "high": 3, "critical": 4}
POLICY_LEVELS = {"low", "moderate", "high", "critical"}
METADATA_SEVERITIES = ("info", "low", "moderate", "high", "critical")
PACKAGE_NAME = re.compile(r"^(?:@[A-Za-z0-9._~-]+/)?[A-Za-z0-9._~-]+$")
CANONICAL_DATE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")


def _duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object key: {key}")
        value[key] = item
    return value


def _reject_json_constant(value: str) -> Any:
    raise ValueError(f"invalid JSON constant: {value}")


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=_duplicate_keys,
            parse_constant=_reject_json_constant,
        )
    except (OSError, UnicodeError, ValueError) as exc:
        raise ValueError(f"cannot read JSON file {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise ValueError(f"JSON document {path} must contain an object")
    return value


def _require_dict(value: Any, context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{context} must be an object")
    return value


def _require_list(value: Any, context: str) -> list[Any]:
    if not isinstance(value, list):
        raise ValueError(f"{context} must be an array")
    return value


def _require_string(value: Any, context: str, *, identifier: bool = False) -> str:
    if not isinstance(value, str) or not value or (identifier and value != value.strip()):
        raise ValueError(f"{context} must be a non-empty string")
    if identifier and any(ord(char) < 0x20 or ord(char) == 0x7F for char in value):
        raise ValueError(f"{context} contains a control character")
    return value


def _require_text(value: Any, context: str) -> str:
    """Require a JSON string, allowing the empty strings npm emits for ranges."""

    if not isinstance(value, str):
        raise ValueError(f"{context} must be a string")
    return value


def _require_keys(
    value: dict[str, Any],
    allowed: set[str],
    required: set[str],
    context: str,
) -> None:
    keys = set(value)
    if keys - allowed:
        raise ValueError(f"{context} has unknown fields")
    if required - keys:
        raise ValueError(f"{context} has missing fields")


def _require_nonnegative_int(value: Any, context: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{context} must be a non-negative integer")
    return value


def _require_positive_int(value: Any, context: str) -> int:
    result = _require_nonnegative_int(value, context)
    if result == 0:
        raise ValueError(f"{context} must be a positive integer")
    return result


def _require_severity(value: Any, context: str) -> str:
    if not isinstance(value, str) or value not in SEVERITY:
        raise ValueError(f"{context} has an invalid severity")
    return value


def _require_number(value: Any, context: str) -> int | float:
    try:
        finite = math.isfinite(value)
    except (TypeError, OverflowError):
        finite = False
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not finite:
        raise ValueError(f"{context} must be a finite number")
    return value


def _require_package(value: Any, context: str) -> str:
    package = _require_string(value, context, identifier=True)
    if not PACKAGE_NAME.fullmatch(package):
        raise ValueError(f"{context} is not a valid npm package name")
    return package


def _validate_cvss(value: Any, context: str) -> None:
    cvss = _require_dict(value, context)
    _require_keys(cvss, {"score", "vectorString"}, {"score", "vectorString"}, context)
    score = _require_number(cvss["score"], f"{context}.score")
    if not 0 <= score <= 10:
        raise ValueError(f"{context}.score must be between 0 and 10")
    _require_string(cvss["vectorString"], f"{context}.vectorString")


def _validate_fix_available(value: Any, context: str) -> None:
    if isinstance(value, bool):
        return
    fix_available = _require_dict(value, context)
    _require_keys(
        fix_available,
        {"name", "version", "isSemVerMajor"},
        {"name", "version", "isSemVerMajor"},
        context,
    )
    _require_package(fix_available["name"], f"{context}.name")
    _require_string(fix_available["version"], f"{context}.version")
    if not isinstance(fix_available["isSemVerMajor"], bool):
        raise ValueError(f"{context}.isSemVerMajor must be boolean")


def _validate_actions(report: dict[str, Any]) -> None:
    if "actions" not in report:
        return
    actions = _require_list(report["actions"], "npm audit report actions")
    for index, raw_action in enumerate(actions):
        context = f"npm audit report actions[{index}]"
        action = _require_dict(raw_action, context)
        _require_keys(action, {"action", "module", "target", "isMajor", "resolves"},
                      {"action", "module", "target", "isMajor", "resolves"}, context)
        if _require_string(action["action"], f"{context}.action") not in {
            "install", "update", "remove", "review"
        }:
            raise ValueError(f"{context}.action has an invalid value")
        _require_package(action["module"], f"{context}.module")
        _require_string(action["target"], f"{context}.target")
        if not isinstance(action["isMajor"], bool):
            raise ValueError(f"{context}.isMajor must be boolean")
        resolves = _require_list(action["resolves"], f"{context}.resolves")
        for resolve_index, raw_resolve in enumerate(resolves):
            resolve_context = f"{context}.resolves[{resolve_index}]"
            resolve = _require_dict(raw_resolve, resolve_context)
            _require_keys(resolve, {"id", "path", "dev", "optional", "bundled"},
                          {"id", "path", "dev", "optional", "bundled"}, resolve_context)
            _require_positive_int(resolve["id"], f"{resolve_context}.id")
            _require_string(resolve["path"], f"{resolve_context}.path")
            for flag in ("dev", "optional", "bundled"):
                if not isinstance(resolve[flag], bool):
                    raise ValueError(f"{resolve_context}.{flag} must be boolean")


def _validate_muted(report: dict[str, Any]) -> None:
    if "muted" not in report:
        return
    muted = _require_list(report["muted"], "npm audit report muted")
    # npm currently emits an empty array.  If it ever emits entries, they are
    # advisory IDs; accepting arbitrary objects here would make the gate
    # silently ignore malformed advisory data.
    for index, value in enumerate(muted):
        _require_positive_int(value, f"npm audit report muted[{index}]")


def _validate_metadata_shape(report: dict[str, Any], report_format: str) -> dict[str, int]:
    metadata = _require_dict(report.get("metadata"), "npm audit report metadata")
    if report_format == "v1":
        allowed_metadata = {
            "vulnerabilities", "dependencies", "devDependencies", "optionalDependencies",
            "totalDependencies",
        }
    else:
        allowed_metadata = {"vulnerabilities", "dependencies"}
    if set(metadata) - allowed_metadata:
        raise ValueError("npm audit report metadata has unknown fields")
    vulnerabilities = _require_dict(
        metadata.get("vulnerabilities"), "npm audit report metadata.vulnerabilities"
    )
    expected = set(METADATA_SEVERITIES) | {"total"}
    if set(vulnerabilities) != expected:
        raise ValueError("npm audit metadata.vulnerabilities has missing or unknown fields")
    counts = {
        severity: _require_nonnegative_int(
            vulnerabilities[severity], f"npm audit metadata.vulnerabilities.{severity}"
        )
        for severity in METADATA_SEVERITIES
    }
    total = _require_nonnegative_int(
        vulnerabilities["total"], "npm audit metadata.vulnerabilities.total"
    )
    if total != sum(counts.values()):
        raise ValueError("npm audit metadata vulnerability counts do not add up")

    if report_format == "v1":
        dependency_fields = {"dependencies", "devDependencies", "optionalDependencies", "totalDependencies"}
        if dependency_fields.intersection(metadata):
            _require_keys(metadata, allowed_metadata, {"vulnerabilities", *dependency_fields},
                          "npm audit report metadata")
            for name in dependency_fields:
                _require_nonnegative_int(metadata[name], f"npm audit metadata.{name}")
    elif "dependencies" in metadata:
        dependencies = metadata["dependencies"]
        dependencies = _require_dict(dependencies, "npm audit metadata.dependencies")
        allowed = {"prod", "dev", "optional", "peer", "peerOptional", "total"}
        _require_keys(dependencies, allowed, allowed, "npm audit metadata.dependencies")
        for name, value in dependencies.items():
            _require_nonnegative_int(value, f"npm audit metadata.dependencies.{name}")
    return {**counts, "total": total}


def _assert_metadata_matches(metadata: dict[str, int], severities: list[str]) -> None:
    expected = {severity: severities.count(severity) for severity in METADATA_SEVERITIES}
    expected["total"] = len(severities)
    if metadata != expected:
        raise ValueError(
            "npm audit metadata vulnerability counts do not match report entries"
        )


def _validate_v1_advisory_metadata(value: Any, context: str) -> None:
    advisory_metadata = _require_dict(value, context)
    _require_keys(
        advisory_metadata,
        {"module_type", "exploitability", "affected_components"},
        {"module_type", "exploitability", "affected_components"},
        context,
    )
    _require_text(advisory_metadata["module_type"], f"{context}.module_type")
    _require_nonnegative_int(advisory_metadata["exploitability"], f"{context}.exploitability")
    _require_text(advisory_metadata["affected_components"], f"{context}.affected_components")


def _validate_v1_person(value: Any, context: str) -> None:
    person = _require_dict(value, context)
    _require_keys(person, {"name", "link", "email"}, {"name"}, context)
    for field in person:
        _require_string(person[field], f"{context}.{field}")


def _validate_v1_advisory(advisory: dict[str, Any], key: str) -> tuple[str, str, str, str, str]:
    context = f"npm audit v1 advisory {key}"
    _require_keys(
        advisory,
        {
            "findings", "id", "created", "updated", "deleted", "title", "found_by",
            "reported_by", "module_name", "cves", "vulnerable_versions", "patched_versions",
            "overview", "recommendation", "references", "access", "severity", "cwe",
            "metadata", "url",
        },
        {"findings", "id", "title", "module_name", "severity", "url"},
        context,
    )
    advisory_id = _require_positive_int(advisory.get("id"), f"{context}.id")
    package = _require_package(advisory.get("module_name"), f"{context}.module_name")
    severity = _require_severity(advisory.get("severity"), context)
    title = _require_string(advisory.get("title"), f"{context}.title")
    url = _require_string(advisory.get("url"), f"{context}.url")
    if not (url.startswith("https://") or url.startswith("http://")):
        raise ValueError(f"{context}.url must be an HTTP(S) URL")

    findings = _require_list(advisory["findings"], f"{context}.findings")
    if not findings:
        raise ValueError(f"{context}.findings must not be empty")
    for finding_index, raw_finding in enumerate(findings):
        finding_context = f"{context}.findings[{finding_index}]"
        finding = _require_dict(raw_finding, finding_context)
        _require_keys(finding, {"version", "paths", "dev", "optional", "bundled"},
                      {"version", "paths"}, finding_context)
        _require_string(finding["version"], f"{finding_context}.version")
        paths = _require_list(finding["paths"], f"{finding_context}.paths")
        if not paths:
            raise ValueError(f"{finding_context}.paths must not be empty")
        for path_index, path in enumerate(paths):
            _require_string(path, f"{finding_context}.paths[{path_index}]")
        for flag in ("dev", "optional", "bundled"):
            if flag in finding and not isinstance(finding[flag], bool):
                raise ValueError(f"{finding_context}.{flag} must be boolean")

    string_fields = {
        "created", "updated", "deleted", "vulnerable_versions", "patched_versions",
        "overview", "recommendation", "references", "access",
    }
    for field in string_fields:
        if field in advisory:
            if field in {"vulnerable_versions", "patched_versions", "overview", "references"}:
                _require_text(advisory[field], f"{context}.{field}")
            else:
                _require_string(advisory[field], f"{context}.{field}")
    for field in ("found_by", "reported_by"):
        if field in advisory:
            _validate_v1_person(advisory[field], f"{context}.{field}")
    if "cves" in advisory:
        cves = _require_list(advisory["cves"], f"{context}.cves")
        for index, cve in enumerate(cves):
            _require_string(cve, f"{context}.cves[{index}]")
    if "cwe" in advisory:
        _require_string(advisory["cwe"], f"{context}.cwe")
    if "metadata" in advisory:
        _validate_v1_advisory_metadata(advisory["metadata"], f"{context}.metadata")
    return str(advisory_id), package, severity, title, url


def _validate_v1(report: dict[str, Any], threshold: str) -> list[dict[str, str]]:
    allowed = {"auditReportVersion", "actions", "advisories", "muted", "metadata", "runId"}
    _require_keys(report, allowed, {"auditReportVersion", "advisories", "metadata"},
                  "npm audit v1 report")
    version = report["auditReportVersion"]
    if isinstance(version, bool) or not isinstance(version, int) or version != 1:
        raise ValueError("npm audit advisories report has an invalid auditReportVersion")
    if "runId" in report:
        _require_string(report["runId"], "npm audit v1 report.runId")
    _validate_actions(report)
    _validate_muted(report)
    advisories = _require_dict(report.get("advisories"), "npm audit v1 advisories")
    metadata = _validate_metadata_shape(report, "v1")
    severities: list[str] = []
    findings: list[dict[str, str]] = []
    seen: set[tuple[str, str]] = set()
    for key, raw_advisory in advisories.items():
        if not isinstance(key, str) or not key.isdigit() or key != key.lstrip("0"):
            raise ValueError(f"npm audit v1 advisory key {key!r} is not canonical")
        advisory = _require_dict(raw_advisory, f"npm audit v1 advisory {key}")
        advisory_id, package, severity, title, url = _validate_v1_advisory(advisory, key)
        if advisory_id != key:
            raise ValueError(f"npm audit v1 advisory {key} id does not match its key")
        severities.append(severity)
        pair = (advisory_id, package)
        if pair in seen:
            raise ValueError(f"duplicate npm advisory: {pair[0]} for {pair[1]}")
        seen.add(pair)
        if SEVERITY[severity] >= SEVERITY[threshold]:
            findings.append(
                {"id": pair[0], "package": package, "severity": severity, "title": title, "url": url}
            )
    _assert_metadata_matches(metadata, severities)
    return findings


def _validate_v2_v3_via_advisory(
    raw_advisory: Any, package: str, context: str
) -> tuple[str, str, str, str, str]:
    if isinstance(raw_advisory, str):
        return f"package:{raw_advisory}", raw_advisory, "", "", ""
    advisory = _require_dict(raw_advisory, context)
    allowed_advisory_fields = {
        "source", "name", "dependency", "title", "url", "severity", "range", "cwe", "cvss"
    }
    _require_keys(
        advisory,
        allowed_advisory_fields,
        {"source", "name", "dependency", "title", "url", "severity", "range"},
        context,
    )
    source = _require_positive_int(advisory["source"], f"{context}.source")
    advisory_name = _require_package(advisory["name"], f"{context}.name")
    if advisory_name != package:
        raise ValueError(f"{context}.name does not match package {package}")
    dependency = _require_package(advisory["dependency"], f"{context}.dependency")
    if dependency != package:
        raise ValueError(f"{context}.dependency does not match package {package}")
    advisory_severity = _require_severity(advisory["severity"], context)
    title = _require_string(advisory["title"], f"{context}.title")
    url = _require_string(advisory["url"], f"{context}.url")
    if not (url.startswith("https://") or url.startswith("http://")):
        raise ValueError(f"{context}.url must be an HTTP(S) URL")
    advisory_range = _require_text(advisory["range"], f"{context}.range")
    if "cwe" in advisory:
        cwe = _require_list(advisory["cwe"], f"{context}.cwe")
        for cwe_index, cwe_id in enumerate(cwe):
            _require_string(cwe_id, f"{context}.cwe[{cwe_index}]")
    if "cvss" in advisory:
        _validate_cvss(advisory["cvss"], f"{context}.cvss")
    return str(source), advisory_severity, title, url, advisory_range


def _validate_v2_v3_vulnerability(
    package: str, raw_vulnerability: Any, threshold: str
) -> tuple[str, set[tuple[str, str]], list[dict[str, str]]]:
    context = f"npm audit vulnerability for {package}"
    vulnerability = _require_dict(raw_vulnerability, context)
    allowed_vulnerability_fields = {
        "name", "severity", "isDirect", "via", "effects", "range", "nodes", "fixAvailable"
    }
    _require_keys(
        vulnerability,
        allowed_vulnerability_fields,
        {"name", "severity", "isDirect", "via", "effects", "range", "nodes"},
        context,
    )
    name = _require_package(vulnerability["name"], f"{context}.name")
    if name != package:
        raise ValueError(f"npm audit vulnerability name does not match package key {package}")
    severity = _require_severity(vulnerability["severity"], context)
    if not isinstance(vulnerability["isDirect"], bool):
        raise ValueError(f"{context}.isDirect must be boolean")
    _require_text(vulnerability["range"], f"{context}.range")
    nodes = _require_list(vulnerability["nodes"], f"{context}.nodes")
    for node_index, node in enumerate(nodes):
        _require_text(node, f"{context}.nodes[{node_index}]")
    effects = _require_list(vulnerability["effects"], f"{context}.effects")
    for effect_index, effect in enumerate(effects):
        _require_package(effect, f"{context}.effects[{effect_index}]")
    if "fixAvailable" in vulnerability:
        _validate_fix_available(vulnerability["fixAvailable"], f"{context}.fixAvailable")
    via = _require_list(vulnerability["via"], f"{context}.via")
    if not via:
        raise ValueError(f"{context}.via must not be empty")
    seen: set[tuple[str, str]] = set()
    findings: list[dict[str, str]] = []
    for via_index, raw_advisory in enumerate(via):
        via_context = f"{context}.via[{via_index}]"
        if isinstance(raw_advisory, str):
            via_package = _require_package(raw_advisory, via_context)
            advisory_id = f"package:{via_package}"
            advisory_severity = severity
            title = ""
            url = ""
            advisory_range = ""
        else:
            advisory_id, advisory_severity, title, url, advisory_range = _validate_v2_v3_via_advisory(
                raw_advisory, package, via_context
            )
        pair = (advisory_id, package)
        if pair in seen:
            raise ValueError(f"duplicate npm advisory: {pair[0]} for {pair[1]}")
        seen.add(pair)
        effective_severity = max((severity, advisory_severity), key=lambda item: SEVERITY[item])
        if SEVERITY[effective_severity] >= SEVERITY[threshold]:
            findings.append(
                {
                    "id": pair[0],
                    "package": package,
                    "severity": effective_severity,
                    "title": title,
                    "url": url,
                    "range": advisory_range,
                }
            )
    return severity, seen, findings


def _validate_v2_v3(report: dict[str, Any], threshold: str) -> list[dict[str, str]]:
    allowed = {"auditReportVersion", "actions", "vulnerabilities", "muted", "metadata"}
    _require_keys(report, allowed, {"auditReportVersion", "vulnerabilities", "metadata"},
                  "npm audit v2/v3 report")
    version = report["auditReportVersion"]
    if isinstance(version, bool) or not isinstance(version, int) or version not in (2, 3):
        raise ValueError("npm audit vulnerabilities report has an invalid auditReportVersion")
    _validate_actions(report)
    _validate_muted(report)
    vulnerabilities = _require_dict(report["vulnerabilities"], "npm audit vulnerabilities")
    metadata = _validate_metadata_shape(report, "v2/v3")
    severities: list[str] = []
    findings: list[dict[str, str]] = []
    seen: set[tuple[str, str]] = set()
    for key, raw_vulnerability in vulnerabilities.items():
        package = _require_package(key, "npm audit vulnerability package")
        severity, vulnerability_seen, vulnerability_findings = _validate_v2_v3_vulnerability(
            package, raw_vulnerability, threshold
        )
        severities.append(severity)
        overlap = seen.intersection(vulnerability_seen)
        if overlap:
            pair = next(iter(overlap))
            raise ValueError(f"duplicate npm advisory: {pair[0]} for {pair[1]}")
        seen.update(vulnerability_seen)
        findings.extend(vulnerability_findings)
    _assert_metadata_matches(metadata, severities)
    return findings


def advisory_findings(report: dict[str, Any], threshold: str) -> list[dict[str, str]]:
    if threshold not in POLICY_LEVELS:
        raise ValueError(f"unsupported npm audit threshold: {threshold}")
    if not isinstance(report, dict):
        raise ValueError("npm audit report must be an object")
    if "error" in report:
        raise ValueError("npm audit report contains an error")
    has_advisories = "advisories" in report
    has_vulnerabilities = "vulnerabilities" in report
    if has_advisories == has_vulnerabilities:
        raise ValueError("npm audit report must contain exactly one known report format")
    if has_advisories:
        return _validate_v1(report, threshold)
    return _validate_v2_v3(report, threshold)


def validate_policy(policy: dict[str, Any], today: date) -> tuple[str, list[dict[str, str]]]:
    if not isinstance(policy, dict):
        raise ValueError("npm audit policy must be an object")
    if set(policy) != {"version", "audit_level", "exceptions"}:
        raise ValueError("npm audit policy has missing or unknown fields")
    if isinstance(policy.get("version"), bool) or not isinstance(policy.get("version"), int) or policy.get("version") != 1:
        raise ValueError("npm audit policy version must be 1")
    level = policy.get("audit_level")
    if not isinstance(level, str) or level not in POLICY_LEVELS:
        raise ValueError("npm audit policy audit_level must be low, moderate, high, or critical")
    raw_exceptions = policy.get("exceptions")
    if not isinstance(raw_exceptions, list):
        raise ValueError("npm audit policy exceptions must be an array")

    exceptions: list[dict[str, str]] = []
    seen_pairs: set[tuple[str, str]] = set()
    seen_ids: set[str] = set()
    for index, raw in enumerate(raw_exceptions):
        if not isinstance(raw, dict) or set(raw) != {"id", "package", "reason", "expires_on"}:
            raise ValueError(f"exception {index} has missing or unknown fields")
        advisory_id = _require_string(raw["id"], f"exception {index}.id", identifier=True)
        package = _require_package(raw["package"], f"exception {index}.package")
        reason = _require_string(raw["reason"], f"exception {index}.reason")
        expires_on = _require_string(raw["expires_on"], f"exception {index}.expires_on", identifier=True)
        if not CANONICAL_DATE.fullmatch(expires_on):
            raise ValueError(f"exception {index}.expires_on must be canonical YYYY-MM-DD UTC date")
        try:
            expiry = date.fromisoformat(expires_on)
        except ValueError as exc:
            raise ValueError(f"exception {advisory_id} has invalid expires_on: {expires_on}") from exc
        if expiry.isoformat() != expires_on:
            raise ValueError(f"exception {advisory_id}.expires_on is not canonical")
        if expiry < today:
            raise ValueError(f"npm audit exception {advisory_id} for {package} expired on {expires_on}")
        pair = (advisory_id, package)
        if pair in seen_pairs or advisory_id in seen_ids:
            raise ValueError(f"duplicate npm audit exception: {advisory_id} for {package}")
        seen_pairs.add(pair)
        seen_ids.add(advisory_id)
        exceptions.append({"id": advisory_id, "package": package, "reason": reason})
    return level, exceptions


def _github_escape(value: Any) -> str:
    """Escape untrusted text before putting it in a GitHub workflow command."""

    return str(value).replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _emit_error(message: Any) -> None:
    print(f"::error::{_github_escape(message)}", file=sys.stderr)


def check(report_path: Path, policy_path: Path, today: date | None = None) -> int:
    try:
        report = load_json(report_path)
        policy = load_json(policy_path)
        threshold, exceptions = validate_policy(
            policy, today or datetime.now(timezone.utc).date()
        )
        findings = advisory_findings(report, threshold)
    except ValueError as exc:
        _emit_error(exc)
        return 1

    exception_keys = {(item["id"], item["package"]): item for item in exceptions}
    unexpected = [
        finding
        for finding in findings
        if (finding["id"], finding["package"]) not in exception_keys
    ]
    matched = [
        finding
        for finding in findings
        if (finding["id"], finding["package"]) in exception_keys
    ]
    matched_keys = {(finding["id"], finding["package"]) for finding in matched}
    stale = [item for key, item in exception_keys.items() if key not in matched_keys]

    print(f"npm audit policy: {threshold}+ findings block; findings at/above threshold={len(findings)}")
    if matched:
        print(f"npm audit policy: {len(matched)} active exception(s) applied")
    if stale:
        for item in stale:
            _emit_error(
                f"npm audit exception {item['id']} for {item['package']} "
                "does not match the current report; remove or update it"
            )
    if unexpected:
        for finding in unexpected:
            location = f" ({finding['url']})" if finding["url"] else ""
            title = f": {finding['title']}" if finding["title"] else ""
            _emit_error(
                f"unexpected {finding['severity']} npm advisory "
                f"{finding['id']} in {finding['package']}{title}{location}"
            )
    return 1 if unexpected or stale else 0


def _is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def _resolve_file(path: Path, context: str) -> Path:
    try:
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise ValueError(f"{context} cannot be resolved: {path}: {exc}") from exc
    if not resolved.is_file():
        raise ValueError(f"{context} must be a regular file: {path}")
    return resolved


def validate_cli_paths(report_path: Path, policy_path: Path) -> tuple[Path, Path]:
    """Limit CLI reads to the checkout and the runner's temporary directory."""

    repository = Path(__file__).resolve().parents[1]
    report = _resolve_file(report_path, "npm audit report path")
    policy = _resolve_file(policy_path, "npm audit policy path")
    if not _is_within(policy, repository):
        raise ValueError("npm audit policy path must be inside the repository")
    allowed_report_roots = [repository]
    runner_temp = os.environ.get("RUNNER_TEMP")
    if runner_temp:
        allowed_report_roots.append(Path(runner_temp).resolve())
    if not any(_is_within(report, root) for root in allowed_report_roots):
        raise ValueError("npm audit report path must be inside the repository or RUNNER_TEMP")
    return report, policy


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="npm audit JSON report")
    parser.add_argument("policy", type=Path, help="repository npm audit policy JSON")
    args = parser.parse_args()
    try:
        report, policy = validate_cli_paths(args.report, args.policy)
    except ValueError as exc:
        _emit_error(exc)
        return 1
    return check(report, policy)


if __name__ == "__main__":
    raise SystemExit(main())
