# OrbiSync Load Generator — Real Traffic Driver

Real `load_test_tooling` driver for `docs/design/scale-and-nfr.md` §9 and `docs/design/test-and-ci.md` §2.6: drives the public contract (`OpenAPI` + `realtime.proto`) as `login -> realtime ticket -> WebSocket (ClientHello -> JoinInstance) -> 10 Hz TransformInput` for `N` concurrent virtual users. Measures observed join latency, transform round-trip, messages received, and failure counts.

## Placement

```
apps/load-generator/   # workspace-external binary (see repo-crate-conventions.md §2.1)
```

This crate is **not** a member of the Cargo workspace (`crates/*`). It must not depend on `orbisync-*` server crates — it drives the server via public API/protocol only.

## Build

```bash
cargo check --manifest-path apps/load-generator/Cargo.toml
cargo build  --manifest-path apps/load-generator/Cargo.toml
# or from within the app dir:
cd apps/load-generator
cargo check
cargo build --release
```

Requires Rust 1.95 (see `rust-toolchain.toml`).

## Run

```bash
# Secure: password via env var (never CLI — leaks via ps/history) or file
export ORBISYNC_LOAD_PASSWORD="$(cat .admin-pass.txt)"
cargo run --manifest-path apps/load-generator/Cargo.toml -- --users 50 --hz 10 --duration 10 --login-id admin --instance-id "$INSTANCE"
# alternative: use a file
cargo run --manifest-path apps/load-generator/Cargo.toml -- --users 50 --hz 10 --duration 10 --login-id admin --password-file .admin-pass.txt --instance-id "$INSTANCE"

# with overrides
cargo run --manifest-path apps/load-generator/Cargo.toml -- --server-url http://127.0.0.1:18081 --users 20 --hz 10 --duration 5 --instance-id "$INSTANCE"
cargo run --manifest-path apps/load-generator/Cargo.toml -- --help
```

> **Security:** Do not pass `--password` on the CLI or embed it in `curl -d '{"password":"..."}'` — both leak via `/proc/*/cmdline` / shell history. Use `ORBISYNC_LOAD_PASSWORD` env var or `--password-file` (0600 file, as produced by `bootstrap-admin --password-output`). For `curl`, build JSON via file: `jq -n --arg pw "$(cat .admin-pass.txt)" '{"login_id":"admin","password":$pw}' > /tmp/login.json && curl -d @/tmp/login.json …; shred -u /tmp/login.json`.

### CLI

| Flag | Default | Description |
|------|---------|-------------|
| `--server-url` | `http://127.0.0.1:8080` | Base HTTP URL |
| `--ws-url` | _(derived)_ | WebSocket URL override (derived from `--server-url` as `ws(s)://host/ws` when empty) |
| `--users` | `50` | Virtual users (Phase 1: 50, Phase 2: 200) |
| `--hz` | `10` | Transform send rate per user (Hz) |
| `--duration` | `10` | Duration in seconds |
| `--login-concurrency` | `4` | Maximum login requests in flight; bounds password-hash pressure |
| `--placement` | `grid` | Spread avatars (`grid`) or keep them at the origin (`origin`) |
| `--login-id` | `admin` | Login identifier |
| `--password` | _(env)_ | Password (prefer `ORBISYNC_LOAD_PASSWORD` env or `--password-file`) |
| `--password-file` | _(none)_ | File containing password (takes precedence, 0600) |
| `--instance-id` | _(auto)_ | World instance to join; when empty, creates world+instance |
| `--commit` | _(auto)_ | Git commit recorded in report (`$GIT_COMMIT` or `git rev-parse HEAD` or `unknown`) |
| `--no-auto-create` | `false` | Do not auto-create world/instance |
| `--scenario` | `transform` | `login`, `ramp`, `transform`, `reconnect`, `slow-consumer`, `single-instance`, or `multi-instance` |
| `--ramp-rate` | `50` | Connections per second for `ramp` |
| `--reconnect-percent` | `10` | Selected users for `reconnect` |
| `--reconnect-interval` | `10` | Reconnect interval parameter in seconds |
| `--slow-consumer-percent` | `5` | Selected users for `slow-consumer` |
| `--slow-consumer-delay-ms` | `250` | Intentional read delay for slow consumers |
| `--metrics-url` | _(none)_ | Internal Prometheus endpoint; omitted means server metrics are unavailable |
| `--metrics-interval` | `5` | Metrics scrape interval in seconds |
| `--json-output` | _(none)_ | Path for a machine-readable JSON report |

## Output

Reports honest percentiles with sample sizes and failure prominence (a load test that cannot report failure is not a load test). Network throughput uses actual WebSocket frame bytes, and Snapshot sizes are measured after all chunks for a logical Snapshot are reassembled:

```
OrbiSync load-generator report (real network)
  server_url: http://127.0.0.1:18081
  ws_url:     ws://127.0.0.1:18081/ws
  commit:     abc1234
  users:      20
  hz:         10.00
  duration:   5s
  ticks_per_user: 50
  total_ops_expected: 1000
  instance_id: 019ff537-...

Results (wall 9.61s, requested 5s):
  login_ok:   20/20 (fail 0)
  ticket_ok:  20/20 (fail 0)
  ws_connected: 20/20 (fail 0)
  hello_ok:   20/20 (fail 0)
  join_ok:    20/20 (fail 0)
  join latency (n=20): min 2.12 ms  mean 10.80 ms  p50 2.97 ms  p95 42.54 ms  p99 43.11 ms  max 43.11 ms
  transform rtt samples: 1000 (received)
  rtt (n=1000): min 0.88 ms  mean 18.68 ms  p50 8.04 ms  p95 46.54 ms  p99 49.06 ms  max 54.03 ms
  FAILURES ARE PROMINENT: join_fail=0 errors=0
  PARAMS: server_url=http://127.0.0.1:18081 users=20 hz=10.00 duration=5s commit=abc1234
```

Percentiles use ceil-rank. `PARAMS` records reproducibility.

## Design

- Real network I/O via `reqwest` + `tokio-tungstenite` + `prost` against public OpenAPI + `realtime.proto` (no workspace crate deps per `repo-crate-conventions.md` §2.1).
- Percentiles computed over sorted `Duration` vec (ceil-rank method).
- `apps/load-generator/Cargo.toml` declares its own `[workspace]` to remain standalone; workspace lints are intentionally relaxed.
- Password handling: `ORBISYNC_LOAD_PASSWORD` env or `--password-file` (0600); `Debug` redacts password; `--password` CLI emits warning that it leaks.
- Server metrics are scraped at test start, periodically, and at test end. Only metrics implemented by the server are parsed; missing metrics and scrape failures are reported as unavailable rather than as zero. CPU is calculated from the delta of `process_cpu_seconds_total` divided by elapsed scrape time. Slow-consumer reports use the server's `outbound_queue_depth`/`outbound_queue_depth_max` and byte high-water marks, rather than the instance mailbox metric.
- Every run records the scenario, test window timestamps, commit, connection count, server URL, disconnect reasons, dropped updates, frame-byte throughput, and JSON-safe metric samples.

## Scenario mapping

The seven load-test scenarios from `docs/design/test-and-ci.md` are selectable with `--scenario`: concurrent login, staged connection ramp, 10 Hz transform traffic, 10% reconnect, mixed slow consumers, one concentrated instance, and distributed instances. Ramp JSON records each stage's join latency plus the observed queue/RSS values; degradation onset is the first stage over the explicit 2x best-observed join-latency threshold, or unavailable when no stage crosses it. The slow-consumer report exposes the observed outbound queue depth and depth/byte high-water marks; unavailable metrics are never assumed to be zero.
