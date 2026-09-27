from __future__ import annotations

import shutil
import tempfile
import unittest
from pathlib import Path

from scripts import check_config_usage as checker

SOURCE_ROOT = Path(__file__).resolve().parents[1]
REAL_ROOT = SOURCE_ROOT  # the actual workspace


def _write_keys(root: Path, keys: list[str]) -> None:
    cfg_dir = root / "crates" / "orbisync-config" / "src"
    cfg_dir.mkdir(parents=True, exist_ok=True)
    body = ",\n    ".join(f'"{k}"' for k in keys)
    cfg_dir.joinpath("keys.rs").write_text(
        f"pub const CONFIG_KEYS: &[&str] = &[\n    {body},\n];\n", encoding="utf-8"
    )


def _write_production_file(root: Path, crate: str, filename: str, content: str) -> Path:
    path = root / "crates" / crate / "src" / filename
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return path


class ConfigUsageRegressionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name).resolve()
        # Minimal crates layout: orbisync-config with keys, plus one production crate
        _write_keys(self.root, ["server.bind", "server.public_url", "auth.token_signing_key_env"])
        _write_production_file(self.root, "orbisync-server", "lib.rs", "pub fn dummy() {}\n")

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def dead_keys(self) -> set[str]:
        dead, _ = checker.check_all(self.root)
        return set(dead.keys())

    def alive_keys(self) -> set[str]:
        _, alive = checker.check_all(self.root)
        return set(alive.keys())

    def test_dead_key_detected_when_no_reader(self) -> None:
        # No file references public_url or token_signing_key_env
        self.assertIn("server.public_url", self.dead_keys())
        self.assertIn("auth.token_signing_key_env", self.dead_keys())
        self.assertIn("server.bind", self.alive_keys() if False else self.dead_keys())  # initially dead
        # Make bind alive
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "fn f(c: &Config) { let _ = c.server.bind.clone(); }\n",
        )
        # Now bind should be alive because leaf `bind` appears as `.bind`
        # But note our checker looks for `.bind` which exists in `c.server.bind`
        self.assertIn("server.bind", self.alive_keys())

    def test_comment_only_does_not_count_as_reader(self) -> None:
        # File contains only a comment mentioning the key
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "// Production should read config.auth.token_signing_key_env\n"
            "// and also server.public_url is important\n"
            "fn f() {}\n",
        )
        self.assertIn("auth.token_signing_key_env", self.dead_keys())
        self.assertIn("server.public_url", self.dead_keys())

    def test_block_comment_only_does_not_count(self) -> None:
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "/*\n config.auth.token_signing_key_env should be read\n */\nfn f() {}\n",
        )
        self.assertIn("auth.token_signing_key_env", self.dead_keys())

    def test_string_literal_does_not_count_as_reader(self) -> None:
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            'fn f() { let s = "auth.token_signing_key_env and public_url"; }\n',
        )
        self.assertIn("auth.token_signing_key_env", self.dead_keys())
        self.assertIn("server.public_url", self.dead_keys())

    def test_raw_string_literal_does_not_count(self) -> None:
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            'fn f() { let s = r#"public_url"#; let t = r##"token_signing_key_env"##; }\n',
        )
        self.assertIn("server.public_url", self.dead_keys())
        self.assertIn("auth.token_signing_key_env", self.dead_keys())

    def test_field_access_counts_as_reader(self) -> None:
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "fn f(c: &Config) { let _ = c.auth.token_signing_key_env.clone(); }\n",
        )
        self.assertNotIn("auth.token_signing_key_env", self.dead_keys())
        self.assertIn("auth.token_signing_key_env", self.alive_keys())

    def test_dot_with_whitespace_counts(self) -> None:
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "fn f(c: &Config) { let _ = c . public_url.clone(); }\n",
        )
        self.assertNotIn("server.public_url", self.dead_keys())

    def test_mixed_comment_and_code(self) -> None:
        # Comment mentions dead key, but code reads a different key
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "// auth.token_signing_key_env is mentioned here\n"
            "fn f(c: &Config) { let _ = c.server.bind.clone(); }\n",
        )
        self.assertIn("auth.token_signing_key_env", self.dead_keys())
        self.assertNotIn("server.bind", self.dead_keys())

    def test_string_containing_dot_leaf_not_counted(self) -> None:
        # Ensure that a string like ".public_url" inside quotes does not count
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            'fn f() { let s = ".public_url"; }\n',
        )
        self.assertIn("server.public_url", self.dead_keys())

    def test_production_reader_must_be_outside_config_crate(self) -> None:
        # Write a file inside orbisync-config that contains a field access;
        # it must not count as production reader.
        cfg_file = self.root / "crates" / "orbisync-config" / "src" / "lib.rs"
        cfg_file.write_text("fn f(c: &Config) { let _ = c.server.public_url.clone(); }\n", encoding="utf-8")
        # Even though config crate has a reader, the key should still be dead
        # because we exclude that crate.
        self.assertIn("server.public_url", self.dead_keys())
        # Adding a reader in a real production crate makes it alive
        _write_production_file(
            self.root,
            "orbisync-identity",
            "lib.rs",
            "fn f(c: &Config) { let _ = c.server.public_url.clone(); }\n",
        )
        self.assertNotIn("server.public_url", self.dead_keys())

    def test_real_workspace_dead_keys_are_reported(self) -> None:
        # On the fixed main all dead keys have been wired or removed, so the
        # gate must be green (no dead keys). This test validates that the
        # current workspace is clean.
        dead, alive = checker.check_all(REAL_ROOT)
        self.assertEqual(dead, {}, f"gate must be green on fixed main, got dead={sorted(dead)}")
        # Verify that known alive keys are still reported as alive
        known_alive = {
            "server.bind",
            "database.url_env",
            "database.max_connections",
            "auth.pagination_hmac_key_env",
            "auth.allow_stub_bearer",
            "realtime.heartbeat_interval_seconds",
            "realtime.connection_timeout_seconds",
            "realtime.max_message_bytes",
            "realtime.outbound_queue_capacity",
            "realtime.allow_stub_ticket",
            "world.default_capacity",
            "world.server_tick_hz",
            "world.resume_grace_seconds",
            "world.checkpoint_interval_secs",
            "interest.cell_size",
            "interest.near_radius",
            "interest.unsubscribe_radius",
            "observability.log_format",
            "observability.log_level",
        }
        for key in known_alive:
            self.assertIn(key, alive, f"{key} should be alive, dead={sorted(dead)}")
        # All keys from CONFIG_KEYS should be alive
        expected_total = checker.parse_config_keys(REAL_ROOT)
        self.assertEqual(set(alive.keys()), set(expected_total), "all config keys should be alive")

    def test_strip_handles_comment_inside_string(self) -> None:
        # Ensure that // inside a string does not start a comment
        code = 'fn f() { let s = "a // b"; let _ = c.server.bind.clone(); }\n'
        _write_production_file(self.root, "orbisync-server", "main.rs", code)
        self.assertNotIn("server.bind", self.dead_keys())
        # And that string with // does not hide a real reader
        code2 = 'fn f() { let s = "public_url"; }\n'
        _write_production_file(self.root, "orbisync-server", "main.rs", code2)
        self.assertIn("server.public_url", self.dead_keys())

    def test_nested_block_comment(self) -> None:
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "/* outer /* inner public_url */ still comment */\nfn f() {}\n",
        )
        self.assertIn("server.public_url", self.dead_keys())
        # Real code after nested comment should still be detected
        _write_production_file(
            self.root,
            "orbisync-server",
            "main.rs",
            "/* comment */ fn f(c: &Config) { let _ = c.server.public_url.clone(); }\n",
        )
        self.assertNotIn("server.public_url", self.dead_keys())


if __name__ == "__main__":
    unittest.main()
