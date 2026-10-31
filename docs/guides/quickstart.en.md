# Quick start

[日本語](quickstart.ja.md) | **English**

Try engine startup and real communication on this computer. PostgreSQL is bundled: no database creation, connection URL, or separate Rust, Node.js, or Docker installation is required.

## 1. Download and extract

Open the [v0.1.0-preview.2 release](https://github.com/machitorimh-cmd/orbisync-engine/releases/tag/v0.1.0-preview.2). Choose the archive for your OS, rather than GitHub's automatically generated "Source code" downloads.

| OS | Archive |
|---|---|
| Windows x64 | `orbisync-v0.1.0-preview.2-windows-x86_64.zip` |
| Linux x86_64 with glibc 2.35 or later | `orbisync-v0.1.0-preview.2-linux-x86_64.tar.gz` |

Extract the entire archive into a dedicated writable folder. Keep the executable and its bundled `runtime/` directory together.

## 2. Open the launcher

On Windows, open **`start-web-admin.cmd`** in the extracted folder. Administrator privileges are unnecessary.

On Linux, run as an ordinary user:

```sh
tar -xzf orbisync-v0.1.0-preview.2-linux-x86_64.tar.gz
cd orbisync-v0.1.0-preview.2-linux-x86_64
./start-web-admin.sh
```

The administration page opens in your browser. If it does not, open the private URL shown in the terminal. When output is redirected to a file, the URL is saved in `.orbisync-admin/launch-url.txt` inside the extracted folder.

## 3. Select "Try on this PC"

The launcher prepares its database, initializes the installation, starts the engine, and signs in automatically. Save the administrator password displayed on first setup securely.

Startup verification is complete when **HTTP, WebSocket, Room entry, and Initial synchronization** all pass. The probe creates a diagnostic World and Instance in this local trial. SDK generation and sample application builds are unnecessary.

## Stop and resume

- **Stop engine and local database** stops both processes and keeps settings and data.
- **Start engine** starts your saved environment.
- **Restart engine** restarts the database and engine and applies saved settings.
- To close the launcher, press **Ctrl+C** in its terminal.

Next time, open the launcher in the same folder and select **Continue local trial**. No database recreation is needed. Data and credentials live in `.orbisync-admin/`; keep that directory and do not share it.

## Troubleshooting

| Situation | Action |
|---|---|
| Administration page does not open | Use this launch's private URL. The launcher chooses a free administration port when necessary. |
| Database or engine port is occupied | Choose the port repair action offered by diagnostics. |
| Setup was interrupted | Select **Retry local startup**. Check available disk space and write permissions too. |
| Connection probe fails | Follow diagnostics and select **Retry connection test**. Restart the engine if needed. |

## Next steps

Use the administration page to manage users, Worlds, and Instances. To connect your own application, continue with the [frontend integration guide](frontend-integration.md). Rendering and application screens belong in your client.

Local trials are a development preview restricted to this computer's loopback interface. For external databases, existing installations, headless servers, storage, and backups, see the [distribution guide](../../deploy/distribution/README.en.md) and [Web administration guide](../../apps/admin-web/README.md).
