# Building the consumer bundles

These are producer instructions. Consumers need only the extracted bundle and an ordinary local user account.

## Pinned PostgreSQL input

Both x64 targets build PostgreSQL **17.11** from the [official source archive](https://ftp.postgresql.org/pub/source/v17.11/). The committed SHA-256 is:

```text
dd27f2b3c59e73ed14aa3324901242bf69a032a6347805f274e6260322d42979  postgresql-17.11.tar.bz2
```

The [upstream checksum](https://ftp.postgresql.org/pub/source/v17.11/postgresql-17.11.tar.bz2.sha256) must be checked when deliberately updating this pin. There is no runtime download, mutable `latest` URL, or consumer package-manager invocation.

The recipe uses the [official Meson installation procedure](https://www.postgresql.org/docs/17/install-meson.html): release build, `--auto-features=disabled`, `-Dssl=none`, `-Duuid=none`, `-Drpath=false`, and relative `bin`, `lib`, `share` directories. No external OpenSSL, ICU, Readline, compression, LDAP, LLVM, Perl/Python procedural-language, or system timezone dependency is enabled. PL/pgSQL and the standard PostgreSQL modules/data remain included. This embedded runtime is for the launcher's private loopback DB; external PostgreSQL deployments retain their existing TLS-capable configuration.

Windows uses MSVC `/MT` (`-Db_vscrt=mt`). The producer needs MSVC x64 tools, Perl, Python, Meson **1.9.1**, and Ninja **1.13.0**. `build_postgres_runtime.py` retrieves the upstream [winflexbison 2.5.25](https://github.com/lexxmark/winflexbison/releases/tag/v2.5.25) producer tools with a committed SHA-256, extracts them inside its temporary build directory, and uses Git for Windows' bundled Perl if no other Perl is on PATH. None of these tools is installed globally or shipped as a consumer dependency.

Linux builds on Ubuntu 22.04 with its C toolchain, Flex, Bison, and Perl. The distribution uses the glibc 2.35 baseline and OS libraries; its exact `ldd` inventory is recorded. Do not build or run the smoke as root.

## Build and verify

In an x64 native build environment (MSVC developer environment on Windows):

```sh
python -m pip install meson==1.9.1 ninja==1.13.0
python scripts/build_postgres_runtime.py --target x86_64-unknown-linux-gnu --output target/postgres-runtime
cargo build --locked --release -p orbisync-server --target x86_64-unknown-linux-gnu
python scripts/package_server.py --binary target/x86_64-unknown-linux-gnu/release/orbisync-server --postgres-runtime target/postgres-runtime --target x86_64-unknown-linux-gnu --version v0.0.0-example --source-commit FULL_40_CHARACTER_GIT_SHA --output target/distribution
```

Use `x86_64-pc-windows-msvc` and `orbisync-server.exe` on Windows. Build that Rust target with `RUSTFLAGS=-C target-feature=+crt-static` as the workflow does. Output directories must not already exist; retain a failed output for diagnosis or choose a fresh output directory.

The PG builder copies its installation to a separate temporary location and runs the included `initdb`, `pg_ctl`, `psql`, PL/pgSQL, and shutdown smoke using that copy. It audits every retained executable/shared module, refusing non-OS dependencies outside the runtime. PostgreSQL source and installed files are not patched. `SOURCE_DATE_EPOCH`, source hash, build options, compiler information, dependency inventory, and the successful smoke are recorded in `runtime/postgres/BUILD.json`. This is a pinned, repeatable source-build recipe; it does not promise identical compiler output across different host/toolchain versions.

The packager requires `--postgres-runtime` and verifies the provenance, source archive, required programs/standard data/modules, and exact file checksums before creating a bundle. It checks the copied runtime again. Linux library symlinks are materialized as regular files so ZIP and tar relocation share one contract. The controller sets the bundled library search path on Linux. Archive metadata is normalized for repeatable archives from identical inputs.

## CI and publication

`distribution-build.yml` is reused by the required CI `distribution` job and `release-server.yml`. Each OS builds its runtime and server, packages them, extracts the actual ZIP/tar, and runs `smoke_distribution.py` with a sanitized PATH. This smoke uses the real local-trial API and verifies HTTP, WebSocket, join, sync, stop, and persistent restart. `verify_local_trial_recovery.py` then uses separate private installations to verify recovery, explicit engine/database port repair, retained users/worlds/roles, and rejection of an unrelated listener. Separate browser fixtures exercise UI states; they do not replace the real distribution smoke.

`pr-ci` requires the distribution matrix in addition to every existing gate. Successful jobs retain only public archives and detached hashes as artifacts. Private trial state, logs, launch capabilities, and credentials are never uploaded.

The CI runtime, PostgreSQL build workspace, package output, and extracted consumer bundle use four separate paths under the runner's temporary directory. They stay outside Cargo's cached `target/` tree. Runtime/package outputs must still be new directories; restoring a Rust cache never authorizes overwriting an existing runtime or bundle. The `distribution-v2` cache key excludes older caches that contained these producer outputs.

Dispatch `release-server.yml` with an existing `v`-prefixed tag at the selected commit. `publish=false` builds artifacts without publishing a release. `publish=true` also requires a successful full CI run at that same commit and matching tag, assembles `SHA256SUMS.txt`, verifies it, and publishes the assets. Tags containing a hyphen, such as `v0.1.0-preview.2`, publish as GitHub prereleases; ordinary version tags publish as standard releases. Do not reuse a tag for different contents.

## Licenses and runtime updates

The bundle includes PostgreSQL's root license and the complete checksum-verified official source archive, containing all incorporated-code license/copyright notices. `THIRD-PARTY.json` inventories PostgreSQL alongside the Rust dependencies. Runtime and package `SHA256SUMS` files cover every corresponding regular file except their own manifest.

Updating the PG pin requires reviewing upstream release/security notes, updating version/hash in `postgres_runtime.py`, rebuilding both platforms, and passing the complete CI matrix. Keep build-time generator pins deliberate too. A major PostgreSQL upgrade also requires a database migration policy; replacing runtime files alone does not upgrade existing data directories.
