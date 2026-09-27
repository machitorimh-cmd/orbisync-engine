//! Shared helper for V-07: make DB skips visible and CI-strict.
//!
//! - When `DATABASE_URL` is unset/empty, DB tests are skipped with a distinctive
//!   message on both stdout and stderr (`SKIPPED: DATABASE_URL ... (V-07)`).
//!   Local `cargo test --workspace` still passes, but the skip is explicit and
//!   `grep SKIPPED` distinguishes it from success.
//! - When `DATABASE_URL` is set to an empty/whitespace value, skipping is always
//!   treated as a failure via panic — this is a misconfiguration.
//! - When `DATABASE_URL` is unset and `ORBISYNC_REQUIRE_DB` is set, skipping is
//!   treated as a failure via panic — CI never silently passes with DB tests
//!   skipped. `ORBISYNC_REQUIRE_DB` is the sole strictness signal; `CI` /
//!   `GITHUB_ACTIONS` are intentionally NOT consulted. GitHub Actions sets
//!   `CI` and `GITHUB_ACTIONS` on every job, including `build-and-test` which
//!   has no PostgreSQL service. If those implied strictness, `build-and-test`
//!   would fail whenever it skips DB tests (the CI-1 bug, run 32561994284).
//!   The integration job sets `ORBISYNC_REQUIRE_DB=1` in `.github/workflows/ci.yml`,
//!   so V-07's intent (prevent silent skips where DB is expected) is still
//!   enforced, but only where DB is actually provided.
//! - All DB-requiring integration tests must go through this helper so the
//!   behaviour is uniform (`identity_persistence.rs`, `migrations.rs`, etc.).

#![allow(dead_code)]

use std::future::Future;

use orbisync_config::DatabaseConfig;
use orbisync_storage_postgres::run_migrations;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Returns the isolated test pool configuration.
///
/// This is intentionally NOT derived from `Config::default()`.
///
/// Production defaults in `crates/orbisync-config/src/model.rs` (`Config::default()`)
/// are free to evolve (e.g. tuning `database.max_connections` for deployed
/// throughput). If tests reused `Config::default()` and overwrote only
/// `max_connections`, a production change would silently alter every other
/// database-related default that tests inherit — or, if the override were
/// forgotten, would directly change the number of connections each test
/// binary opens. `W-29` showed this failure mode: the production default of
/// `20` leaked into tests and `5 binaries * 20 = 100` hit the PostgreSQL
/// `max_connections` ceiling, while the true cost was obscured.
///
/// To keep the blast radius zero, the test pool is defined as a self-contained
/// literal below. Every field is set explicitly and commented with why that
/// value was chosen. Adding a field to `DatabaseConfig` will break compilation
/// here, which is intentional — the test author must decide the test-appropriate
/// value rather than inheriting a production one implicitly.
pub fn test_database_config() -> DatabaseConfig {
    DatabaseConfig {
        // `url_env` names the environment variable that carries the connection
        // string in production (`DATABASE_URL` by convention). The integration
        // test pool does not read the URL through this indirection — `pool_or_skip`
        // fetches `DATABASE_URL` from the environment and passes the concrete
        // string to `create_pool` as the second argument. The field is still
        // required by `DatabaseConfig`, so it is set explicitly to the
        // conventional name for documentation and for any future diagnostic that
        // prints the struct. It is NOT used to resolve the URL at test runtime.
        url_env: "DATABASE_URL".to_owned(),

        // `max_connections` — upper bound per `PgPool` (per test binary).
        //
        // Why `5`:
        // - PostgreSQL's default `max_connections` is `100` (with
        //   `superuser_reserved_connections = 3`), so only `97` connections
        //   are usable by unprivileged clients. `cargo test --workspace`
        //   runs each `tests/integration/tests/*.rs` file as an independent
        //   test binary, plus one binary per workspace crate that has unit
        //   tests. All binaries run in parallel by default (no
        //   `--test-threads=1`). In this repository there are ~16 integration
        //   test binaries (`auth_w13`, `backpressure_w17`, `entity_w16`,
        //   `identity_persistence`, `migrations`, etc.). Worst-case demand is
        //   therefore `num_binaries * max_connections`.
        // - With the production default `20`, the old formula
        //   `5 * 20 = 100` already saturated the server, and
        //   `16 * 20 = 320` would exceed it by 3x. `W-29` hit
        //   `5 simultaneously active binaries * 20 = 100` during
        //   investigation and lengthened root-cause analysis.
        // - With `5`, usable demand is `16 * 5 = 80` connections, comfortably
        //   below `97` with headroom for background workers and ad-hoc
        //   connections. Even the minimal W-29 scenario (`5 * 5 = 25`) leaves
        //   `72` spare connections.
        // - Per-test concurrency is low: each test creates a single `PgPool`
        //   and runs mostly sequential queries/migrations; a handful of
        //   concurrent checkouts (e.g. parallel sub-tests inside one binary)
        //   is sufficient. Larger pools increase connection churn and
        //   slow shutdown without improving throughput for these tests.
        // - Keep this small and explicit. If PostgreSQL's `max_connections`
        //   is raised in deployment, tests do not automatically scale up and
        //   hide future contention; if it is lowered, tests remain safe.
        max_connections: 5,
        acquire_timeout_seconds: 2,
        readiness_timeout_seconds: 3,
    }
}

fn is_ci_strict() -> bool {
    // Strictness is gated SOLELY on `ORBISYNC_REQUIRE_DB`.
    // Rationale (CI-1): `CI` and `GITHUB_ACTIONS` are set on every GitHub Actions
    // job, including `build-and-test` which has no Postgres service. Using them
    // as a strictness signal made "DB not set → skip" turn into a panic in
    // `build-and-test`, breaking CI (run 32561994284: 8 tests FAILED with
    // `SKIPPED (V-07)`). Only the `integration` job sets `ORBISYNC_REQUIRE_DB=1`
    // (see `.github/workflows/ci.yml:167` env), so V-07's guarantee — "a job
    // that is supposed to have DB must not silently skip" — is preserved
    // without poisoning DB-less jobs.
    std::env::var("ORBISYNC_REQUIRE_DB").is_ok()
}

/// Distinctive skip marker — grep for `V-07` or `SKIPPED.*DATABASE_URL`.
const SKIP_UNSET_MSG: &str = "SKIPPED (V-07): DATABASE_URL not set - DB integration tests skipped; set DATABASE_URL to run PostgreSQL-backed tests";
const SKIP_EMPTY_MSG: &str = "SKIPPED (V-07): DATABASE_URL is set but empty/whitespace - refusing to silently skip DB tests; provide a valid URL or unset DATABASE_URL to allow local skip";

/// Returns a migrated `PgPool` when `DATABASE_URL` is set, otherwise prints a
/// distinctive skip message and returns `None` (local) or panics (CI).
#[track_caller]
pub fn pool_or_skip() -> impl Future<Output = Option<PgPool>> {
    let caller = std::panic::Location::caller();
    // Include caller location so "which test was skipped" is visible and
    // grep -c "SKIPPED (V-07)" counts distinct skips across the workspace.
    // Using a sync wrapper that returns a Future preserves the caller location
    // (async fn + track_caller is a no-op, see #110011).
    let skip_unset_with_caller =
        format!("{} at {}:{}", SKIP_UNSET_MSG, caller.file(), caller.line());
    let skip_empty_with_caller =
        format!("{} at {}:{}", SKIP_EMPTY_MSG, caller.file(), caller.line());
    async move {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.trim().is_empty() => {
                // W-29 note: the observed timeout was primarily CPU-bound Argon2id
                // in dev builds (3.1s vs 0.187s with `profile.dev.package.argon2`
                // opt-level=3). The pool headroom fix (`max_connections = 5`) is
                // kept because `Config::default().database.max_connections` (20)
                // previously leaked into tests and caused `num_binaries *
                // max_connections` to saturate PostgreSQL's `max_connections = 100`
                // (see `test_database_config` docs). This pool is intentionally
                // isolated from production defaults.
                let db_config = test_database_config();
                // Integration tests exercise SET ROLE for privilege boundaries. Reset
                // the session role before every connection is returned to the pool so
                // a later checkout cannot inherit an operator/runtime role.
                let pool = PgPoolOptions::new()
                    .max_connections(db_config.max_connections)
                    .acquire_timeout(std::time::Duration::from_secs(
                        db_config.acquire_timeout_seconds,
                    ))
                    .after_release(|connection, _| {
                        Box::pin(async move {
                            sqlx::query("RESET ROLE").execute(connection).await?;
                            Ok(true)
                        })
                    })
                    .connect_lazy(&url)
                    .expect("pool is created");
                run_migrations(&pool).await.expect("migrations apply");
                Some(pool)
            }
            Ok(_) => {
                // DATABASE_URL is set but empty/whitespace
                eprintln!("{skip_empty_with_caller}");
                println!("{skip_empty_with_caller}");
                // In any environment where DATABASE_URL is considered "set", skipping is a failure.
                // This satisfies: "DATABASE_URL が設定されている環境でスキップが発生したら、それは異常".
                panic!("{skip_empty_with_caller} - failing instead of silently skipping (V-07)");
            }
            Err(_) => {
                eprintln!("{skip_unset_with_caller}");
                println!("{skip_unset_with_caller}");
                if is_ci_strict() {
                    panic!(
                        "{skip_unset_with_caller} - CI/ORBISYNC_REQUIRE_DB requires DATABASE_URL; failing instead of silently skipping (V-07)"
                    );
                }
                // Help local `cargo test --workspace` make it obvious even without --nocapture:
                // the test harness captures stdout/stderr for passing tests, so also ensure
                // the message appears in the overall run via the per-test skip return.
                // The caller does `let Some(pool) = pool_or_skip().await else { return; };`
                // and the test will show as `ok`, but the skip is distinguishable by
                // running `cargo test -- --nocapture 2>&1 | grep -F "SKIPPED (V-07)"`
                // and by the per-test eprintln above.
                None
            }
        }
    }
}

/// Returns `Some(url)` when `DATABASE_URL` is set and non-empty, otherwise the
/// same V-07 skip/fail semantics as `pool_or_skip`.
#[track_caller]
pub fn url_or_skip() -> Option<String> {
    let caller = std::panic::Location::caller();
    let skip_unset_with_caller =
        format!("{} at {}:{}", SKIP_UNSET_MSG, caller.file(), caller.line());
    let skip_empty_with_caller =
        format!("{} at {}:{}", SKIP_EMPTY_MSG, caller.file(), caller.line());
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        Ok(_) => {
            eprintln!("{skip_empty_with_caller}");
            println!("{skip_empty_with_caller}");
            panic!("{skip_empty_with_caller} - failing instead of silently skipping (V-07)");
        }
        Err(_) => {
            eprintln!("{skip_unset_with_caller}");
            println!("{skip_unset_with_caller}");
            if is_ci_strict() {
                panic!("{skip_unset_with_caller} - CI requires DATABASE_URL; failing (V-07)");
            }
            None
        }
    }
}

/// Helper for tests that want a hard requirement (e.g. `revision_w28`).
/// Panics with a clear message when DB is required, otherwise same skip-or-pool.
/// Retained for call sites that previously used `.expect("DATABASE_URL must be set")`.
pub async fn require_pool() -> PgPool {
    pool_or_skip()
        .await
        .expect("DATABASE_URL must be set for this DB test (V-07: set DATABASE_URL or run without CI to allow skip with SKIPPED marker)")
}

#[allow(dead_code)]
pub fn require_url() -> String {
    url_or_skip().expect("DATABASE_URL must be set for this DB test (V-07)")
}

/// Minimal checkpoint store for WebSocket tests whose scenarios never persist
/// a checkpoint. Activation (HIGH-001) fails closed when no store is
/// configured, so join-driven tests must wire one explicitly; this fake
/// restores an empty instance and accepts saves without touching a database.
///
/// `save_checkpoint` must still return a `CheckpointSaveReceipt` for every
/// dedup entry in the saved payload: `persist_command_outcome_inner`
/// (`realtime_ws_connection_runtime.rs`) looks up the just-saved command's
/// receipt in the returned list and keeps the command fenced as
/// `PERSISTENCE_UNAVAILABLE` until it finds one. The extraction below is
/// deliberately field-for-field identical to
/// `PgCheckpointStore::dedup_receipts` (`orbisync-storage-postgres/src/checkpoint.rs`):
/// same source field (`data["dedup"]`), same three fields read per entry,
/// same missing-field error kind (`PortFailure` via a checkpoint-dedup
/// error), same order (array iteration order, unfiltered), same empty-array
/// fallback. The one property this fake cannot reproduce is
/// `PgCheckpointStore::merge_same_revision`: a real store merges the
/// incoming payload with any already-durable same-revision row before
/// deriving receipts, so a concurrent racing writer's receipts would include
/// entries this fake never sees. No test using this fake exercises
/// concurrent writers to the same instance, so that gap does not currently
/// weaken any assertion.
pub struct NoopCheckpointStore;

#[async_trait::async_trait]
impl orbisync_application::CheckpointStore for NoopCheckpointStore {
    async fn save_checkpoint(
        &self,
        checkpoint: orbisync_application::AppCheckpoint,
    ) -> Result<
        Vec<orbisync_application::CheckpointSaveReceipt>,
        orbisync_application::ApplicationError,
    > {
        let map_err = |detail: &str| {
            orbisync_application::ApplicationError::new(
                orbisync_application::ApplicationErrorKind::PortFailure,
                detail.to_owned(),
            )
        };
        let data: serde_json::Value = serde_json::from_slice(&checkpoint.payload)
            .map_err(|_| map_err("invalid checkpoint payload"))?;
        let Some(entries) = data.get("dedup").and_then(serde_json::Value::as_array) else {
            return Ok(Vec::new());
        };
        entries
            .iter()
            .map(|entry| {
                let command_id = entry
                    .get("command_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| map_err("invalid checkpoint dedup"))?;
                let created_at_millis = entry
                    .get("created_at_millis")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| map_err("invalid checkpoint dedup"))?;
                let expires_at_millis = entry
                    .get("expires_at_millis")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| map_err("invalid checkpoint dedup"))?;
                Ok(orbisync_application::CheckpointSaveReceipt {
                    command_id: command_id.to_owned(),
                    created_at_millis,
                    expires_at_millis,
                })
            })
            .collect()
    }

    async fn load_latest(
        &self,
        _instance_id: orbisync_domain::InstanceId,
    ) -> Result<Option<orbisync_application::AppCheckpoint>, orbisync_application::ApplicationError>
    {
        Ok(None)
    }
}
