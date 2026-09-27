"""Package one prebuilt server with launchers, documentation, and crate licenses."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", choices=["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"], required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
        parser.error("source commit must be a full Git SHA")
    if not re.fullmatch(r"v[0-9A-Za-z.-]+", args.version):
        parser.error("invalid release version")
    if not args.binary.is_file():
        parser.error("binary does not exist")

    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", args.target],
        cwd=ROOT,
    ))
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    server = next(p for p in packages.values() if p["name"] == "orbisync-server")
    # Include normal and build dependencies, but do not traverse dev-only edges.
    selected, pending = set(), [server["id"]]
    while pending:
        package_id = pending.pop()
        if package_id in selected:
            continue
        selected.add(package_id)
        pending.extend(dep["pkg"] for dep in nodes[package_id]["deps"]
                       if any(k["kind"] != "dev" for k in dep["dep_kinds"]))

    platform = "windows-x86_64" if "windows" in args.target else "linux-x86_64"
    name = f"orbisync-{args.version}-{platform}"
    args.output.mkdir(parents=True, exist_ok=True)
    bundle = args.output / name
    bundle.mkdir()  # Never silently overwrite an existing distribution.
    executable = "orbisync-server.exe" if "windows" in args.target else "orbisync-server"
    shutil.copyfile(args.binary, bundle / executable)
    (bundle / executable).chmod(0o755)
    for filename in ["LICENSE-MIT", "LICENSE-APACHE"]:
        shutil.copyfile(ROOT / filename, bundle / filename)
    for filename in ["README.ja.md", "README.en.md"]:
        shutil.copyfile(ROOT / "deploy/distribution" / filename, bundle / filename)
    launcher = "start-web-admin.cmd" if "windows" in args.target else "start-web-admin.sh"
    shutil.copyfile(ROOT / "deploy/distribution" / launcher, bundle / launcher)
    (bundle / launcher).chmod(0o755)

    inventory = []
    license_pattern = re.compile(r"^(licen[cs]e|notice|copying|copyright)(?:[._-]|$)", re.I)
    for package in sorted((packages[x] for x in selected), key=lambda p: (p["name"], p["version"])):
        if package["source"] is None:
            continue  # Workspace crates use the root licenses above.
        source = Path(package["manifest_path"]).parent
        files = {p for p in source.rglob("*") if p.is_file() and not p.is_symlink()
                 and (license_pattern.match(p.name) or "LICENSES" in p.parts)}
        if package.get("license_file"):
            files.add(source / package["license_file"])
        if not files:
            supplement = ROOT / "deploy/distribution/licenses" / f"{package['name']}-{package['version']}"
            if supplement.is_dir():
                source = supplement
                files = {p for p in supplement.iterdir() if p.is_file() and not p.is_symlink()}
        if not files:
            raise RuntimeError(f"No license files found for {package['name']} {package['version']}")
        prefix = Path("third-party-licenses") / f"{package['name']}-{package['version']}"
        included = []
        for path in sorted(files):
            relative = path.resolve().relative_to(source.resolve())
            target = bundle / prefix / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            included.append((prefix / relative).as_posix())
        inventory.append({"name": package["name"], "version": package["version"],
                          "license": package.get("license"), "repository": package.get("repository"),
                          "files": included})
    (bundle / "THIRD-PARTY.json").write_text(json.dumps(inventory, indent=2) + "\n", encoding="utf-8")
    build = {"release": args.version, "packageVersion": server["version"],
             "sourceCommit": args.source_commit, "target": args.target,
             "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
             "binarySha256": sha256(bundle / executable), "cargoLockSha256": sha256(ROOT / "Cargo.lock"),
             "dependencyLicenseCount": len(inventory)}
    (bundle / "BUILD.json").write_text(json.dumps(build, indent=2) + "\n", encoding="utf-8")
    if "windows" in args.target:
        archive = args.output / f"{name}.zip"
        with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as output:
            for path in sorted(bundle.rglob("*")):
                if path.is_file():
                    output.write(path, path.relative_to(args.output).as_posix())
    else:
        archive = args.output / f"{name}.tar.gz"
        with tarfile.open(archive, "x:gz") as output:
            output.add(bundle, arcname=name)
    checksum = f"{sha256(archive)}  {archive.name}\n"
    (args.output / f"{archive.name}.sha256").write_text(checksum, encoding="ascii")
    print(checksum.strip())
    print(f"Included licenses for {len(inventory)} dependency crates")


if __name__ == "__main__":
    main()
