import tempfile
import unittest
from pathlib import Path

from scripts import check_realtime_socket_writes


class RealtimeSocketWriteGateTests(unittest.TestCase):
    def write_fixture(self, root: Path, socket_source: str, connection_source: str = "") -> None:
        source = root / "crates/orbisync-server/src"
        source.mkdir(parents=True)
        (source / "realtime_ws_socket.rs").write_text(socket_source, encoding="utf-8")
        (source / "realtime_ws_connection.rs").write_text(connection_source, encoding="utf-8")

    def test_accepts_one_write_implementation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(root, "self.inner.send(message).await")

            result = check_realtime_socket_writes.inspect_socket_writes(root)

            self.assertEqual(result["socket_write_implementation_count"], 1)
            self.assertEqual(result["direct_write_bypasses"], [])

    def test_rejects_direct_write_bypass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                "self.inner.send(message).await",
                "let _ = socket.send(Message::Close(None)).await;",
            )

            result = check_realtime_socket_writes.inspect_socket_writes(root)

            self.assertEqual(result["socket_write_implementation_count"], 1)
            self.assertEqual(
                result["direct_write_bypasses"],
                ["crates/orbisync-server/src/realtime_ws_connection.rs:1"],
            )

    def test_rejects_message_variant_bypass_outside_socket_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                "self.inner.send(message).await",
                "socket.send(Message::Binary(bytes)).await;",
            )

            result = check_realtime_socket_writes.inspect_socket_writes(root)

            self.assertEqual(
                result["direct_write_bypasses"],
                ["crates/orbisync-server/src/realtime_ws_connection.rs:1"],
            )

    def test_rejects_variable_message_bypass_outside_socket_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                "self.inner.send(message).await",
                "let outbound = Message::Text(text);\nsocket.send(outbound).await;",
            )

            result = check_realtime_socket_writes.inspect_socket_writes(root)

            self.assertEqual(
                result["direct_write_bypasses"],
                ["crates/orbisync-server/src/realtime_ws_connection.rs:2"],
            )

    def test_ignores_channel_send_without_message_payload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(
                root,
                "self.inner.send(message).await",
                "status_tx.send(true).await;\nstarted_tx.send(()).await;",
            )

            result = check_realtime_socket_writes.inspect_socket_writes(root)

            self.assertEqual(result["direct_write_bypasses"], [])


if __name__ == "__main__":
    unittest.main()
