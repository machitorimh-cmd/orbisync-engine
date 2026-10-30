"""Pinned PostgreSQL build contract and fail-closed distribution validation."""

import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile

VERSION = "17.11"
SOURCE_URL = f"https://ftp.postgresql.org/pub/source/v{VERSION}/postgresql-{VERSION}.tar.bz2"
SOURCE_SHA256 = "dd27f2b3c59e73ed14aa3324901242bf69a032a6347805f274e6260322d42979"
TARGETS = ("x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu")
PROGRAMS = ("postgres", "initdb", "pg_ctl", "psql", "pg_isready", "createdb", "pg_dump", "pg_dumpall", "pg_restore")
OPTIONS = ["--buildtype=release", "--auto-features=disabled", "--bindir=bin", "--libdir=lib",
           "--datadir=share", "-Dssl=none", "-Duuid=none", "-Drpath=false"]


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def files(root):
    """Reject links/reparse points instead of silently following another tree."""
    root = Path(root)
    if (not root.is_dir() or root.is_symlink()
            or getattr(root.lstat(), "st_file_attributes", 0) & 0x400):
        raise ValueError("runtime must be a real directory")
    result = []
    for path in sorted(root.rglob("*")):
        info = path.lstat()
        if path.is_symlink() or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError(f"links/reparse points are forbidden: {path}")
        path.resolve().relative_to(root.resolve())
        if stat.S_ISREG(info.st_mode):
            result.append(path)
        elif not stat.S_ISDIR(info.st_mode):
            raise ValueError(f"special file in runtime: {path}")
    return result


def write_checksums(root):
    content = "".join(f"{sha256(path)}  {path.relative_to(root).as_posix()}\n"
                      for path in files(root) if path != Path(root) / "SHA256SUMS")
    (Path(root) / "SHA256SUMS").write_text(content, encoding="utf-8", newline="\n")


def validate_x64_binary(path, target):
    with Path(path).open("rb") as stream:
        header = stream.read(64)
        if "windows" in target:
            if len(header) < 64 or header[:2] != b"MZ":
                raise ValueError(f"invalid PE binary: {path}")
            stream.seek(int.from_bytes(header[60:64], "little"))
            pe = stream.read(6)
            if pe != b"PE\0\0\x64\x86":
                raise ValueError(f"PE binary is not x64: {path}")
        elif header[:5] != b"\x7fELF\x02" or header[18:20] != b"\x3e\x00":
            raise ValueError(f"ELF binary is not x64: {path}")


def validate_runtime(root, target):
    root = Path(root)
    actual = {p.relative_to(root).as_posix(): p for p in files(root)}
    if target not in TARGETS:
        raise ValueError("unsupported runtime target")
    for name in ("BUILD.json", "SHA256SUMS", "LICENSE", "NOTICES.json", f"source/postgresql-{VERSION}.tar.bz2"):
        if name not in actual:
            raise ValueError(f"runtime is missing {name}")
    expected = {}
    for line in actual["SHA256SUMS"].read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
        if not match:
            raise ValueError("invalid runtime checksum manifest")
        digest, name = match.groups()
        path = PurePosixPath(name)
        if path.is_absolute() or ".." in path.parts or "\\" in name or name in expected or name == "SHA256SUMS":
            raise ValueError("unsafe/duplicate runtime checksum path")
        expected[name] = digest
    if set(expected) != set(actual) - {"SHA256SUMS"}:
        raise ValueError("runtime checksum manifest must cover exactly every file")
    for name, digest in expected.items():
        if sha256(actual[name]) != digest:
            raise ValueError(f"runtime checksum mismatch: {name}")
    build = json.loads(actual["BUILD.json"].read_text(encoding="utf-8"))
    options = OPTIONS + (["-Db_vscrt=mt"] if "windows" in target else [])
    if (build.get("schemaVersion") != 1 or build.get("component") != "postgresql"
            or build.get("version") != VERSION or build.get("target") != target
            or build.get("source") != {"url": SOURCE_URL, "sha256": SOURCE_SHA256}
            or build.get("options") != options or build.get("smokeVerified") is not True
            or not build.get("tools") or not build.get("dependencyAudit")):
        raise ValueError("runtime provenance does not match the pinned build contract")
    if sha256(actual[f"source/postgresql-{VERSION}.tar.bz2"]) != SOURCE_SHA256:
        raise ValueError("runtime source archive differs from pinned upstream source")
    if "PostgreSQL" not in actual["LICENSE"].read_text(encoding="utf-8"):
        raise ValueError("PostgreSQL license is missing")
    try:
        with tarfile.open(actual[f"source/postgresql-{VERSION}.tar.bz2"]) as source:
            notice = source.extractfile(f"postgresql-{VERSION}/COPYRIGHT")
            if notice is None or notice.read() != actual["LICENSE"].read_bytes():
                raise ValueError("PostgreSQL license differs from the verified source")
    except (KeyError, tarfile.TarError) as error:
        raise ValueError("verified source license is unavailable") from error
    windows = "windows" in target
    suffix = ".exe" if windows else ""
    for program in PROGRAMS:
        path = actual.get(f"bin/{program}{suffix}")
        if path is None or not path.stat().st_size:
            raise ValueError(f"required PostgreSQL program missing: {program}")
        validate_x64_binary(path, target)
        if not windows and path.stat().st_mode & 0o111 == 0:
            raise ValueError(f"PostgreSQL program is not executable: {program}")
    for name in ("postgres.bki", "postgresql.conf.sample", "pg_hba.conf.sample", "pg_ident.conf.sample",
                 "sql_features.txt", "system_views.sql", "information_schema.sql"):
        if f"share/{name}" not in actual:
            raise ValueError(f"required PostgreSQL initialization data missing: {name}")
    if not any(name.startswith("share/timezone/") for name in actual):
        raise ValueError("bundled PostgreSQL timezone data missing")
    if not any(name.startswith("share/extension/plpgsql--") for name in actual):
        raise ValueError("standard plpgsql extension SQL missing")
    for module in ("plpgsql", "dict_snowball"):
        if f"lib/{module}{'.dll' if windows else '.so'}" not in actual:
            raise ValueError(f"PostgreSQL standard runtime module missing: {module}")
    if not any(Path(name).name.lower().startswith("libpq") for name in actual):
        raise ValueError("libpq runtime missing")
    return build
