"""Owned socket/process regressions for the local-trial recovery smoke."""
import errno
import os
from pathlib import Path
import queue
import signal
import socket
import subprocess
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

if __package__:
    from . import verify_local_trial_recovery as recovery
else:
    import verify_local_trial_recovery as recovery

listen = recovery.listen


class RecoveryListenerTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "Linux TIME_WAIT regression")
    def test_reuses_stopped_listener_port_in_time_wait(self):
        # Match a reusable Unix server, then actively close its accepted side.
        # This leaves the server's tuple in TIME_WAIT after the listener exits.
        with socket.socket() as previous:
            previous.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            previous.bind(("127.0.0.1", 0))
            previous.listen(1)
            port = previous.getsockname()[1]
            with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
                accepted, _ = previous.accept()
                with accepted:
                    accepted.shutdown(socket.SHUT_WR)
                    self.assertEqual(client.recv(1), b"")

        # Prove this is the original Linux failure condition, not just a free
        # port. The recovery fixture must still bind while that tuple exists.
        with socket.socket() as without_reuse:
            with self.assertRaises(OSError) as raised:
                without_reuse.bind(("127.0.0.1", port))
            self.assertEqual(raised.exception.errno, errno.EADDRINUSE)
        with listen(port) as foreign:
            self.assertEqual(foreign.getsockname()[1], port)
            self.assertEqual(foreign.getsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR), 1)

    def test_does_not_replace_a_live_listener(self):
        with listen(0) as original:
            port = original.getsockname()[1]
            with self.assertRaises(OSError):
                listen(port)
            with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
                accepted, _ = original.accept()
                accepted.close()
                self.assertEqual(client.recv(1), b"")

    def test_closes_socket_when_bind_fails(self):
        with patch.object(recovery.socket, "socket") as create_socket:
            server = create_socket.return_value
            server.bind.side_effect = OSError(errno.EADDRINUSE, "fixture_port_busy")
            with self.assertRaises(OSError):
                listen(12345)
            server.close.assert_called_once_with()
            server.listen.assert_not_called()


@unittest.skipUnless(sys.platform == "linux", "Linux thread-owned children regression")
class RecoveryOwnedProcessTests(unittest.TestCase):
    def test_finds_and_terminates_engine_spawned_by_worker_thread(self):
        spawned = queue.Queue()
        release_worker = threading.Event()

        def spawn_from_worker():
            try:
                child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)", "managed-serve"],
                                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                spawned.put(child)
            except BaseException as error:
                spawned.put(error)
            release_worker.wait(20)

        worker = threading.Thread(target=spawn_from_worker)
        worker.start()
        engine = unrelated = None
        try:
            engine = spawned.get(timeout=10)
            if isinstance(engine, BaseException):
                raise engine
            unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            parent_pid = os.getpid()
            leader_children = (Path("/proc") / str(parent_pid) / "task" / str(parent_pid) / "children").read_text().split()
            self.assertNotIn(str(engine.pid), leader_children)
            binary = Path(sys.executable)
            self.assertEqual(recovery.linux_owned_engine_candidates(parent_pid, binary), [engine.pid])
            self.assertFalse(recovery.linux_owned_engine_matches(engine.pid, parent_pid + 1, binary))
            self.assertFalse(recovery.linux_owned_engine_matches(engine.pid, parent_pid, Path("/not-this-fixture")))

            recovery.terminate_owned_engine(SimpleNamespace(process=SimpleNamespace(pid=parent_pid)), binary)
            self.assertEqual(engine.wait(timeout=5), -signal.SIGKILL)
            self.assertIsNone(unrelated.poll())
        finally:
            for child in (engine, unrelated):
                if isinstance(child, subprocess.Popen):
                    if child.poll() is None:
                        child.kill()
                    child.wait(timeout=5)
            release_worker.set()
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())


if __name__ == "__main__":
    unittest.main()
