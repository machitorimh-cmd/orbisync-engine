import tempfile
import unittest
from pathlib import Path

from scripts import check_realtime_delivery_queue


class RealtimeDeliveryQueueGateTests(unittest.TestCase):
    def write_fixture(self, root: Path, delivery: str, runtime: str = "") -> None:
        source = root / "crates/orbisync-server/src"
        source.mkdir(parents=True)
        (source / "realtime_ws_connection_delivery.rs").write_text(delivery, encoding="utf-8")
        (source / "realtime_ws_connection_runtime.rs").write_text(runtime, encoding="utf-8")

    def test_accepts_interest_before_single_queue(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                """
                let Some(filtered) = filter_payload_for_viewer_with_roles(payload) else { return; };
                self.queue_payload(filtered);
                let queue = OutboundQueue::from_config(&state.config);
                """,
            )

            result = check_realtime_delivery_queue.inspect_delivery_queue(root)

            self.assertTrue(result["filter_before_queue"])
            self.assertFalse(result["raw_payload_queue_bypasses_filter"])
            self.assertEqual(result["outbound_queue_allocations"], 1)

    def test_rejects_interest_bypass_into_queue(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                """
                let Some(filtered) = filter_payload_for_viewer_with_roles(payload) else { return; };
                self.queue_payload(payload);
                let queue = OutboundQueue::from_config(&state.config);
                """,
            )

            result = check_realtime_delivery_queue.inspect_delivery_queue(root)

            self.assertTrue(result["raw_payload_queue_bypasses_filter"])

    def test_rejects_queue_before_interest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                """
                self.queue_payload(filtered);
                let Some(filtered) = filter_payload_for_viewer_with_roles(payload) else { return; };
                let queue = OutboundQueue::from_config(&state.config);
                """,
            )

            result = check_realtime_delivery_queue.inspect_delivery_queue(root)

            self.assertFalse(result["filter_before_queue"])

    def test_rejects_runtime_intermediate_channel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                "filter_payload_for_viewer_with_roles(payload); self.queue_payload(filtered);",
                "let delivery_rx = state.delivery.register(instance_id);",
            )

            result = check_realtime_delivery_queue.inspect_delivery_queue(root)

            self.assertTrue(result["runtime_delivery_rx"])
            self.assertTrue(result["runtime_registers_legacy_queue"])


if __name__ == "__main__":
    unittest.main()
