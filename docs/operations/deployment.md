# Deployment — Docker Compose Startup

How to bring up OrbiSync locally with Docker Compose. The Compose file under `deploy/compose/` is the development topology (PostgreSQL + server). Production terminates TLS in an external reverse proxy (ADR-009); this Compose file does not provide TLS, backups, or monitoring — configure those separately per `docs/design/deployment-and-threat-model.md` §1.2.

> References: `docs/design/deployment-and-threat-model.md`, `docs/design/repo-crate-conventions.md` §1.2 / §6.2, `metaverse_core_specification.md` §37.

## Prerequisites

For a lost administrator password on an existing installation, follow
[local administrator password recovery](admin-password-recovery.md).

| Tool | Purpose |
|------|---------|
| Docker Engine + Compose v2 | PostgreSQL and optional server container |
| Rust 1.95 (rustup) | `cargo run -p orbisync-server` from host |
| Node 18+ (optional) | Live `apps/reference-web` and `apps/admin-web` browser clients |
| `cp` / shell | env setup |

`protoc` is not required — `orbisync-protocol` builds `proto/` with `protox` (ADR-004).

## 1. Configure environment

```bash
# from repo root
cp deploy/compose/.env.example .env
# edit .env: replace every secret placeholder and set independent keys before starting the server
```

Secrets are injected via environment variables only; never write them into `orbisync.toml` (`observability-and-config.md` §6.4).

Key variables (see `deploy/compose/.env.example`):

| Var | Default in example | Notes |
|-----|--------------------|-------|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `orbisync` / `orbisync-local-dev` / `orbisync` | Postgres container creds |
| `DATABASE_URL` | `postgres://orbisync:…@localhost:5432/orbisync` | Host-side URL. Inside the `server` container use `postgres` as host, not `localhost`. |
| `ORBISYNC_TOKEN_SIGNING_KEY` | `__REPLACE_WITH_ED25519_PRIVATE_PEM__` (see `.env.example`) | Ed25519 PKCS#8 PEM private key. Generate via `openssl genpkey -algorithm ed25519`; server derives the public key and fails startup if invalid |
| `ORBISYNC_PAGINATION_HMAC_KEY` | `development-only-pagination-hmac-key` | Pagination cursor HMAC key, separate from the token signing key (V-04/CR-07). Generate a real key for any shared deployment |
| `ORBISYNC_REFRESH_TOKEN_HMAC_KEY` | `__REPLACE_WITH_RANDOM_32_BYTES__` | Non-empty HMAC key for refresh-token digests; keep separate from every other HMAC key |
| `ORBISYNC_REALTIME_TICKET_HMAC_KEY` | `__REPLACE_WITH_RANDOM_32_BYTES__` | Non-empty HMAC key for realtime-ticket digests; keep separate from every other HMAC key |
| `ORBISYNC_REALTIME_ALLOW_STUB_TICKET` | `false` | Explicit local-only override; keep false for shared/production deployments |
| `ORBISYNC_IDEMPOTENCY_HMAC_KEY` | `__REPLACE_WITH_RANDOM_32_BYTES__` | Non-empty HMAC key for idempotency request hashes; keep separate from every other HMAC key |
| `ORBISYNC_EXTENSION_TOKEN_HMAC_KEY` | `__REPLACE_WITH_RANDOM_32_BYTES__` | Required at startup; at least 32 bytes, independent from other keys. Used for extension-token digests; changing it invalidates existing extension tokens. See [extension setup](../guides/extension-command-api.md). |
| `ORBISYNC_HOST_BIND` / `ORBISYNC_PORT` / `POSTGRES_PORT` | `127.0.0.1` / `8080` / `5432` | Compose host bind and port mappings; keep the host bind loopback for local/stub use |

## 2. Start PostgreSQL

```bash
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d postgres
# wait for healthy
docker compose -f deploy/compose/compose.dev.yml ps
docker compose -f deploy/compose/compose.dev.yml logs postgres --tail 20
```

To also exercise the PostgreSQL 16 support target (ADR-005):

```bash
docker compose -f deploy/compose/compose.dev.yml --profile pg16 --env-file .env up -d postgres16
```

## 3. Run migrations and start the server (from host)

```bash
# load .env into shell (bash)
set -a; . ./.env; set +a
# PowerShell: set each var manually, e.g. $env:DATABASE_URL=...

cargo run -p orbisync-server -- migrate
cargo run -p orbisync-server -- doctor
cargo run -p orbisync-server
# or: cargo run -p orbisync-server -- --config orbisync.toml.example
```

`scripts/bootstrap-dev.sh` does the above plus design checks in one go.

`doctor` is read-only: it checks the merged configuration, presence of required secret variables without printing their values, PostgreSQL connectivity, applied migration versions/checksums, the checkpoint table, the password denylist, and bind-address availability. It exits `0` only when every required check passes. Use `doctor --json` for a stable machine-readable report. Run it after `migrate`; an empty database intentionally fails the migration and checkpoint checks.

## 4. Start the server as a container (alternative)

```bash
docker compose -f deploy/compose/compose.dev.yml --profile server --env-file .env up -d --build server
docker compose -f deploy/compose/compose.dev.yml logs server --tail 40
# When using the container, DATABASE_URL must point at host `postgres`, e.g.:
# DATABASE_URL=postgres://orbisync:orbisync-local-dev@postgres:5432/orbisync
```

## 5. Verify health

```bash
curl -i http://localhost:8080/health/live   # 200 when process is up
curl -i http://localhost:8080/health/ready  # 200 when DB pool is connected, else 503
curl http://localhost:8080/version
```

Ready semantics: `live` is process liveness; `ready` requires DB connectivity (`deployment-and-threat-model.md` §1.3).

For a compact operator-only summary, use an access token whose role has
`admin.diagnostics.read`:

```bash
curl -H "Authorization: Bearer $ACCESS_TOKEN" \
  http://localhost:8080/v1/admin/diagnostics
```

The response combines the newest durable checkpoint timestamp, checkpoint
error counters, bounded queue observations and configured capacities, active
connections, rate-limit rejection totals, extension outbox/DLQ state, and the
last successful retention ticks. It never returns credentials, payloads, or
user/instance identifiers. A `null` retention timestamp means this process has
not completed that worker successfully since startup. Existing installations
must add `admin.diagnostics.read` to the intended operator role; newly
bootstrapped administrators receive it automatically. Do not expose this
endpoint as a public health check.

## 6. Reference and admin web clients

```bash
npm --prefix apps/reference-web install
npm --prefix apps/reference-web run dev   # http://localhost:5173

npm --prefix apps/admin-web install
npm --prefix apps/admin-web run dev       # http://localhost:5174
```

The default development CORS list includes both `localhost` and `127.0.0.1`
for ports 5173 and 5174. Set the Server field to the matching API origin.
Both applications require a running server and use its real APIs; neither
falls back to fake data. Reference Web covers login, World/Instance creation,
realtime join, Entity operations, and automatic resume. Admin Web covers the
operator REST workflows.

## 7. Stop and clean

```bash
docker compose -f deploy/compose/compose.dev.yml --env-file .env down
# to also remove data:
docker compose -f deploy/compose/compose.dev.yml --env-file .env down -v
```

## Compose file reference

- `deploy/compose/compose.dev.yml` — dev topology (`orbisync-dev`): `postgres`, `postgres16` (profile `pg16`), `server` (profile `server`, built from `deploy/docker/Dockerfile`).
- `deploy/compose/.env.example` — copy to `.env` at repo root.
- `deploy/docker/Dockerfile` — multi-stage build; runtime ships the single binary only (TD-10), runs as non-root, healthcheck via `/health/live`.

## Troubleshooting

- `POSTGRES_PASSWORD must be set` / `DATABASE_URL must be set` / `ORBISYNC_TOKEN_SIGNING_KEY must be set` / `ORBISYNC_PAGINATION_HMAC_KEY must be set` — Compose requires them (`compose.dev.yml` uses `${VAR:?must be set}` so a missing value fails with a clear message); ensure `--env-file .env` is passed and `.env` is at the expected path.
- `cargo run` fails on missing `ORBISYNC_TOKEN_SIGNING_KEY` / `ORBISYNC_PAGINATION_HMAC_KEY` / `DATABASE_URL` — startup validation (`Config::verify_secrets`) exits early (spec §28.1); export from `.env` before invoking cargo.
- `ready` returns 503 — DB not yet healthy or `DATABASE_URL` host mismatch (`localhost` vs `postgres` when using the container).
- Port conflicts — change `POSTGRES_PORT` / `ORBISYNC_PORT` in `.env`.
- `doctor` reports `migrations` or `checkpoint_store` as failed — run `orbisync-server migrate` with the same configuration and database URL, then rerun `doctor` before starting the server. The command never applies migrations itself.

## Production notes

- Pin OCI image tags; never use `latest` in production (spec §37.2, `release-maintenance-license.md` §1.5).
- Terminate TLS in a reverse proxy; keep `/health/*` on an internal network (`deployment-and-threat-model.md` §1.2–1.3, ADR-009).
- Backups/restore and secret rotation are in `docs/operations/backup-restore.md` / `secret-rotation.md`.

### Reverse-proxy timeout requirements

The application applies a configurable `server.request_timeout_seconds`
deadline (30 seconds by default) across buffering a normal HTTP request body
and executing its handler. The legacy body-read override remains separate and
cannot extend the overall request deadline. This is defense in depth, not a
replacement for the edge configuration. Configure the supported external
proxy/load balancer with all
of the following requirements before exposing the server:

- header/read timeout: **10 seconds or less**;
- request-body upload timeout: **30 seconds or less** for the maximum 2 MiB
  request body (and an explicit minimum upload rate, if the proxy supports it);
- upstream response timeout: **35 seconds or less** for ordinary REST requests;
- idle/keep-alive timeout: **60 seconds or less** for HTTP, but do **not** apply
  this as a maximum lifetime to `/ws` or `/v1/realtime/ws` WebSocket upgrades.

The server's request deadline is scoped to the ordinary HTTP router. WebSocket
upgrade and long-lived connection routes are mounted separately and must use
the proxy's WebSocket-aware idle/heartbeat policy. Verify these values with a
slow-body probe through the deployed proxy after every proxy configuration
change; a direct server test cannot prove the external timeout.

### Realtime ticket verification

- `realtime.allow_stub_ticket` defaults to `false`. With the default, the server composes `HmacRealtimeTicketVerifier` with the PostgreSQL `realtime_tickets` store. `DATABASE_URL` and the dedicated `ORBISYNC_REALTIME_TICKET_HMAC_KEY` are startup requirements; the server exits before serving if the HMAC secret is missing or empty. The pool is lazy, so an unavailable database leaves `/health/ready` at `503`; run migrations before using the ticket endpoint. The verifier atomically consumes a ticket, requires its associated `auth_sessions` row to be active and unexpired, and therefore enforces single-use tickets.
- The client flow is: authenticate with an access token, `POST /v1/realtime/tickets`, then immediately connect to `GET /ws` (or `GET /v1/realtime/ws`) and send the returned `realtime_ticket` in `ClientHello`. Tickets are opaque, short-lived (60 seconds), and are not access tokens; do not put either value in logs.
- Env override: `ORBISYNC_REALTIME_ALLOW_STUB_TICKET`. File key: `realtime.allow_stub_ticket` (`orbisync.toml.example`). Keep the setting `false` in every shared or production deployment.
- Setting it to `true` explicitly selects `StubTicketVerifier`: every non-empty ticket is accepted and a fabricated user identity is assigned. This is **non-production only** and provides no authentication. For host `cargo run`, bind the server to loopback (for example `ORBISYNC_SERVER_BIND=127.0.0.1:8080`). For Compose, keep the container bind at `0.0.0.0:8080` but leave `ORBISYNC_HOST_BIND=127.0.0.1` so the published port remains host-local. Do not publish port 8080 through a reverse proxy, firewall, port-forward, or container host interface. Never combine stub mode with a public bind or public TLS endpoint.
- The sample and UI clients use the real ticket endpoint and `/ws`; they do not require stub mode when connected to a server with a configured database and valid access credentials. Enable stub mode only for deliberately offline/local development where fabricated identity is acceptable.

### Checkpoint retention

- `world.checkpoint_interval_secs` (default `300`, valid `60..3600`) controls how often the server persists instance checkpoints. The ticker still runs at `world.server_tick_hz` (1–60 Hz) but checkpointing is time-based (`Instant::elapsed() >= checkpoint_interval`), so changing `server_tick_hz` no longer changes checkpoint frequency.
- Env override: `ORBISYNC_WORLD_CHECKPOINT_INTERVAL_SECS`. File key: `world.checkpoint_interval_secs` (`orbisync.toml.example`).
- Retention: `PgCheckpointStore::save_checkpoint` prunes after each save to keep only the latest 3 checkpoints per instance (`DELETE … WHERE instance_id=$1 AND id NOT IN (SELECT id … ORDER BY revision DESC LIMIT 3)`) and deletes rows older than 30 days (`created_at < NOW() - INTERVAL '30 days'`). Without this, `instance_checkpoints` would grow without bound (every `checkpoint_interval_secs` per live instance).
- On first join after process start or idle reap, the server loads the latest checkpoint before publishing the instance actor. Store errors, malformed payloads, row/payload identity or revision mismatches, and invalid domain values fail the join closed; they never create an empty replacement actor.
- Checkpoint JSON format version 2 persists entity IDs, kind, owner, transform, structured visibility, revision/timestamps, and custom components. Velocity, animation, presence, and live membership remain deliberately ephemeral. Version 1 payloads are accepted and all visibility forms produced by the former Debug-string encoder are migrated in memory; custom components cannot be recovered from version 1 because that encoder never stored them.
- `checkpoint_save_rejected_total` (log event `checkpoint.save_rejected_too_large`) counts checkpoints refused because the serialized payload exceeded the configured byte limit (`MAX_CHECKPOINT_PAYLOAD_BYTES`, currently an interim 8 MiB derived from the process memory budget — see the derivation comment on that constant). This metric rising means at least one world grew past the limit: its in-memory state keeps serving, but it is no longer durable, so a restart or idle reap loses that world's state. Treat any increase as an incident: identify the instance from the `checkpoint.save_rejected_too_large` log entry, shrink the world (delete entities or reduce component payload sizes) so its checkpoint fits again, then trigger a save by letting the instance checkpoint or rejoin. The world-growth limit itself is tracked as a separate work item; this counter exists so the failure is never silent.
