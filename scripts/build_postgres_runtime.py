"""Build the checksum-pinned, relocatable PG17 runtime (builder tools only)."""

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

if __package__:
    from . import postgres_runtime as contract
else:
    import postgres_runtime as contract

WINDOWS_TOOLS_URL = "https://github.com/lexxmark/winflexbison/releases/download/v2.5.25/win_flex_bison-2.5.25.zip"
WINDOWS_TOOLS_SHA256 = "8d324b62be33604b2c45ad1dd34ab93d722534448f55a16ca7292de32b6ac135"


def windows_tools(cache, destination):
    archive = cache / "win_flex_bison-2.5.25.zip"
    if not archive.exists():
        with urllib.request.urlopen(WINDOWS_TOOLS_URL, timeout=60) as source, archive.open("wb") as output:
            shutil.copyfileobj(source, output)
    if contract.sha256(archive) != WINDOWS_TOOLS_SHA256:
        raise ValueError("Windows generator tools SHA-256 mismatch")
    with zipfile.ZipFile(archive) as source:
        for entry in source.infolist():
            path = Path(entry.filename)
            if path.is_absolute() or ".." in path.parts or "\\" in entry.filename:
                raise ValueError("unsafe generator archive member")
        source.extractall(destination)
    return [f"-DBISON={(destination / 'win_bison.exe').as_posix()}",
            f"-DFLEX={(destination / 'win_flex.exe').as_posix()}"]


def run(command, **kwargs):
    print("+", " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), check=True, **kwargs)


@contextmanager
def build_directory(work_dir):
    parent = work_dir.resolve()
    temporary = Path(tempfile.mkdtemp(prefix="postgres-build-", dir=parent))
    try:
        yield temporary
    except BaseException:
        print(f"Build failed; workspace preserved at {temporary}", flush=True)
        raise
    else:
        # Only the newly-created child of the requested workspace is removed.
        temporary.resolve().relative_to(parent)
        shutil.rmtree(temporary)


def extract_source(archive, destination):
    # Reject traversal/links/devices even though the archive's hash is pinned.
    with tarfile.open(archive) as source:
        for member in source.getmembers():
            relative = Path(member.name)
            if (relative.is_absolute() or ".." in relative.parts or "\\" in member.name
                    or not (member.isfile() or member.isdir())
                    or not relative.parts or relative.parts[0] != f"postgresql-{contract.VERSION}"):
                raise ValueError("unsafe source archive member")
        source.extractall(destination, filter="data")


def dependency_audit(runtime, target, extra_executables=(), library_dir=None):
    windows = "windows" in target
    paths = contract.files(runtime)
    local = {p.name.lower() for p in paths}
    audit = {}
    linux_os = re.compile(r"^(?:linux-vdso|ld-linux-x86-64|lib(?:c|m|pthread|dl|rt|gcc_s|crypt|resolv))\.so(?:\.[0-9]+)*$")
    windows_os = {"kernel32.dll", "advapi32.dll", "ws2_32.dll", "secur32.dll", "shell32.dll",
                  "shfolder.dll", "bcrypt.dll", "crypt32.dll", "user32.dll", "gdi32.dll",
                  "ole32.dll", "oleaut32.dll", "version.dll", "psapi.dll", "dbghelp.dll",
                  "ucrtbase.dll", "ntdll.dll", "normaliz.dll", "winmm.dll",
                  "bcryptprimitives.dll", "combase.dll"}
    env = os.environ.copy()
    env["VSLANG"] = "1033"
    env["LD_LIBRARY_PATH"] = str(library_dir or runtime / "lib")
    for path in paths:
        if not (path.suffix.lower() in (".exe", ".dll") if windows else
                path.parent == runtime / "bin" or ".so" in path.name or path in extra_executables):
            continue
        command = ["dumpbin", "/DEPENDENTS", str(path)] if windows else ["ldd", str(path)]
        result = subprocess.run(command, capture_output=True, text=True, check=True, env=env)
        if "not found" in result.stdout.lower():
            raise ValueError(f"unresolved runtime dependency: {path.name}")
        deps = re.findall(r"^\s*([A-Za-z0-9_.+-]+\.(?:dll|exe))\s*$", result.stdout, re.M | re.I) if windows else [
            line.strip().split()[0].rsplit("/", 1)[-1] for line in result.stdout.splitlines() if line.strip()]
        for name in deps:
            key = name.lower()
            if windows and key.startswith(("vcruntime", "msvcp", "concrt")):
                raise ValueError(f"static CRT contract violated: {path.name}: {name}")
            allowed = (key in windows_os or key.startswith(("api-ms-win-", "ext-ms-win-"))) if windows else bool(linux_os.fullmatch(name))
            if key not in local and not allowed:
                raise ValueError(f"non-OS dependency must not escape the bundle: {path.name}: {name}")
        audit[path.relative_to(runtime).as_posix()] = sorted(deps)
    if not audit:
        raise ValueError("dependency audit scanned no binaries")
    return audit


def seal_runtime(runtime, target, build):
    """Audit and smoke an assembled build before writing its public manifest."""
    runtime, build = Path(runtime), Path(build)
    archive = runtime / "source" / f"postgresql-{contract.VERSION}.tar.bz2"
    if contract.sha256(archive) != contract.SOURCE_SHA256:
        raise ValueError("cannot seal a runtime with an unverified source archive")
    windows = "windows" in target
    tools = {"python": platform.python_version(),
             "meson": subprocess.check_output([sys.executable, "-m", "mesonbuild.mesonmain", "--version"], text=True).strip(),
             "compiler": json.loads((build / "meson-info/intro-compilers.json").read_text(encoding="utf-8")),
             "host": platform.platform()}
    if windows:
        tools["winflexbison"] = {"version": "2.5.25", "url": WINDOWS_TOOLS_URL,
                                 "sha256": WINDOWS_TOOLS_SHA256}
        tools["msysNativeArguments"] = True
    audit = dependency_audit(runtime.resolve(), target)
    run([sys.executable, Path(__file__).with_name("smoke_postgres_runtime.py"), "--runtime", runtime.resolve()])
    with tarfile.open(archive) as source_tar:
        epoch = str(max(int(m.mtime) for m in source_tar.getmembers()))
    provenance = {"schemaVersion": 1, "component": "postgresql", "version": contract.VERSION,
                  "target": target, "source": {"url": contract.SOURCE_URL, "sha256": contract.SOURCE_SHA256},
                  "options": contract.OPTIONS + (["-Db_vscrt=mt"] if windows else []), "tools": tools,
                  "sourceDateEpoch": epoch, "dependencyAudit": audit, "smokeVerified": True}
    (runtime / "BUILD.json").write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    contract.write_checksums(runtime)
    contract.validate_runtime(runtime, target)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=contract.TARGETS, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, default=Path("target/postgres-build"))
    parser.add_argument("--source-archive", type=Path)
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 2, 4))
    args = parser.parse_args()
    windows = "windows" in args.target
    if (windows != (sys.platform == "win32") or platform.machine().lower() not in ("amd64", "x86_64")
            or args.output.exists() or args.jobs < 1):
        parser.error("native x64 build, positive jobs, and a new output directory are required")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    archive = args.source_archive or args.work_dir / f"postgresql-{contract.VERSION}.tar.bz2"
    if not archive.exists():
        partial = archive.with_suffix(".download")
        with urllib.request.urlopen(contract.SOURCE_URL, timeout=60) as source, partial.open("wb") as output:
            shutil.copyfileobj(source, output)
        if contract.sha256(partial) != contract.SOURCE_SHA256:
            raise ValueError("downloaded source does not match the committed upstream SHA-256")
        partial.rename(archive)
    if contract.sha256(archive) != contract.SOURCE_SHA256:
        raise ValueError("source archive SHA-256 mismatch")
    options = contract.OPTIONS + (["-Db_vscrt=mt"] if windows else [])
    with build_directory(args.work_dir) as temporary:
        extract_source(archive, temporary)
        source = temporary / f"postgresql-{contract.VERSION}"
        build = temporary / "build"
        install = temporary / "postgres-install"
        env = os.environ.copy()
        env["PATH"] = str(Path(sys.executable).parent) + os.pathsep + env.get("PATH", "")
        tool_options = []
        if windows:
            # Git's MSYS Perl otherwise rewrites dumpbin /nologo and /symbols
            # as Unix paths when generating postgres.def. Keep native MSVC
            # arguments intact without patching PostgreSQL's source.
            env["MSYS2_ARG_CONV_EXCL"] = "*"
            env["MSYS_NO_PATHCONV"] = "1"
            tool_options = windows_tools(args.work_dir, temporary / "windows-tools")
            if not shutil.which("perl"):
                perl = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/usr/bin/perl.exe"
                if not perl.is_file():
                    raise RuntimeError("producer requires Perl 5.14+ (Git for Windows Perl is supported)")
                tool_options.append(f"-DPERL={perl.as_posix()}")
        with tarfile.open(archive) as source_tar:
            env["SOURCE_DATE_EPOCH"] = str(max(int(m.mtime) for m in source_tar.getmembers()))
        run([sys.executable, "-m", "mesonbuild.mesonmain", "setup", build, source,
             f"--prefix={install}", *options, *tool_options], env=env)
        run([sys.executable, "-m", "mesonbuild.mesonmain", "compile", "-C", build, "-j", args.jobs], env=env)
        run([sys.executable, "-m", "mesonbuild.mesonmain", "install", "-C", build, "--no-rebuild"], env=env)
        args.output.mkdir(parents=True)
        for directory in ("bin", "lib", "share"):
            for path in sorted((install / directory).rglob("*")):
                if not path.is_file():
                    continue
                path.resolve().relative_to(install.resolve())
                relative = path.relative_to(install)
                if directory == "bin" and path.stem not in contract.PROGRAMS and path.suffix.lower() != ".dll":
                    continue
                if path.suffix.lower() in (".a", ".lib", ".pdb", ".exp", ".pc") or "man" in relative.parts:
                    continue
                destination = args.output / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, destination)  # Materialize internal install symlinks for portable ZIPs.
        shutil.copyfile(source / "COPYRIGHT", args.output / "LICENSE")
        source_dest = args.output / "source" / f"postgresql-{contract.VERSION}.tar.bz2"
        source_dest.parent.mkdir()
        shutil.copyfile(archive, source_dest)
        notices = {"sourceArchive": source_dest.relative_to(args.output).as_posix(),
                   "notice": "Verified complete upstream source contains all PostgreSQL and incorporated-code copyright/license notices."}
        (args.output / "NOTICES.json").write_text(json.dumps(notices, indent=2) + "\n", encoding="utf-8")
        seal_runtime(args.output, args.target, build)
    print(f"Verified PostgreSQL {contract.VERSION}: {args.output}")


if __name__ == "__main__":
    main()
