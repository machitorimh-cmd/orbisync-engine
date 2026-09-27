#!/usr/bin/env python3
"""Developer-side assembly; output is a source-independent consumer kit."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="New output directory (must not exist)")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    kit = output / "orbisync-consumer-kit"
    starter = kit / "starter"
    (starter / "src").mkdir(parents=True)
    npm = shutil.which("npm.cmd") or shutil.which("npm")
    if not npm:
        raise SystemExit("npm is required on the producer")
    subprocess.run([npm, "pack", "--pack-destination", str(starter)],
                   cwd=ROOT / "sdk/typescript", check=True)
    source = ROOT / "examples/minimal-client-typescript"
    for name in ["package.json", "package-lock.json", "tsconfig.json",
                 "connection.env.example", ".gitignore"]:
        shutil.copyfile(source / name, starter / name)
    package = json.loads((starter / "package.json").read_text(encoding="utf-8"))
    package["scripts"].pop("test", None)  # producer fixture is not a consumer dependency
    (starter / "package.json").write_text(json.dumps(package, indent=2) + "\n", encoding="utf-8")
    lock = json.loads((starter / "package-lock.json").read_text(encoding="utf-8"))
    # A newly built tarball can have the same version/path as a previous build.
    # Force npm to read its new integrity rather than retain the old lock entry.
    lock["packages"].pop("node_modules/@orbisync/client", None)
    (starter / "package-lock.json").write_text(json.dumps(lock, indent=2) + "\n", encoding="utf-8")
    for name in ["main.ts", "connect.ts"]:
        shutil.copyfile(source / "src" / name, starter / "src" / name)
    # Refresh only the local tarball integrity; no install/build hooks run here.
    subprocess.run([npm, "install", "--package-lock-only", "--ignore-scripts"], cwd=starter, check=True)
    for name in ["README.md", "LLM-INTEGRATION.md", "EXTERNAL-RULES.md", "CLIENT-WIRE.md"]:
        shutil.copyfile(ROOT / "docs/consumer-kit" / name, kit / name)
    protocol = kit / "protocol"
    (protocol / "orbisync/v1").mkdir(parents=True)
    (protocol / "http").mkdir()
    shutil.copyfile(ROOT / "proto/orbisync/v1/realtime.proto", protocol / "orbisync/v1/realtime.proto")
    for name in ["orbisync-v1.yaml", "errors.yaml"]:
        shutil.copyfile(ROOT / "openapi" / name, protocol / "http" / name)
    shutil.copytree(ROOT / "examples/external-input-python", kit / "external-input-python",
                    ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    shutil.copyfile(ROOT / "orbisync.toml.example", kit / "orbisync.toml.example")
    for name in ["LICENSE-MIT", "LICENSE-APACHE"]:
        shutil.copyfile(ROOT / name, kit / name)
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip())
    (kit / "BUILD.json").write_text(json.dumps({"sourceCommit": revision, "dirty": dirty,
        "runtime": "Node 24.x", "package": "@orbisync/client@0.1.0"}, indent=2) + "\n", encoding="utf-8")
    archive = output / "orbisync-consumer-kit.zip"
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zipped:
        for path in sorted(kit.rglob("*")):
            if path.is_file():
                zipped.write(path, path.relative_to(output).as_posix())
    tarball = starter / "orbisync-client-0.1.0.tgz"
    sums = "".join(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.relative_to(output).as_posix()}\n"
                   for p in [tarball, archive])
    (output / "SHA256SUMS.txt").write_text(sums, encoding="utf-8")
    print(sums)


if __name__ == "__main__":
    main()
