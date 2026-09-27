#!/usr/bin/env python3
"""Reject handwritten protobuf wire definitions in public client tooling.

``proto/orbisync/v1/realtime.proto`` is the sole wire-schema source.  The
standalone Rust load generator must include prost output from ``build.rs`` and
the TypeScript SDK must import Buf generated types.  This gate deliberately
checks the wiring as well as common generated-code markers, so a future
handwritten replacement cannot silently drift from the protocol.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PROTO = ROOT / "proto" / "orbisync" / "v1" / "realtime.proto"
LOAD_GENERATOR = ROOT / "apps" / "load-generator"
SDK = ROOT / "sdk" / "typescript"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def check(root: Path = ROOT) -> list[str]:
    proto = root / "proto" / "orbisync" / "v1" / "realtime.proto"
    load = root / "apps" / "load-generator"
    sdk = root / "sdk" / "typescript"
    errors: list[str] = []

    if not proto.is_file():
        return [f"missing protocol source of truth: {proto}"]

    build_rs = load / "build.rs"
    main_rs = load / "src" / "main.rs"
    if not build_rs.is_file() or not main_rs.is_file():
        errors.append("load-generator must contain build.rs and src/main.rs")
    else:
        build = read(build_rs)
        main = read(main_rs)
        for marker in ("protox::compile", "prost_build::Config", "compile_fds"):
            if marker not in build:
                errors.append(f"apps/load-generator/build.rs must use {marker}")
        if 'include!(concat!(env!("OUT_DIR"), "/orbisync.v1.rs"))' not in main:
            errors.append("apps/load-generator/src/main.rs must include the build-time prost output")

        rust_sources = sorted((load / "src").rglob("*.rs"))
        for path in rust_sources:
            text = read(path)
            if re.search(r"#\s*\[prost\s*\(", text) or re.search(
                r"derive\s*\([^\n]*\bMessage\b", text
            ):
                errors.append(f"{path.relative_to(root)} contains handwritten prost definitions")

    client = sdk / "src" / "client.ts"
    if not client.is_file():
        errors.append("sdk/typescript/src/client.ts is missing")
    else:
        text = read(client)
        if "./generated/orbisync/v1/realtime_pb.js" not in text:
            errors.append("sdk/typescript/src/client.ts must import protobuf types from generated output")

    # Generated TypeScript is intentionally ignored by git and is excluded
    # from this scan.  These markers are specific to protoc-gen-es output and
    # should never be recreated in a hand-maintained SDK source file.
    generated_dir = sdk / "src" / "generated"
    for path in sorted((sdk / "src").rglob("*.ts")):
        if generated_dir in path.parents:
            continue
        text = read(path)
        for marker in ("proto3.", "MessageType", "messageDesc"):
            if marker in text:
                errors.append(f"{path.relative_to(root)} contains handwritten/generated protobuf marker {marker!r}")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    errors = check(args.root.resolve())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("No handwritten protobuf definitions found; client tooling uses generated bindings.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
