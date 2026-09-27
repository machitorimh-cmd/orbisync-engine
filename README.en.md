# OrbiSync

[日本語](README.md) | **English**

<p align="center">
  <img src="icon/OrbiSync.png" alt="OrbiSync — Realtime. Together." width="360">
</p>

OrbiSync is a Rust real-time synchronization engine for sharing worlds, entities, transforms, state, and events between authenticated clients. It provides PostgreSQL persistence, a REST API, a Protocol Buffers WebSocket protocol, and a TypeScript SDK. Rendering, assets, physics, and application-specific rules belong in clients or external services.

This README describes the current source and configuration in the OSS repository. The project is under development, not certified for production or a guarantee of performance or connection capacity. Each release's notes record binary builds, startup checks, and untested areas. Historical verification is not presented as a new full test run, live-service validation, or load test of this public snapshot.

## Prebuilt server

[GitHub Releases](https://github.com/machitorimh-cmd/orbisync-engine/releases/tag/v0.1.0-preview.1) provides a Windows x64 ZIP and a Linux x86_64 tar.gz. Rust and Node.js are unnecessary; PostgreSQL is required separately. Each archive includes Japanese/English startup guides, a launcher, project licenses, and dependency licenses. Checksums are in `SHA256SUMS.txt`. [Usage guide](deploy/distribution/README.en.md)

## Implementation status

These features are present in the source. Their inclusion does not mean they have been validated in every deployment environment.

| Area | Current implementation |
|---|---|
| Authentication and administration | Login, tokens, users and roles, password changes and recovery, audit logs, administrative diagnostics |
| Worlds and instances | Creation, lookup, management, join/leave, membership management, start/stop |
| Real-time synchronization | Authenticated WebSockets, snapshots and deltas, entity creation/update/deletion/ownership transfer, revision validation |
| Connections and delivery | Resume and resynchronization, interest management, visibility and ownership filtering, outbound queues and backpressure |
| Persistence and operations | PostgreSQL, migrations, checkpoints and restore, health, metrics, operator CLI |
| Clients | TypeScript SDK, initial synchronization readiness, server-authoritative input API, display prediction and interpolation helpers |
| External integration | Extension API, webhooks, operator-configured external HTTPS input rules |
| Administration UI | Web setup and administration embedded in the Rust binary, with Japanese and English display languages |

The REST contract contains 37 paths and 46 operations in [OpenAPI](openapi/orbisync-v1.yaml). The real-time contract is [realtime.proto](proto/orbisync/v1/realtime.proto). WebSockets are available at `/ws` and `/v1/realtime/ws`. The contract-to-router mapping can be checked with `scripts/validate_openapi_routes.py`.

The implementation includes SDK state application scoped to changed entries, individual entity revision access, cached presence membership, recipient indexes, and bounded concurrency for instance ticks and checkpoint work. The reference 3D demo waits for initial synchronization before sending updates and recognizes revision advances while stationary. These changes do not establish a measured speedup or a maximum supported connection count.

The core does not provide rendering, audio/video delivery, a physics engine, payments, or a world editor. See the [roadmap](docs/design/roadmap-and-traceability.md) and [acceptance mapping](docs/design/acceptance-traceability/) for design and test coverage.

## Get the source

```sh
git clone https://github.com/machitorimh-cmd/orbisync-engine.git
cd orbisync-engine
```

This repository has an independent Git history exported from the development repository. Internal work notes, execution logs, dependency caches, generated code, and local authentication settings are excluded. Checks requiring commits from the old development history need that history or the relevant fixtures separately.

## Local development

### Prerequisites

| Tool | Purpose |
|---|---|
| Rust / rustup | `rust-toolchain.toml` pins Rust 1.95.0; needed to build the server from source |
| PostgreSQL | Server database; development Compose uses version 17 and also defines a version 16 profile |
| Docker Compose v2 | Only needed when running the database or server through Compose |
| Node.js 24.x / npm | SDK and SDK-consuming app development, following the SDK's `engines` declaration |
| Python 3.12+ | Design and dependency checks; OpenAPI checks additionally require `openapi-spec-validator` / `pyyaml` |

The Rust build script generates protocol code using `protox`, so a local `protoc` installation is unnecessary. The embedded administration UI does not require Node.js. The standalone reference 3D demo declares Node.js 22 or newer.

### Set up through the browser

Prepare an empty PostgreSQL database, then run from the repository root:

```sh
cargo run -p orbisync-server -- web-admin
```

Open the private URL on the same machine to configure the database connection, password denylist, first administrator, and engine startup. The UI supports Japanese and English. New setup generates keys and applies migrations. Do not use it to reinitialize an existing database.

The administration listener is restricted to `127.0.0.1`. Its default port is 8090; the engine defaults to 8080. The default `.orbisync-admin/` directory contains credentials and must not be committed. The UI's **Production** option selects the denylist mode; it does not certify the product for production use.

See the [Web setup guide](apps/admin-web/README.md) for saving/changing the initial password, restarts, existing installations, and headless operation.

### Start a development environment with Compose

These commands use Bash. On Windows, use Bash / WSL or the Web setup above.

```sh
cp deploy/compose/.env.example .env
bash scripts/gen-dev-password-denylist.sh
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d postgres
docker compose -f deploy/compose/compose.dev.yml --env-file .env run --rm --build --entrypoint orbisync-server server migrate
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d --build server
```

The environment example and generated denylist are for development. The server requires a 10,000-entry password corpus. The development generator's placeholders do not block real common passwords. Before real users sign in, supply an operator-approved corpus and independent secret keys. Compose does not configure TLS, backups, or monitoring.

To run the Rust server on the host, start only Compose's `postgres` service and supply the database URL and keys through environment variables. Make sure `DATABASE_URL` is reachable from the host.

```sh
set -a && . ./.env && set +a
export ORBISYNC_PASSWORD_DENYLIST_FILE=deploy/dev-password-denylist.txt
cargo run -p orbisync-server -- migrate
cargo run -p orbisync-server -- doctor
cargo run -p orbisync-server
```

Configuration precedence is CLI > environment > configuration file > defaults. See the [configuration example](orbisync.toml.example) and [development Compose configuration](deploy/compose/compose.dev.yml). `doctor` performs nondestructive checks of configuration, database connectivity, and migrations; it does not apply migrations.

To create the first administrator through the CLI, use a migrated database with no users. Put the credential output in a protected location outside the repository:

```sh
cargo run -p orbisync-server -- bootstrap-admin --login-id admin --display-name Administrator --password-denylist deploy/dev-password-denylist.txt --password-output ../orbisync-initial-admin-password.txt
```

Startup endpoints are `/health/live`, `/health/ready`, and `/version`. Consult [OpenAPI](openapi/orbisync-v1.yaml) for other operations and required permissions.

## Generate and build the TypeScript SDK

Generated protocol code and `dist/` are not tracked in Git. With Node.js 24.x, run these commands from the repository root after cloning and after proto changes:

```sh
npm install --global @bufbuild/buf@1.72.0 @bufbuild/protoc-gen-es@2.13.0
cargo install protoc-gen-prost --locked --version 0.5.0
buf generate
npm --prefix sdk/typescript ci
npm --prefix sdk/typescript run build
```

Both plugins must be on PATH. The root `buf.gen.yaml` writes TypeScript to `sdk/typescript/src/generated/` and Rust to `generated/rust/`. The Rust server's own build separately generates into `OUT_DIR`.

The SDK package entry point is `dist/index.js`. Neither `npm ci` nor `buf generate` alone completes the SDK build required by consuming apps. See [SDK development](sdk/typescript/DEVELOPMENT.md) and [distribution instructions](docs/consumer-kit/PRODUCING.md) for packaging and examples.

## Samples and administration apps

### REST / real-time reference console

**Generate and build the SDK above first.** You also need a running server, an account, and the permissions required by the operations you use.

```sh
npm --prefix apps/reference-console-web ci
npm --prefix apps/reference-console-web run dev
```

The default URL is `http://localhost:5173`. The console demonstrates login, worlds/instances, entity operations, and resume. [Details](apps/reference-console-web/README.md)

### Separate administration console

This Vite app operates the existing REST API and is separate from the embedded setup UI.

```sh
npm --prefix apps/admin-console-web ci
npm --prefix apps/admin-console-web run dev
```

The default URL is `http://localhost:5174`. [Details](apps/admin-console-web/README.md)

For a host-run API, allow the UI's origin before starting the server. Development Compose configures the following origins:

```sh
export ORBISYNC_CORS_ALLOWED_ORIGINS=http://localhost:5173,http://127.0.0.1:5173,http://localhost:5174,http://127.0.0.1:5174
```

### Reference 3D demo: Starlight Island

```sh
npm --prefix apps/reference-web ci
npm --prefix apps/reference-web run dev
```

This app generates the TypeScript protocol using its own dependencies before dev/build/check. It does not need the global plugins above or a prebuilt SDK package. Its configuration requires Node.js 22 or newer.

Single-player mode needs no server. Online mode requires an account and a joinable instance, and shares position and orientation. Item collection, scores, and completion remain local to each client. The game UI is Japanese. [Startup and controls](apps/reference-web/RUNNING.md)

Both the console and demo default to port 5173. When running them together, choose a separate port and configure the corresponding CORS origin.

## Verification and limitations

CI runs automatically on pushes to `main` and pull requests targeting `main`. View the [run results](https://github.com/machitorimh-cmd/orbisync-engine/actions/workflows/ci.yml). SDK CI uses Node.js 24.

Examples of checks to select for your task follow. This is not a record that these commands were run for this documentation update.

```sh
python scripts/validate_design.py --review
python scripts/check_architecture.py
# After SDK dependencies and proto generation
npm --prefix sdk/typescript run check
```

Some SDK tests require a prebuilt Rust `orbisync-e2e-helper`. Database and browser checks have their own prerequisites. Consult [SDK development](sdk/typescript/DEVELOPMENT.md) and [test design](docs/design/test-and-ci.md) before running them. `scripts/bootstrap-dev.sh` runs workspace-wide checks in addition to preparing the database; it is not a lightweight startup-only command.

This source distribution does not bundle a public container image. It provides no certification of 24-hour continuous operation or 1,000 connections. Configure TLS, keys, the password denylist, backup/restore, and performance evaluation for your deployment.

## Documentation

- [Frontend integration](docs/guides/frontend-integration.md) / [SDK consumer guide](docs/consumer-kit/README.md)
- [Web setup and administration](apps/admin-web/README.md) / [Password recovery](docs/operations/admin-password-recovery.md)
- [Architecture](docs/design/architecture.md) / [Crate layout and dependency rules](docs/design/repo-crate-conventions.md)
- [Design index](docs/design/README.md) / [ADRs](docs/adr/README.md) / [Original specification](metaverse_core_specification.md)
- [Operations runbooks](docs/operations/README.md) / [Threat model](docs/security/threat-model.md)
- [Extension API](docs/guides/extension-command-api.md) / [External input rules](docs/consumer-kit/EXTERNAL-RULES.md)

Some detailed documents are Japanese-only or contain historical design and verification records. The Japanese README is [README.md](README.md).

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). Use Issues for ordinary bugs and proposals. Do not publish vulnerabilities or secrets; follow [SECURITY.md](SECURITY.md).

## License

Available under your choice of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). SPDX: `MIT OR Apache-2.0`.

Copyright © 2026 avistoria
