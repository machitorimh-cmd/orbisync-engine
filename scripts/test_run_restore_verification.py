from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.run_restore_verification import run_verification


class RestoreVerificationRunnerTests(unittest.TestCase):
    def args(self, root: Path, **overrides: object) -> argparse.Namespace:
        drill = root / "restore-drill.sh"
        drill.write_text("#!/usr/bin/env bash\n", encoding="utf-8")
        values: dict[str, object] = {
            "backup_id": "backup-2026-09-22T030000Z",
            "backup_file": None,
            "backup_sha256": None,
            "login_id": None,
            "password_file": None,
            "history": root / "history.jsonl",
            "drill_script": drill,
            "bash": "bash",
        }
        values.update(overrides)
        return argparse.Namespace(**values)

    def test_success_and_failure_are_both_appended(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            args = self.args(root)

            def passed(*_args: object, **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
                return subprocess.CompletedProcess([], 0)

            def failed(*_args: object, **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
                return subprocess.CompletedProcess([], 7)

            self.assertEqual(run_verification(args, passed), 0)
            self.assertEqual(run_verification(args, failed), 7)
            rows = [
                json.loads(line)
                for line in args.history.read_text(encoding="utf-8").splitlines()
            ]
            self.assertEqual([row["status"] for row in rows], ["passed", "failed"])
            self.assertEqual([row["exit_code"] for row in rows], [0, 7])
            self.assertTrue(all(row["backup_id"] == args.backup_id for row in rows))
            self.assertTrue(all(row["duration_ms"] >= 0 for row in rows))
            self.assertTrue(all(row["started_at"].endswith("Z") for row in rows))
            self.assertTrue(all(row["completed_at"].endswith("Z") for row in rows))

    def test_external_backup_inputs_are_forwarded_without_password_contents(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            backup = root / "backup.dump"
            password = root / "password.txt"
            backup.write_bytes(b"dump")
            password.write_text("do-not-log-this", encoding="utf-8")
            args = self.args(
                root,
                backup_file=backup,
                backup_sha256="a" * 64,
                login_id="restore_admin",
                password_file=password,
            )
            captured: dict[str, object] = {}

            def runner(command: object, **kwargs: object) -> subprocess.CompletedProcess[bytes]:
                captured["command"] = command
                captured["env"] = kwargs["env"]
                return subprocess.CompletedProcess([], 0)

            self.assertEqual(run_verification(args, runner), 0)
            env = captured["env"]
            self.assertIsInstance(env, dict)
            assert isinstance(env, dict)
            self.assertEqual(env["RESTORE_DRILL_BACKUP_FILE"], str(backup.resolve()))
            self.assertEqual(env["RESTORE_DRILL_PASSWORD_FILE"], str(password.resolve()))
            self.assertNotIn("do-not-log-this", json.dumps(env))
            row = json.loads(args.history.read_text(encoding="utf-8"))
            self.assertEqual(row["verification_mode"], "external_backup")

    def test_external_backup_requires_smoke_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            backup = root / "backup.dump"
            backup.write_bytes(b"dump")
            args = self.args(root, backup_file=backup)
            with self.assertRaisesRegex(ValueError, "requires --login-id"):
                run_verification(args)
            self.assertFalse(args.history.exists())

    def test_invalid_backup_id_is_rejected_before_launch(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            args = self.args(root, backup_id="contains a space")
            with self.assertRaisesRegex(ValueError, "backup ID"):
                run_verification(args)
            self.assertFalse(args.history.exists())


if __name__ == "__main__":
    unittest.main()
