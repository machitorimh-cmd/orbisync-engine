import re
import tempfile
import unittest
from pathlib import Path

from scripts import check_realtime_sample_contract as contract


class RealtimeSampleContractTests(unittest.TestCase):
    def copy_inputs(self, root: Path) -> None:
        for relative in contract._REQUIRED_FILES:
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            source = contract.ROOT / relative
            target.write_text(source.read_text(encoding="utf-8"), encoding="utf-8")

    def swap_ticket_branches(self, source: str) -> str:
        stripped = contract.strip_rust_comments(source)
        marker = re.search(r"\bif\s+config\.realtime\.allow_stub_ticket\b", stripped)
        self.assertIsNotNone(marker)
        assert marker is not None
        opening = stripped.find("{", marker.end())
        self.assertGreaterEqual(opening, 0)
        true_close = contract._matching_rust_brace(stripped, opening)
        self.assertIsNotNone(true_close)
        assert true_close is not None
        else_match = re.match(r"\s*else\s*\{", stripped[true_close + 1 :])
        self.assertIsNotNone(else_match)
        assert else_match is not None
        false_open = true_close + 1 + else_match.end() - 1
        false_close = contract._matching_rust_brace(stripped, false_open)
        self.assertIsNotNone(false_close)
        assert false_close is not None
        return (
            source[: opening + 1]
            + source[false_open + 1 : false_close]
            + source[true_close : false_open + 1]
            + source[opening + 1 : true_close]
            + source[false_close:]
        )

    def test_repository_contract_is_consistent(self) -> None:
        self.assertEqual(contract.check_contract(contract.ROOT), [])

    def test_route_drift_fails_but_comment_does_not_supply_a_route(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            main = root / "crates/orbisync-server/src/main.rs"
            text = main.read_text(encoding="utf-8")
            text, count = re.subn(
                r'\.route\(\s*"/v1/realtime/ws",\s*'
                r'axum::routing::get\(realtime_ws::realtime_ws_handler\),\s*\)',
                '// .route("/v1/realtime/ws", axum::routing::get(realtime_ws::realtime_ws_handler))',
                text,
            )
            self.assertEqual(count, 1)
            main.write_text(text, encoding="utf-8")
            errors = contract.check_contract(root)
            self.assertTrue(any("server realtime routes changed" in error for error in errors))

    def test_comments_cannot_fake_ticket_composition(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            main = root / "crates/orbisync-server/src/main.rs"
            text = main.read_text(encoding="utf-8")
            text = text.replace("HmacRealtimeTicketVerifier::new(", "/* HmacRealtimeTicketVerifier::new( */ OtherVerifier::new(")
            main.write_text(text, encoding="utf-8")
            errors = contract.check_contract(root)
            self.assertTrue(any("HmacRealtimeTicketVerifier" in error for error in errors))

    def test_reversed_ticket_polarity_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            main = root / "crates/orbisync-server/src/main.rs"
            text = main.read_text(encoding="utf-8")
            text = text.replace(
                "if config.realtime.allow_stub_ticket {",
                "if config.realtime.allow_stub_ticket == false {",
                1,
            )
            main.write_text(text, encoding="utf-8")
            errors = contract.check_contract(root)
            self.assertTrue(any("direct positive flag" in error for error in errors))

    def test_swapped_ticket_branches_fail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            main = root / "crates/orbisync-server/src/main.rs"
            text = main.read_text(encoding="utf-8")
            text = self.swap_ticket_branches(text)
            main.write_text(text, encoding="utf-8")
            errors = contract.check_contract(root)
            self.assertTrue(any("true allow_stub_ticket branch" in error for error in errors))
            self.assertTrue(any("false allow_stub_ticket branch" in error for error in errors))

    def test_missing_ticket_else_branch_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            main = root / "crates/orbisync-server/src/main.rs"
            text = main.read_text(encoding="utf-8")
            stripped = contract.strip_rust_comments(text)
            marker = re.search(r"\bif\s+config\.realtime\.allow_stub_ticket\b", stripped)
            self.assertIsNotNone(marker)
            assert marker is not None
            opening = stripped.find("{", marker.end())
            true_close = contract._matching_rust_brace(stripped, opening)
            self.assertIsNotNone(true_close)
            assert true_close is not None
            else_match = re.match(r"\s*else\s*\{", stripped[true_close + 1 :])
            self.assertIsNotNone(else_match)
            assert else_match is not None
            false_open = true_close + 1 + else_match.end() - 1
            text = text[: true_close + 1] + text[true_close + 1 :].replace("else", "", 1)
            main.write_text(text, encoding="utf-8")
            errors = contract.check_contract(root)
            self.assertTrue(
                any("if/else allow_stub_ticket branch" in error for error in errors)
            )

    def test_obsolete_sample_path_and_subprotocol_fail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            sample = root / "sdk/typescript/src/client.ts"
            text = sample.read_text(encoding="utf-8")
            text = text.replace("orbisync.v1.protobuf", "orbisync.realtime.v1")
            text = text.replace("/v1/realtime/tickets", "/realtime")
            sample.write_text(text, encoding="utf-8")
            errors = contract.check_contract(root)
            self.assertTrue(any("obsolete root WebSocket path" in error for error in errors))
            self.assertTrue(any("obsolete WebSocket subprotocol" in error for error in errors))

    def test_reference_sdk_subprotocol_drift_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_inputs(root)
            sdk = root / "sdk/typescript/src/client.ts"
            source = sdk.read_text(encoding="utf-8")
            sdk.write_text(
                source.replace(
                    'WEBSOCKET_SUBPROTOCOL = "orbisync.v1.protobuf"',
                    'WEBSOCKET_SUBPROTOCOL = "orbisync.realtime.v1"',
                    1,
                ),
                encoding="utf-8",
            )
            errors = contract.check_contract(root)
            self.assertTrue(any("SDK must use canonical realtime path and subprotocol" in error for error in errors))

    def test_valid_v1_realtime_paths_are_not_legacy_path_matches(self) -> None:
        self.assertIsNone(contract._LEGACY_WS_PATH.search("/v1/realtime/ws"))
        self.assertIsNone(contract._LEGACY_WS_PATH.search("POST /v1/realtime/tickets"))
        self.assertIsNotNone(contract._LEGACY_WS_PATH.search("ws://localhost:8080/realtime"))


if __name__ == "__main__":
    unittest.main()
