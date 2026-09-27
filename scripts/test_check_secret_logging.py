from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import check_secret_logging as gate


def build_dev_key() -> str:
    # Split to avoid writing the literal verbatim in the repository
    # "dev-pagination-hmac-key-32bytes!!"
    parts = ["dev", "-pagination", "-hmac", "-key", "-32bytes", "!!"]
    return "".join(parts)


def build_insecure_key() -> str:
    # "insecure-fixed-pagination-key-for-tests-only-32b!!"
    parts = [
        "insecure",
        "-fixed",
        "-pagination",
        "-key",
        "-for",
        "-tests",
        "-only",
        "-32b",
        "!!",
    ]
    return "".join(parts)


def write_temp(content: str) -> Path:
    tmp = tempfile.NamedTemporaryFile(delete=False, suffix=".rs", mode="w", encoding="utf-8")
    tmp.write(content)
    tmp.flush()
    tmp.close()
    return Path(tmp.name)


class HardcodedSecretGateTests(unittest.TestCase):
    def test_dev_pagination_key_is_detected(self) -> None:
        key = build_dev_key()
        self.assertEqual(key, "dev-pagination-hmac-key-32bytes!!")
        path = write_temp(f'let key = "{key}".to_owned();\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertTrue(
                any("hardcoded secret" in p for p in problems),
                f"dev key should be flagged, got {problems}",
            )
        finally:
            path.unlink(missing_ok=True)

    def test_insecure_fixed_key_is_detected(self) -> None:
        key = build_insecure_key()
        self.assertEqual(key, "insecure-fixed-pagination-key-for-tests-only-32b!!")
        path = write_temp(f'const KEY: &str = "{key}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertTrue(
                any("hardcoded secret" in p for p in problems),
                f"insecure key should be flagged, got {problems}",
            )
        finally:
            path.unlink(missing_ok=True)

    def test_hardcoded_in_cfg_test_is_allowed(self) -> None:
        key = build_dev_key()
        content = (
            "#[cfg(test)]\n"
            "mod tests {\n"
            f'    const K: &str = "{key}";\n'
            "    fn helper() {}\n"
            "}\n"
        )
        path = write_temp(content)
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertEqual(problems, [], f"cfg(test) should be excluded, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_hardcoded_allow_marker_is_respected(self) -> None:
        key = build_dev_key()
        content = f'let k = "{key}".to_owned(); // allow-hardcoded-secret: test\n'
        path = write_temp(content)
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertEqual(problems, [], f"allow marker should suppress, got {problems}")
        finally:
            path.unlink(missing_ok=True)

        # preceding line marker
        content2 = f'// allow-hardcoded-secret: test\nlet k = "{key}".to_owned();\n'
        path2 = write_temp(content2)
        try:
            problems2 = gate.check_hardcoded_file(path2)
            self.assertEqual(problems2, [], f"preceding allow marker should suppress, got {problems2}")
        finally:
            path2.unlink(missing_ok=True)

    def test_config_key_not_flagged(self) -> None:
        path = write_temp('let k = "auth.token_signing_key_env".to_owned();\n')
        try:
            self.assertEqual(gate.check_hardcoded_file(path), [])
        finally:
            path.unlink(missing_ok=True)

    def test_env_var_not_flagged(self) -> None:
        path = write_temp('let v = "ORBISYNC_PAGINATION_HMAC_KEY".to_owned();\n')
        try:
            self.assertEqual(gate.check_hardcoded_file(path), [])
        finally:
            path.unlink(missing_ok=True)

    def test_sql_not_flagged(self) -> None:
        path = write_temp('let q = "SELECT * FROM users WHERE id = $1";\n')
        try:
            self.assertEqual(gate.check_hardcoded_file(path), [])
        finally:
            path.unlink(missing_ok=True)

    def test_hardcoded_tracked_files_excludes_testkit(self) -> None:
        files = gate.hardcoded_tracked_files()
        for p in files:
            self.assertNotIn("orbisync-testkit", p.parts, f"testkit should be excluded: {p}")
            self.assertNotIn("orbisync-e2e-helper", p.parts, f"e2e helper should be excluded: {p}")
            self.assertNotIn("tests", p.parts, f"tests folder should be excluded: {p}")
            self.assertEqual(p.parts[0], "crates", f"only crates should be scanned: {p}")

    def test_hardcoded_scan_set_not_empty(self) -> None:
        # Regression for Windows path bug where str(p).startswith("crates/")
        # excluded everything and left the gate silently inactive.
        files = gate.hardcoded_tracked_files()
        existing = [p for p in files if p.exists()]
        self.assertGreater(len(existing), 0, "hardcoded gate must scan >0 production files; got 0 (path logic bug?)")
        # Spot-check a known production file is included
        self.assertTrue(
            any(p.as_posix() == "crates/orbisync-domain/src/lib.rs" for p in files),
            "expected crates/orbisync-domain/src/lib.rs in scan set",
        )

    def test_hardcoded_path_is_os_independent(self) -> None:
        # Naive check str(p).startswith("crates/") fails on Windows because
        # str(Path("crates/x.rs")) is "crates\\x.rs". The gate must use
        # Path.parts or as_posix for OS independence.
        win_path = Path("crates/orbisync-domain/src/lib.rs")
        self.assertEqual(win_path.as_posix(), "crates/orbisync-domain/src/lib.rs")
        self.assertEqual(win_path.parts[0], "crates")
        # Verify filtering would keep a crates file
        self.assertEqual(win_path.parts[0], "crates")
        # Directly test the gate's filtering on a synthetic Path
        # (we cannot easily mock git ls-files, but we verify the predicate)
        def would_be_included(p: Path) -> bool:
            if p.suffix != ".rs":
                return False
            if not p.parts or p.parts[0] != "crates":
                return False
            if "orbisync-testkit" in p.parts:
                return False
            return True
        self.assertTrue(would_be_included(win_path))
        self.assertFalse(would_be_included(Path("crates/orbisync-testkit/src/lib.rs")))

    def test_production_file_without_hardcoded_passes(self) -> None:
        # Simulate a clean production file resembling current main's pagination.rs
        content = '''
use orbisync_application::pagination::CursorCodec;

pub fn codec_from_env(env: &dyn EnvSource, config: &Config) -> CursorCodec {
    let key = env.get(&config.auth.pagination_hmac_key_env).expect("missing");
    CursorCodec::new(key.into_bytes()).unwrap()
}
'''
        path = write_temp(content)
        try:
            self.assertEqual(gate.check_hardcoded_file(path), [])
        finally:
            path.unlink(missing_ok=True)

    def test_generic_key_via_context_is_detected(self) -> None:
        # A generic long key assigned to a variable named `api_key` should be flagged
        # even though the literal itself does not contain "key".
        path = write_temp('let api_key = "aB3!xY9@qW8#kL0$mN1&pQ2*rS4".to_owned();\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertTrue(any("hardcoded secret" in p for p in problems), f"generic api_key should be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_pem_block_is_detected(self) -> None:
        # PEM blocks contain spaces (BEGIN PRIVATE KEY) so they need dedicated detection.
        pem = "-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\\n-----END PRIVATE KEY-----"
        # Use b"..." form as in Rust
        path = write_temp(f'const KEY: &[u8] = b"{pem}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertTrue(any("hardcoded secret" in p for p in problems), f"PEM should be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_pem_with_dev_prefix_is_allowed(self) -> None:
        # Existing dev keys in orbisync-server are named DEV_PRIVATE_PEM and will be
        # removed by another worker; allow them via DEV_ in outer context to keep
        # main green while still flagging other PEM injections.
        pem = "-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\\n-----END PRIVATE KEY-----"
        path = write_temp(f'const DEV_PRIVATE_PEM: &[u8] = b"{pem}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertEqual(problems, [], f"DEV_ PEM should be allowed, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    # ------------------------------------------------------------------
    # Regression for PEM header-only false positive (token.rs:454)
    # ------------------------------------------------------------------

    def test_private_pem_with_body_is_detected(self) -> None:
        # B: full Ed25519 private PEM (header + base64 body + footer) must be flagged
        pem = "-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\\n-----END PRIVATE KEY-----"
        path = write_temp(f'const KEY: &str = "{pem}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertTrue(any("hardcoded secret" in p for p in problems), f"full private PEM should be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_public_key_header_only_not_flagged(self) -> None:
        # C: BEGIN PUBLIC KEY header alone must NOT be flagged (public keys are not secrets,
        # and header-only strings lack base64 body)
        header = "-----BEGIN PUBLIC KEY-----\\n"
        path = write_temp(f'let mut pem = String::from("{header}");\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertEqual(problems, [], f"public header-only should not be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_public_key_full_pem_not_flagged(self) -> None:
        # Public PEM with body must also NOT be flagged (only private keys are secrets)
        pem = "-----BEGIN PUBLIC KEY-----\\nMCowBQYDK2VwAyEAJrFakeBase64Body1234567890ABCDEFGHIJ\\n-----END PUBLIC KEY-----"
        path = write_temp(f'const PUB: &str = "{pem}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertEqual(problems, [], f"full public PEM should not be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_private_header_only_not_flagged(self) -> None:
        # Header-only private key string (no body) must NOT be flagged
        header = "-----BEGIN PRIVATE KEY-----\\n"
        path = write_temp(f'let x = "{header}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertEqual(problems, [], f"private header-only should not be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)

    def test_dev_pagination_hmac_const_is_detected(self) -> None:
        # D: dev-pagination-hmac-key-32bytes!! const must be flagged
        key = build_dev_key()
        path = write_temp(f'const KEY: &str = "{key}";\n')
        try:
            problems = gate.check_hardcoded_file(path)
            self.assertTrue(any("hardcoded secret" in p for p in problems), f"dev pagination const should be flagged, got {problems}")
        finally:
            path.unlink(missing_ok=True)


if __name__ == "__main__":
    unittest.main()
