"""Integrity regressions for the portable PostgreSQL distribution boundary."""

import io
import json
from pathlib import Path
import tarfile
import tempfile
import subprocess
import unittest
import urllib.error
from unittest.mock import patch

from scripts import build_postgres_runtime as builder
from scripts import package_server
from scripts import postgres_runtime as runtime
from scripts import private_fixture
from scripts import smoke_distribution


class RuntimeIntegrityTests(unittest.TestCase):
    @staticmethod
    def pe():
        data = bytearray(128)
        data[:2] = b"MZ"
        data[60:64] = (64).to_bytes(4, "little")
        data[64:70] = b"PE\0\0\x64\x86"
        return bytes(data)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "postgres"
        self.root.mkdir()
        self.target = runtime.TARGETS[0]
        source = io.BytesIO()
        with tarfile.open(fileobj=source, mode="w:bz2") as output:
            copyright_file = tarfile.TarInfo(f"postgresql-{runtime.VERSION}/COPYRIGHT")
            copyright_file.size = len(b"PostgreSQL License")
            output.addfile(copyright_file, io.BytesIO(b"PostgreSQL License"))
        self.source = source.getvalue()
        (self.root / "source").mkdir()
        (self.root / f"source/postgresql-{runtime.VERSION}.tar.bz2").write_bytes(self.source)
        digest = runtime.sha256(self.root / f"source/postgresql-{runtime.VERSION}.tar.bz2")
        self.patch = patch.object(runtime, "SOURCE_SHA256", digest)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        for program in runtime.PROGRAMS:
            self.write(f"bin/{program}.exe", self.pe())
        self.write("bin/libpq.dll", b"MZ\0fixture")
        self.write("lib/plpgsql.dll", b"MZ\0fixture")
        self.write("lib/dict_snowball.dll", b"MZ\0fixture")
        for name in ("postgres.bki", "postgresql.conf.sample", "pg_hba.conf.sample", "pg_ident.conf.sample",
                     "sql_features.txt", "system_views.sql", "information_schema.sql", "timezone/UTC",
                     "extension/plpgsql--1.0.sql"):
            self.write(f"share/{name}", b"fixture")
        self.write("LICENSE", b"PostgreSQL License")
        self.write("NOTICES.json", b"{}")
        build = {"schemaVersion": 1, "component": "postgresql", "version": runtime.VERSION,
                 "target": self.target, "source": {"url": runtime.SOURCE_URL, "sha256": digest},
                 "options": runtime.OPTIONS + ["-Db_vscrt=mt"], "tools": {"compiler": "fixture"},
                 "dependencyAudit": {"bin/postgres.exe": ["KERNEL32.dll"]}, "smokeVerified": True}
        self.write("BUILD.json", json.dumps(build).encode())
        runtime.write_checksums(self.root)

    def write(self, name, data):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def test_valid_runtime_and_target_mismatch(self):
        self.assertEqual(runtime.validate_runtime(self.root, self.target)["version"], runtime.VERSION)
        with self.assertRaises(ValueError):
            runtime.validate_runtime(self.root, runtime.TARGETS[1])

    def test_new_private_fixture_grants_only_current_sid_for_restricted_postgres(self):
        sid = "S-1-5-21-123-456-789-1001"
        owner = subprocess.CompletedProcess([], 0, stdout=f'"host\\user","{sid}"\r\n'.encode())
        system = Path(self.temporary.name) / "Windows"
        with patch.object(private_fixture, "WINDOWS", True), \
                patch.dict(private_fixture.os.environ, {"SystemRoot": str(system)}), \
                patch.object(private_fixture.subprocess, "run", side_effect=[owner, None]) as run:
            with private_fixture.private_temporary_directory(prefix="orbisync-test-private-") as directory:
                created = Path(directory)
                self.assertTrue(created.is_dir())
                self.assertEqual(run.call_args_list[0].args[0],
                                 [str(system / "System32/whoami.exe"), "/user", "/fo", "csv", "/nh"])
                self.assertEqual(run.call_args_list[1].args[0],
                                 [str(system / "System32/icacls.exe"), directory, "/grant:r", f"*{sid}:(OI)(CI)F"])
                self.assertTrue(run.call_args_list[1].kwargs["check"])
            self.assertFalse(created.exists())
        invalid = subprocess.CompletedProcess([], 0, stdout=b'"host\\user","Everyone"\r\n')
        with patch.object(private_fixture, "WINDOWS", True), \
                patch.dict(private_fixture.os.environ, {"SystemRoot": str(system)}), \
                patch.object(private_fixture.subprocess, "run", return_value=invalid) as run, \
                self.assertRaisesRegex(RuntimeError, "user SID"):
            with private_fixture.private_temporary_directory(prefix="orbisync-test-private-"):
                self.fail("invalid identity must fail before exposing a fixture")
        self.assertEqual(run.call_count, 1)

    def test_dependency_audit_accepts_os_and_bundled_libraries_but_rejects_external_or_dynamic_crt(self):
        def output(names):
            return subprocess.CompletedProcess([], 0, stdout="\n".join(f"    {name}" for name in names), stderr="")
        with patch.object(builder.subprocess, "run", return_value=output(["KERNEL32.dll", "libpq.dll", "postgres.exe"])):
            self.assertTrue(builder.dependency_audit(self.root, self.target))
        for dependency in ("VCRUNTIME140.dll", "libssl-3-x64.dll"):
            with self.subTest(dependency=dependency), \
                    patch.object(builder.subprocess, "run", return_value=output([dependency])), \
                    self.assertRaises(ValueError):
                builder.dependency_audit(self.root, self.target)

    def test_linux_dependency_audit_handles_no_needed_libraries_without_allowing_external_deps(self):
        linux = Path(self.temporary.name) / "linux"
        module = linux / "lib/auth_delay.so"
        module.parent.mkdir(parents=True)
        module.write_bytes(b"ELF module fixture")
        def output(text):
            return subprocess.CompletedProcess([], 0, stdout=text, stderr="")
        with patch.object(builder.subprocess, "run", return_value=output("\tstatically linked\n")) as inspect:
            self.assertEqual(builder.dependency_audit(linux, runtime.TARGETS[1]), {"lib/auth_delay.so": []})
            self.assertEqual(inspect.call_args.kwargs["env"]["LC_ALL"], "C")
        system_deps = "libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x123)\n/lib64/ld-linux-x86-64.so.2 (0x456)\n"
        with patch.object(builder.subprocess, "run", return_value=output(system_deps)):
            self.assertEqual(builder.dependency_audit(linux, runtime.TARGETS[1])["lib/auth_delay.so"],
                             ["ld-linux-x86-64.so.2", "libc.so.6"])
        for text in ("statically linked\nlibssl.so.3 => /usr/lib/libssl.so.3 (0x123)\n",
                     "statically linked\nlibpq.so.5 => not found\n", "statically linked unexpected\n"):
            with self.subTest(text=text), \
                    patch.object(builder.subprocess, "run", return_value=output(text)), \
                    self.assertRaises(ValueError):
                builder.dependency_audit(linux, runtime.TARGETS[1])

    def test_changed_binary_extra_file_and_missing_data_fail(self):
        for name, mutation in [
            ("bin/postgres.exe", b"MZtampered"), ("bin/foreign.dll", b"MZinjected"),
            ("share/postgres.bki", None),
        ]:
            with self.subTest(name=name):
                path = self.root / name
                original = path.read_bytes() if path.exists() else None
                if mutation is None:
                    path.unlink()
                else:
                    self.write(name, mutation)
                with self.assertRaises(ValueError):
                    runtime.validate_runtime(self.root, self.target)
                if original is None:
                    path.unlink()
                else:
                    self.write(name, original)

    def test_regenerated_checksums_do_not_bypass_required_files_or_source_pin(self):
        (self.root / "bin/initdb.exe").unlink()
        runtime.write_checksums(self.root)
        with self.assertRaisesRegex(ValueError, "initdb"):
            runtime.validate_runtime(self.root, self.target)
        self.write("bin/initdb.exe", b"MZfixture")
        self.write(f"source/postgresql-{runtime.VERSION}.tar.bz2", b"wrong source")
        runtime.write_checksums(self.root)
        with self.assertRaisesRegex(ValueError, "pinned"):
            runtime.validate_runtime(self.root, self.target)

    def test_duplicate_manifest_and_non_smoked_provenance_are_rejected(self):
        manifest = self.root / "SHA256SUMS"
        text = manifest.read_text()
        manifest.write_text(text + text.splitlines()[0] + "\n")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            runtime.validate_runtime(self.root, self.target)
        build = json.loads((self.root / "BUILD.json").read_text())
        build["smokeVerified"] = False
        self.write("BUILD.json", json.dumps(build).encode())
        runtime.write_checksums(self.root)
        with self.assertRaisesRegex(ValueError, "provenance"):
            runtime.validate_runtime(self.root, self.target)

    def test_link_escape_and_source_archive_traversal_are_rejected(self):
        try:
            (self.root / "outside").symlink_to(Path(self.temporary.name), target_is_directory=True)
        except OSError:
            pass  # Windows may not grant symlink creation; archive checks still run.
        else:
            with self.assertRaisesRegex(ValueError, "links"):
                runtime.files(self.root)
            (self.root / "outside").unlink()
        archive = Path(self.temporary.name) / "unsafe.tar"
        with tarfile.open(archive, "w") as output:
            member = tarfile.TarInfo("../escape")
            member.size = 1
            output.addfile(member, io.BytesIO(b"x"))
        with self.assertRaisesRegex(ValueError, "unsafe"):
            builder.extract_source(archive, Path(self.temporary.name) / "extract")

    def test_package_includes_verified_runtime_and_component_provenance(self):
        binary = Path(self.temporary.name) / "server.exe"
        binary.write_bytes(self.pe())
        output = Path(self.temporary.name) / "out"
        metadata = {"packages": [{"id": "server", "name": "orbisync-server", "version": "0.1.0", "source": None}],
                    "resolve": {"nodes": [{"id": "server", "deps": []}]}}
        def command(arguments, **_kwargs):
            return json.dumps(metadata).encode() if arguments[0] == "cargo" else "rustc fixture"
        with patch("sys.argv", ["package_server.py", "--binary", str(binary), "--target", self.target,
                                "--version", "v0.0.0-test", "--source-commit", "a" * 40,
                                "--output", str(output), "--postgres-runtime", str(self.root)]), \
                patch.object(package_server.subprocess, "check_output", side_effect=command), \
                patch.dict(package_server.os.environ, {"RUSTFLAGS": "-C target-feature=+crt-static"}), \
                patch.object(package_server, "dependency_audit", return_value={"orbisync-server.exe": ["KERNEL32.dll"]}):
            package_server.main()
        bundle = output / "orbisync-v0.0.0-test-windows-x86_64"
        runtime.validate_runtime(bundle / "runtime/postgres", self.target)
        build = json.loads((bundle / "BUILD.json").read_text())
        self.assertEqual(build["postgresql"]["source"]["sha256"], runtime.SOURCE_SHA256)
        self.assertIn("runtime/postgres/SHA256SUMS", (bundle / "SHA256SUMS").read_text())
        self.assertEqual(len(list(output.glob("*.zip"))), 1)


class ConsumerDiagnosticTests(unittest.TestCase):
    def failure(self, body):
        error = urllib.error.HTTPError("http://localhost/?token=URL_SECRET", 400, "REASON_SECRET", {}, io.BytesIO(body))
        return str(smoke_distribution._http_failure("/admin/local-trial", error))

    def test_only_bounded_public_code_is_exposed_from_secret_containing_response(self):
        body = json.dumps({"code": "private_storage", "message": "MESSAGE_SECRET",
                           "initial_password": "PASSWORD_SECRET", "access_token": "TOKEN_SECRET"}).encode()
        self.assertEqual(self.failure(body), "consumer request /admin/local-trial returned HTTP 400 (code=private_storage)")
        self.assertEqual(self.failure(json.dumps({"code": "A" * 80}).encode()),
                         "consumer request /admin/local-trial returned HTTP 400 (code=" + "A" * 80 + ")")

    def test_invalid_code_and_non_json_response_are_not_exposed(self):
        for code in ("", "A" * 81, "URL/SECRET", "line\nSECRET", "日本語", "invalid-code", None, 400, ["SECRET"], {"SECRET": True}):
            with self.subTest(code=code):
                self.assertEqual(self.failure(json.dumps({"code": code, "message": "BODY_SECRET"}).encode()),
                                 "consumer request /admin/local-trial returned HTTP 400")
        for body in (b"BODY_SECRET", b"[]", b"{\"code\":", b"x" * 20_000, b"\xff"):
            with self.subTest(body_type=type(body).__name__):
                self.assertEqual(self.failure(body), "consumer request /admin/local-trial returned HTTP 400")


if __name__ == "__main__":
    unittest.main()
