# OrbiSync binary distribution

[日本語](README.ja.md) | **English**

This package runs the server and its embedded browser administration UI without installing Rust or Node.js. It is a development preview. PostgreSQL is not bundled: prepare an empty PostgreSQL database separately. Setup applies migrations and creates an administrator, so do not point it at an existing valuable database.

## Windows x64

Extract the ZIP into a dedicated directory writable by your ordinary user, then open `start-web-admin.cmd`. Administrator privileges are unnecessary. To launch from a terminal:

```powershell
.\orbisync-server.exe web-admin
```

If the browser does not open, use the private URL printed in the terminal. This is not a signed installer. Verify the download source and SHA-256. If Windows reports a missing Microsoft Visual C++ runtime DLL, install Microsoft's official Visual C++ v14 x64 Redistributable.

## Linux x86_64

This is a glibc Linux build, not an Alpine/musl or ARM64 build. Consult the release notes for the required glibc version and checked environments. System CA certificates and `libgcc_s.so.1` are also required.

Extract the archive into a dedicated writable directory owned by your ordinary user:

```sh
tar -xzf orbisync-v0.1.0-preview.1-linux-x86_64.tar.gz
cd orbisync-v0.1.0-preview.1-linux-x86_64
./start-web-admin.sh
```

For a headless machine:

```sh
./start-web-admin.sh --no-browser
```

Administration is restricted to the server's loopback interface. For access from another machine, use a tunnel such as `ssh -L 8090:127.0.0.1:8090 USER@HOST`, then open the private URL in your local browser. Preserve its host, port, and token. When logs are redirected, the private link is saved in `.orbisync-admin/launch-url.txt`. Do not share that file or URL.

## First setup

1. Enter the connection URL for an empty PostgreSQL database and check connectivity.
2. For real users, provide an operator-approved 10,000-entry password denylist. For disposable local trials only, explicitly select **Local development**. Its generated placeholders do not block real common passwords.
3. Enter the administrator's login ID and display name, then initialize. Setup creates keys and applies migrations. Save the one-time initial password securely.
4. Select **Start engine**, wait for readiness, and sign in. Change the initial password and sign in again.

The header switches between Japanese and English. The default administration port is 8090 and engine port is 8080. If a port is occupied, change configuration instead of stopping unrelated processes.

The launcher uses the package directory as its working directory. Configuration and secrets are stored in `.orbisync-admin/` by default. Restart using the same launcher in the same directory. Use `--data-dir` to select another location. Stop with Ctrl+C in the terminal. Securely back up both the database and configuration directory. Never add the credentials directory to a redistribution ZIP or Git.

This local Web launcher does not automatically configure a public network service. See the [Web administration guide](https://github.com/machitorimh-cmd/orbisync-engine/blob/main/apps/admin-web/README.md) for ordinary `serve`, existing installations, recovery, and TLS considerations.

## Contents and verification

- `orbisync-server[.exe]`: server with embedded administration UI and DB migrations
- `start-web-admin.cmd` or `start-web-admin.sh`: launcher
- `BUILD.json`: source commit, target, Rust version, and binary SHA-256
- `LICENSE-MIT` / `LICENSE-APACHE`: OrbiSync licenses
- `THIRD-PARTY.json` / `third-party-licenses/`: dependency crate licenses and notices

Compare the downloaded archive's SHA-256 against the release's `SHA256SUMS.txt`. Use `Get-FileHash <ZIP-name> -Algorithm SHA256` on Windows or `sha256sum <archive-name>` on Linux.

Release notes list the checks performed and their limits. A preview release does not certify production deployment, database migration/recovery, long-running operation, or a connection capacity.
