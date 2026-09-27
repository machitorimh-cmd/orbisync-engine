from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from scripts import validate_design as validator


SOURCE_ROOT = Path(__file__).resolve().parents[1]


class ValidatorRegressionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name).resolve()
        for name in (
            "README.md", "CONTRIBUTING.md", "SECURITY.md",
            "metaverse_core_specification.md", "contracts", "docs",
            "openapi", "proto", "test-vectors",
        ):
            source = SOURCE_ROOT / name
            target = self.root / name
            if source.is_dir():
                shutil.copytree(source, target)
            else:
                shutil.copy2(source, target)
        self.original_root = validator.ROOT
        self.original_markdown = validator.MARKDOWN
        validator.ROOT = self.root
        validator.MARKDOWN = [self.root / "README.md", self.root / "CONTRIBUTING.md", self.root / "SECURITY.md"]
        validator.MARKDOWN += sorted((self.root / "docs").rglob("*.md"))

    def tearDown(self) -> None:
        validator.ROOT = self.original_root
        validator.MARKDOWN = self.original_markdown
        self.tempdir.cleanup()

    def contract_errors(self) -> list[str]:
        errors: list[str] = []
        validator.check_contracts(errors)
        return errors

    def test_proto_and_wire_table_drift_is_rejected(self) -> None:
        path = self.root / "proto/orbisync/v1/realtime.proto"
        text = path.read_text(encoding="utf-8").replace("    ErrorMessage error = 34;\n", "")
        path.write_text(text, encoding="utf-8")
        self.assertTrue(any("wire payload table differs" in error for error in self.contract_errors()))

    def test_client_hello_required_field_omission_is_rejected(self) -> None:
        path = self.root / "proto/orbisync/v1/realtime.proto"
        text = path.read_text(encoding="utf-8").replace("  string client_type = 6;\n", "")
        path.write_text(text, encoding="utf-8")
        self.assertTrue(any("client_type references missing Proto field" in error for error in self.contract_errors()))

        text = path.read_text(encoding="utf-8").replace("  repeated string enabled_features = 6;\n", "")
        path.write_text(text, encoding="utf-8")
        self.assertTrue(any("features references missing Proto field" in error for error in self.contract_errors()))

    def test_hello_requirement_table_drift_is_rejected(self) -> None:
        path = self.root / "docs/design/realtime-protocol-and-connection.md"
        text = path.read_text(encoding="utf-8").replace("`ClientHello.client_type`", "`ClientHello.client_name`")
        path.write_text(text, encoding="utf-8")
        self.assertTrue(any("ClientHello requirement table differs" in error for error in self.contract_errors()))

    def test_protocol_vector_rule_violation_is_rejected(self) -> None:
        path = self.root / "test-vectors/protocol/v1/envelope-valid.json"
        vector = json.loads(path.read_text(encoding="utf-8"))
        vector["envelope"]["sequence"] = 0
        vector["envelope"]["client_hello"].pop("client_type")
        path.write_text(json.dumps(vector), encoding="utf-8")
        errors = self.contract_errors()
        self.assertTrue(any("sequence must start at 1" in error for error in errors))
        self.assertTrue(any("ClientHello vector must cover all Proto fields" in error for error in errors))

        server_path = self.root / "test-vectors/protocol/v1/server-hello-valid.json"
        server = json.loads(server_path.read_text(encoding="utf-8"))
        server["envelope"]["server_hello"].pop("enabled_features")
        server_path.write_text(json.dumps(server), encoding="utf-8")
        self.assertTrue(any("ServerHello vector must cover all Proto fields" in error for error in self.contract_errors()))

    def test_state_transition_drift_and_unreachable_state_are_rejected(self) -> None:
        path = self.root / "contracts/realtime-connection-state-machine.json"
        machine = json.loads(path.read_text(encoding="utf-8"))
        machine["states"].append("orphaned")
        machine["transitions"].pop()
        path.write_text(json.dumps(machine), encoding="utf-8")
        errors = self.contract_errors()
        self.assertTrue(any("unreachable states" in error for error in errors))
        self.assertTrue(any("transition table differs" in error for error in errors))

    def test_required_adr_must_be_accepted(self) -> None:
        path = self.root / "docs/adr/ADR-003-rest-api.md"
        path.write_text(path.read_text(encoding="utf-8").replace("Status: Accepted", "Status: Proposed"), encoding="utf-8")
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_adrs(texts, errors)
        self.assertTrue(any("required ADR must have Status: Accepted" in error or "Proposed ADR must have Decision trigger" in error for error in errors))

    def test_expected_adrs_derived_covers_all(self) -> None:
        # Verify that _discover_expected_adrs finds the repository's ADR set and
        # that check_adrs enforces Accepted (or Proposed with trigger) for all of
        # them. The count tracks the ADR files actually present; it had already
        # drifted to 26 before ADR-026 was added, which makes it 27; the ownership transfer ADR makes 28.
        discovered = validator._discover_expected_adrs(self.root)
        self.assertIn("ADR-010", discovered)
        self.assertIn("ADR-015", discovered)
        self.assertIn("ADR-017", discovered)
        self.assertIn("ADR-018", discovered)
        self.assertEqual(len(discovered), 28)
        # Adding a new ADR without Accepted/Proposed should be rejected
        new_adr = self.root / "docs/adr/ADR-099-test.md"
        new_adr.write_text("# ADR-099\n\n- Status: Draft\n", encoding="utf-8")
        validator.MARKDOWN += [new_adr]
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_adrs(texts, errors)
        self.assertTrue(any("ADR-099" in e or "required ADR" in e for e in errors))

    def test_adr_design_contradiction_is_detected(self) -> None:
        # Simulate W-27 real case: ADR says enforce (拒否), design says advisory => RED
        adr15 = self.root / "docs/adr/ADR-015-local-credential-security.md"
        adr16 = self.root / "docs/adr/ADR-016-identity-administration-contract.md"
        # Replace advisory lines with enforce in ADR
        for path in [adr15, adr16]:
            text = path.read_text(encoding="utf-8")
            text = text.replace("ブロックしない（advisory）", "拒否する")
            text = text.replace("advisoryとし、サーバーは状態を通知するのみで操作をブロックしない", "中はchange-password/logout以外を拒否する")
            # Ensure no advisory marker remains on must_change lines
            text = text.replace("advisory", "enforce")
            path.write_text(text, encoding="utf-8")
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_adr_design_consistency(texts, errors)
        self.assertTrue(any("ADR/design contradiction" in e and "must_change_password" in e for e in errors))
        # Restoring advisory should be green
        for path in [adr15, adr16]:
            # Reload from source
            src = SOURCE_ROOT / path.relative_to(self.root)
            path.write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
        errors = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_adr_design_consistency(texts, errors)
        self.assertFalse(any("ADR/design contradiction" in e for e in errors))

    def acceptance_errors(self) -> list[str]:
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_acceptance_implementation_consistency(texts, errors)
        return errors

    def drop_refresh_token_from_login_response(self) -> None:
        """Remove refresh_token from LoginResponse only, leaving TokenPair intact."""
        path = self.root / "openapi/orbisync-v1.yaml"
        text = path.read_text(encoding="utf-8")
        block = validator._yaml_block(text, "LoginResponse")
        self.assertIsNotNone(block)
        assert block is not None
        kept: list[str] = []
        skip_indent: int | None = None
        for line in block.splitlines(keepends=True):
            indent = len(line) - len(line.lstrip(" "))
            if skip_indent is not None:
                if line.strip() and indent > skip_indent:
                    continue
                skip_indent = None
            if line.strip() == "- refresh_token":
                continue
            if line.strip() == "refresh_token:":
                skip_indent = indent
                continue
            kept.append(line)
        path.write_text(text.replace(block, "".join(kept), 1), encoding="utf-8")

    def test_yaml_block_stops_at_the_next_sibling_entry(self) -> None:
        text = (self.root / "openapi/orbisync-v1.yaml").read_text(encoding="utf-8")
        block = validator._yaml_block(text, "LoginResponse")
        self.assertIsNotNone(block)
        assert block is not None
        self.assertIn("refresh_token", block)
        # The body must not bleed into the schema declared after it.
        self.assertNotIn("TokenPair", block)
        # An absent key yields None, never an empty body, so a caller cannot
        # mistake a failed extraction for a genuinely missing field.
        self.assertIsNone(validator._yaml_block(text, "NoSuchSchemaHere"))

    def test_refresh_token_acceptance_contradiction_is_detected(self) -> None:
        design = self.root / "docs/design/auth-authorization.md"
        text = design.read_text(encoding="utf-8")
        # A claim without a planned marker, against a contract that does not
        # deliver refresh_token => RED.
        text_without_planned = text.replace("planned", "future").replace("未配信", "future").replace("将来", "future")
        design.write_text(text_without_planned, encoding="utf-8")
        self.drop_refresh_token_from_login_response()
        self.assertTrue(any("acceptance criteria declares refresh token" in e for e in self.acceptance_errors()))
        # The same claim is green once the contract delivers refresh_token.
        # This is the case a fixed-indent extraction reported as a false
        # positive, because the empty block it produced looked like absence.
        shutil.copy2(SOURCE_ROOT / "openapi/orbisync-v1.yaml", self.root / "openapi/orbisync-v1.yaml")
        self.assertFalse(any("acceptance criteria declares refresh token" in e for e in self.acceptance_errors()))
        # Restoring the planned marker is green as well.
        src = SOURCE_ROOT / "docs/design/auth-authorization.md"
        design.write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
        self.assertFalse(any("acceptance criteria declares refresh token" in e for e in self.acceptance_errors()))

    def test_blanket_suppression_is_rejected(self) -> None:
        adr = self.root / "docs/adr/ADR-015-local-credential-security.md"
        text = adr.read_text(encoding="utf-8")
        text += "\n<!-- validate-design: suppress all -->\n"
        adr.write_text(text, encoding="utf-8")
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_adr_design_consistency(texts, errors)
        self.assertTrue(any("blanket validate-design suppression" in e for e in errors))

    def test_identifier_specific_suppression_is_allowed(self) -> None:
        # Identifier-specific suppression should silence the contradiction
        adr15 = self.root / "docs/adr/ADR-015-local-credential-security.md"
        text = adr15.read_text(encoding="utf-8")
        text = text.replace("ブロックしない（advisory）", "拒否する")
        text += "\n<!-- validate-design: suppress must_change_password -->\n"
        adr15.write_text(text, encoding="utf-8")
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_adr_design_consistency(texts, errors)
        # Should not report contradiction because suppressed
        self.assertFalse(any("ADR/design contradiction" in e for e in errors))
        self.assertFalse(any("blanket" in e for e in errors))

    def test_broken_markdown_fragment_is_rejected(self) -> None:
        source = self.root / "README.md"
        source.write_text(source.read_text(encoding="utf-8") + "\n[bad](docs/design/architecture.md#missing-anchor)\n", encoding="utf-8")
        errors: list[str] = []
        texts = {path: validator.load_text(path, errors) for path in validator.MARKDOWN if path.exists()}
        validator.check_links(texts, errors)
        self.assertTrue(any("broken Markdown fragment" in error for error in errors))

    def test_realtime_ticket_schema_drift_is_rejected(self) -> None:
        # Implemented contract uses TicketResponse with realtime_ticket/expires_in (truthful).
        # Mutate that shape to prove the gate catches drift.
        path = self.root / "openapi/orbisync-v1.yaml"
        text = path.read_text(encoding="utf-8")
        # Handle both flow `{type: string}` and block `type: string` styles (W-29 reformatted to block)
        if "realtime_ticket: {type: string}" in text:
            text = text.replace("realtime_ticket: {type: string}", "realtime_ticket: {type: integer}")
        else:
            # block style: realtime_ticket:\n          type: string
            import re

            text = re.sub(
                r"realtime_ticket:\s*\n\s*type:\s*string",
                "realtime_ticket:\n          type: integer",
                text,
            )
        path.write_text(text, encoding="utf-8")
        self.assertTrue(any("realtime ticket contract missing" in error for error in self.contract_errors()))

    def test_transport_loss_summary_drift_is_rejected(self) -> None:
        path = self.root / "docs/design/realtime-protocol-and-connection.md"
        text = path.read_text(encoding="utf-8").replace("、`failing → failed`", "")
        path.write_text(text, encoding="utf-8")
        self.assertTrue(any("transport loss summary differs from JSON" in error for error in self.contract_errors()))

    def test_mobile_sdk_state_name_drift_is_rejected(self) -> None:
        path = self.root / "docs/design/client-sdk.md"
        text = path.read_text(encoding="utf-8").replace("Reconnecting", "Restoring")
        path.write_text(text, encoding="utf-8")
        errors = self.contract_errors()
        self.assertTrue(any("states not declared by client SDK" in error for error in errors))

    def test_resume_policy_missing_case_is_rejected(self) -> None:
        path = self.root / "contracts/realtime-resume-token-policy.json"
        policy = json.loads(path.read_text(encoding="utf-8"))
        policy["cases"] = [case for case in policy["cases"] if case["case"] != "neither"]
        path.write_text(json.dumps(policy), encoding="utf-8")
        errors = self.contract_errors()
        self.assertTrue(any("input combinations are incomplete" in error for error in errors))

    def test_resume_policy_duplicate_input_is_rejected(self) -> None:
        path = self.root / "contracts/realtime-resume-token-policy.json"
        policy = json.loads(path.read_text(encoding="utf-8"))
        duplicate = dict(policy["cases"][0])
        duplicate["case"] = "duplicate_client_hello_only"
        policy["cases"].append(duplicate)
        path.write_text(json.dumps(policy), encoding="utf-8")
        self.assertTrue(any("duplicate resume policy input combinations" in error for error in self.contract_errors()))

    def test_resume_policy_duplicate_case_name_is_rejected(self) -> None:
        path = self.root / "contracts/realtime-resume-token-policy.json"
        policy = json.loads(path.read_text(encoding="utf-8"))
        policy["cases"].append(dict(policy["cases"][0]))
        path.write_text(json.dumps(policy), encoding="utf-8")
        self.assertTrue(any("duplicate resume policy case names" in error for error in self.contract_errors()))

    def test_resume_policy_unknown_input_leaves_required_combination_uncovered(self) -> None:
        path = self.root / "contracts/realtime-resume-token-policy.json"
        policy = json.loads(path.read_text(encoding="utf-8"))
        neither = next(case for case in policy["cases"] if case["case"] == "neither")
        neither["tokens_equal"] = True
        path.write_text(json.dumps(policy), encoding="utf-8")
        self.assertTrue(any("input combinations are incomplete" in error for error in self.contract_errors()))

    def test_resume_policy_expected_result_change_is_rejected(self) -> None:
        json_path = self.root / "contracts/realtime-resume-token-policy.json"
        policy = json.loads(json_path.read_text(encoding="utf-8"))
        mismatch = next(case for case in policy["cases"] if case["case"] == "both_mismatch")
        mismatch["decision"] = "accept_resume_request"
        mismatch["next_state"] = "resuming"
        mismatch["error"] = None
        json_path.write_text(json.dumps(policy), encoding="utf-8")

        doc_path = self.root / "docs/design/realtime-protocol-and-connection.md"
        doc = doc_path.read_text(encoding="utf-8").replace(
            "| `both_mismatch` | `true` | `true` | `false` | `reject_resume` | `ready` | `resume_token_mismatch` |",
            "| `both_mismatch` | `true` | `true` | `false` | `accept_resume_request` | `resuming` | `null` |",
        )
        doc_path.write_text(doc, encoding="utf-8")
        self.assertTrue(any("expected results differ" in error for error in self.contract_errors()))

    def test_pascal_case_prose_does_not_trigger_server_state_check(self) -> None:
        path = self.root / "docs/design/realtime-protocol-and-connection.md"
        path.write_text(path.read_text(encoding="utf-8") + "\nActive connections are observable.\n", encoding="utf-8")
        self.assertFalse(any("server state identifiers" in error for error in self.contract_errors()))

        path.write_text(path.read_text(encoding="utf-8") + "\n`Active`\n", encoding="utf-8")
        self.assertTrue(any("server state identifiers" in error for error in self.contract_errors()))


if __name__ == "__main__":
    unittest.main()
