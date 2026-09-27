# Engine Web setup and administration

The real UI is embedded in the Rust executable. Consumers need PostgreSQL and
`orbisync-server`, with no Node or frontend build toolchain.

## Display language

The header's **日本語 / English** selector is available before setup or sign-in
and throughout administration. On first use, a Japanese browser language selects
Japanese; other browser languages select English. The choice is saved in local
storage for this origin (host and port). If storage is unavailable, the choice
lasts for the current page.

Switching changes UI text in place without requests or reloading forms. Entered
passwords, drafts, role selections, pagination, active sections and expanded
details stay intact. UI labels, help, known lifecycle states, local validation
and the launcher's four fixed success messages have Japanese and English copy.
Names, IDs, permission keys, JSON, server errors and raw diagnostic details are
preserved. Arbitrary server messages are not translated. Browser-native validation
popup chrome follows the browser; its field validation text follows the selector.
The local password recovery command from
[the recovery guide](../../docs/operations/admin-password-recovery.md) is unchanged.

Focused UI verification with an already installed Playwright (no DB or server):

```sh
node --check apps/admin-web/src/main.js
node scripts/verify_admin_language.cjs
```

If Playwright is installed elsewhere, set `PLAYWRIGHT_MODULE` to that existing
`@playwright/test` module path. The check uses fixed API responses and actual
browser controls; it does not verify backend authorization or recovery.

## First start

From the repository root on Windows or Linux:

```sh
cargo build -p orbisync-server --bin orbisync-server
cargo run -p orbisync-server -- web-admin
```

With a distributed executable: `orbisync-server web-admin` (Windows:
`orbisync-server.exe web-admin`). The launcher opens a browser best effort and
prints a private URL on an interactive terminal. With redirected logs, open the
private link saved in `<data-dir>/launch-url.txt`; secrets are not printed to logs.
Headless mode:

```sh
orbisync-server web-admin --no-browser --port 8090
```

The administration listener always binds to `127.0.0.1`. Open the URL on the same
machine; an SSH tunnel can provide access on headless Linux. Preserve the exact
host and port in the URL. The default engine port is 8080 and admin port is 8090.
Choose free ports; the launcher never stops another process.

1. Prepare a **new empty PostgreSQL database**. Enter its connection URL, for
   example `postgresql://user:YOUR_PASSWORD@127.0.0.1:5432/orbisync`, and click
   **Check connection**.
2. Keep **Production** selected and enter the server-side path to your approved
   corpus of exactly 10,000 distinct passwords. OrbiSync does not bundle this
   third-party data. For a disposable local installation only, explicitly select
   **Local development**. This reproduces the existing
   `scripts/gen-dev-password-denylist.sh` generator without requiring a shell.
   Its placeholders block no real common passwords; replace them before real
   users log in. Production setup rejects that placeholder corpus.
3. Enter the administrator login/display name and click **Initialize this
   database**. This explicitly runs the existing migrations and guarded bootstrap,
   and generates Ed25519/HMAC keys automatically. Save the one-time password.
4. Click **Start engine**, wait for **Engine ready**, then sign in. Open **Change
   password**, submit the temporary and new password, and sign in again. Existing
   password policy applies (default minimum length 12).
5. Manage users and role assignments, create worlds and instances, and start/stop
   instances. New users use the same login/change-password flow. All operations
   use the existing server-side permissions.

## Restart and saved settings

Run the same command from the same directory and click **Start engine**. To choose
another installation directory, use `web-admin --data-dir /absolute/private/path`
and keep using that path. The launcher creates a private directory (Unix 0700,
files 0600; Windows restricted ACL) and rejects shared pre-created directories.

The default `.orbisync-admin/` is git-ignored. Back it up securely with your DB:

- `orbisync.toml`: the existing config format, loaded with the existing validator.
  The UI edits engine bind, DB pool/timeouts, realtime connection limit and default
  world capacity. Other TOML keys survive edits.
- `secrets.json`: private values supplied through the existing EnvSource interface.
  Keys and DB credentials are never returned by settings endpoints. Managed config
  is isolated from ambient config overrides so displayed values match startup.
- `password-denylist.txt`: the selected corpus. Replace development placeholders
  with an operator-approved real corpus before deployment, then restart.
- `input-rules.json`: the existing version 1 external-input manifest. Each binding
  has `world_id`, `rule`, `component_key`, `endpoint` (HTTPS), and
  `signing_secret_ref`. Set that referenced environment secret before launching.
  The UI reuses the existing manifest validator, including secret resolution.
  Use actual world IDs and services; no game-specific rules are bundled.

Configuration access requires an active session with `admin.roles.assign`, an
existing permission included in the bootstrap Administrator role. The launcher
checks it through `/v1/auth/administration-access` on every settings request.
Changes are saved for restart, with a visible notice. Close the launcher with
Ctrl+C (the engine uses its existing drain), rerun the command, and click Start.
Blank DB URL keeps the current credential; a replacement URL must point at an
already-initialized engine DB. It does not copy data or migrate keys.

The private URL carries a per-launch local capability. Keep it private. The tab
holds it in session storage for reloads; login access tokens stay in memory.
Reopen the new URL after restarting. Exact Host and Origin checks also apply.
Setup closes when config is created; it cannot administer an initialized engine.
Temporary passwords and generated secrets are not sent to routine telemetry.

## Existing installations and ordinary CLI

Setup refuses nonempty databases and never overwrites an installation. If an
existing config is detected, explicitly use:

```sh
orbisync-server --config /absolute/path/orbisync.toml web-admin --no-browser
```

Supply its original secret environment variables, including
`ORBISYNC_PASSWORD_DENYLIST_FILE`. This mode does not create keys, run migrations,
bootstrap, or write the existing config. Config display is read-only; continue its
original operator workflow for edits. It starts an engine only on an available
loopback bind. It does not attach to or replace an already-running service.

Ordinary `serve`, `migrate`, `bootstrap-admin`, checkpoint operator, and default
production startup remain usable. Public engine deployment uses ordinary `serve`;
the local Web launcher requires a loopback engine bind. To use ordinary CLI with
a Web-managed installation, load its `secrets.json` pairs into the child process
environment and pass `--config <dir>/orbisync.toml` and
`--input-rules <dir>/input-rules.json`. Never put secrets in command arguments.

## Recovery and verification

Validation/connection failures before initialization leave files/DB unchanged.
Once initialization begins, an `initializing` marker closes repeated attempts.
On migration/bootstrap failure, preserve the directory and DB, correct privileges
or connectivity, load the saved secrets into the operator process environment,
and use existing `migrate` then `bootstrap-admin` recovery commands. Bootstrap
still refuses if any user exists. Do not delete the marker to repeat Web setup.

If the browser disconnects after bootstrap, the private file
`initial-admin-password.txt` holds the initial password for local recovery. No Web
endpoint can reread it. Remove this obsolete file after changing the password.
Engine start failures appear as stopped in the UI with the standard diagnostic in
the terminal. Correct the cause and restart the launcher before retrying start.

```sh
node --check apps/admin-web/src/main.js
# Set ORBISYNC_ADMIN_TEST_DATABASE_URL to a NEW EMPTY disposable database.
python scripts/verify_web_admin.py --binary target/debug/orbisync-server
# On Windows the binary ends in .exe.
```

The focused smoke test initializes that fixture DB, verifies auth/admin/settings,
restarts its own processes, checks preservation/guards, and cleans up its processes
and temporary files. The caller owns DB cleanup. Browser acceptance is separate.
