"""Integrity regressions for the portable PostgreSQL distribution boundary."""

import io
import json
from pathlib import Path
import tarfile
import tempfile
import subprocess
import unittest
from unittest.mock import patch

from scripts import build_postgres_runtime as builder
from scripts import package_server
from scripts import postgres_runtime as runtime


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


if __name__ == "__main__":
    unittest.main()
