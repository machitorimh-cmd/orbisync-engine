# OrbiSync binary distribution

[日本語](README.ja.md) | **English**

Download, extract, and launch this package to try the server and its embedded browser administration UI. PostgreSQL 17.11 is bundled. Local trials require no separate Rust, Node.js, Docker, or PostgreSQL installation and no database URL. This is a development preview.

For your first launch, follow the [quick start](https://github.com/machitorimh-cmd/orbisync-engine/blob/main/docs/guides/quickstart.en.md). This guide covers supported environments, storage, and manual configuration.

## Windows x64

Extract the ZIP into a dedicated directory writable by your ordinary user, then open `start-web-admin.cmd`. Administrator privileges are unnecessary. To launch from a terminal:

```powershell
.\orbisync-server.exe web-admin
```

If the browser does not open, use the private URL printed in the terminal. This is a Windows x64 package; CI distribution tests target Windows Server 2022. Its C/C++ runtime is statically linked, so installing a Visual C++ Redistributable separately is unnecessary. This is not a signed installer. Verify the download source and SHA-256.

## Linux x86_64

This x86_64 build uses Ubuntu 22.04 as its baseline and requires glibc 2.35 or later. It is not an Alpine/musl or ARM64 build. Ordinary OS libraries such as `libgcc_s.so.1` are used; external HTTPS connections use the system CA certificates. Run as an ordinary user: PostgreSQL refuses to run as root. Consult the release notes for checked environments.

Extract the archive into a dedicated writable directory owned by your ordinary user:

```sh
tar -xzf orbisync-v0.1.0-preview.3-linux-x86_64.tar.gz
cd orbisync-v0.1.0-preview.3-linux-x86_64
./start-web-admin.sh
```

For a headless machine:

```sh
./start-web-admin.sh --no-browser
```

Administration is restricted to the server's loopback interface. For access from another machine, use a tunnel such as `ssh -L 8090:127.0.0.1:8090 USER@HOST`, then open the private URL in your local browser. Preserve its host, port, and token. When logs are redirected, the private link is saved in `.orbisync-admin/launch-url.txt`. Do not share that file or URL.

## First setup

1. Select **Try on this PC**. The package prepares its private database, applies migrations, creates an administrator, and starts the engine.
2. The administration page signs in automatically and tests HTTP / WebSocket / join / sync. Save the administrator password shown once in a secure place.
3. Use the page's stop, start, and restart controls. Opening the same installation reuses its database and configuration instead of initializing it again.
4. If startup fails, follow the diagnostics. Port changes require an explicit choice in the page; unrelated processes are not stopped.

Local trials use a temporary development password policy. For real users, use manual configuration to supply an operator-approved 10,000-entry denylist and other deployment settings. The existing external-PostgreSQL manual setup remains available. Manual initialization applies migrations and creates an administrator, so never point it at an existing valuable database.

The header switches between Japanese and English. Administration prefers port 8090 and chooses a free port when it is occupied. Local trials also select free database and engine ports and save them. If a saved port becomes occupied on a later start, use the diagnostic port repair action.

The launcher uses the package directory as its working directory. Configuration, secrets, and the local database are stored in `.orbisync-admin/` by default; the database files are under its `local-postgres/data/` directory. Restart using the same launcher in the same directory. Use `--data-dir` to select another location. Stop with Ctrl+C in the terminal. Securely back up both the database and configuration directory; copying live database files alone is not a consistent backup. Never add the private installation directory to a redistribution ZIP or Git.

This local Web launcher does not automatically configure a public network service. See the [Web administration guide](https://github.com/machitorimh-cmd/orbisync-engine/blob/main/apps/admin-web/README.md) for ordinary `serve`, existing installations, recovery, and TLS considerations.

## Guest participation from your app

Create a World and Instance in administration, start the Instance, then enable
**Configuration → Guest participation** and select the allowed World. Save and
restart the engine. A Guest role is prepared on first use when needed.

Set the app's **engine URL and Instance ID** from administration. These differ
from the administration URL and World ID. Leave and Join again to obtain a fresh
guest session after a settings change. Guests cannot join unselected Worlds.

## Contents and verification

- `orbisync-server[.exe]`: server with embedded administration UI and DB migrations
- `start-web-admin.cmd` or `start-web-admin.sh`: launcher
- `BUILD.json`: source commit, target, Rust version, and binary SHA-256
- `runtime/postgres/`: PostgreSQL executables, standard modules, initialization data, and timezone data
- `runtime/postgres/BUILD.json` / `SHA256SUMS`: pinned source URL/hash, build options, dependency audit, and checksums of every runtime file
- `runtime/postgres/LICENSE` / `source/`: PostgreSQL license and verified official source containing incorporated-code copyright/license notices
- `SHA256SUMS`: checksums of the extracted package files
- `LICENSE-MIT` / `LICENSE-APACHE`: OrbiSync licenses
- `THIRD-PARTY.json` / `third-party-licenses/`: dependency crate licenses and notices

Compare the downloaded archive's SHA-256 against the release's `SHA256SUMS.txt`. Use `Get-FileHash <ZIP-name> -Algorithm SHA256` on Windows or `sha256sum <archive-name>` on Linux.

Release notes list the checks performed and their limits. A preview release does not certify production deployment, database migration/recovery, long-running operation, or a connection capacity.
