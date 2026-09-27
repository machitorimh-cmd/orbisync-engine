from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.check_instance_task_ownership import inspect_instance_task_ownership


class InstanceTaskOwnershipTests(unittest.TestCase):
    def _workspace(self, main: str, realtime: str = "") -> Path:
        temp = Path(tempfile.mkdtemp())
        source = temp / "crates" / "orbisync-server" / "src"
        source.mkdir(parents=True)
        (source / "main.rs").write_text(main, encoding="utf-8")
        (source / "realtime_ws.rs").write_text(realtime, encoding="utf-8")
        return temp

    def test_accepts_task_owned_paths(self) -> None:
        root = self._workspace(
            "registry.ensure_instance(actor); registry.handles(); handle.tick(now, checkpoint_due);"
            "state.registry.submit(command);"
        )
        result = inspect_instance_task_ownership(root)
        self.assertEqual(result["legacy_registry_lock_bypasses"], [])
        self.assertTrue(result["task_spawn_boundary"])
        self.assertTrue(result["handle_tick_boundary"])
        self.assertTrue(result["handle_submit_boundary"])

    def test_rejects_registry_actor_access(self) -> None:
        root = self._workspace("state.registry.inner(); actor.submit(command);")
        result = inspect_instance_task_ownership(root)
        self.assertIn("registry.inner()", result["legacy_registry_lock_bypasses"])
        self.assertIn("actor.submit(", result["legacy_registry_lock_bypasses"])

    def test_rejects_tick_without_handle_iteration(self) -> None:
        root = self._workspace("registry.ensure_instance(actor); state.registry.submit(command);")
        result = inspect_instance_task_ownership(root)
        self.assertFalse(result["handle_tick_boundary"])

    def test_rejects_missing_task_spawn_boundary(self) -> None:
        root = self._workspace("registry.handles(); handle.tick(now, checkpoint_due);")
        result = inspect_instance_task_ownership(root)
        self.assertFalse(result["task_spawn_boundary"])


if __name__ == "__main__":
    unittest.main()
