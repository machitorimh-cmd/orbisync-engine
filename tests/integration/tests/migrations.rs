//! Migration integration test.
//!
//! Applies every migration to a real PostgreSQL database (`test-and-ci.md`
//! §2.9). CI runs it against PostgreSQL 16 and 17 (ADR-005). The test is
//! skipped when `DATABASE_URL` is unset, so `cargo test` stays runnable without
//! a database.
//!
//! Milestone 0 ships no schema migration yet; the test proves that the runner,
//! the embedded migrator and the CI wiring work, and it will cover the
//! Milestone 1 schema unchanged.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use orbisync_storage_postgres::{MIGRATOR, create_pool, run_migrations};
use uuid::Uuid;

mod common;

fn database_url() -> Option<String> {
    common::url_or_skip()
}

#[tokio::test]
async fn test_migrations_apply_to_an_empty_database() {
    let Some(url) = database_url() else {
        return;
    };

    let db_config = common::test_database_config();
    let pool = create_pool(&db_config, &url).expect("pool is created");
    run_migrations(&pool).await.expect("migrations apply");

    // Re-running must be a no-op: `sqlx migrate` is idempotent and verifies the
    // checksum of every applied migration.
    run_migrations(&pool)
        .await
        .expect("migrations are idempotent");

    let applied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success = true")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
    assert_eq!(
        applied,
        i64::try_from(MIGRATOR.iter().count()).expect("migration count fits in i64")
    );
}

#[tokio::test]
async fn test_readiness_probe_answers_against_a_real_database() {
    let Some(url) = database_url() else {
        return;
    };

    use orbisync_application::HealthProbe as _;
    let db_config = common::test_database_config();
    let pool = create_pool(&db_config, &url).expect("pool is created");
    let probe = orbisync_storage_postgres::PgHealthProbe::new(pool);
    probe.check().await.expect("database answers");
}

#[tokio::test]
async fn test_audit_source_ip_split_preserves_data_and_privileges() {
    let Some(url) = database_url() else {
        return;
    };

    let db_config = common::test_database_config();
    let pool = create_pool(&db_config, &url).expect("pool is created");
    let schema = format!("audit_source_ip_{}", Uuid::now_v7().simple());
    let legacy = MIGRATOR
        .iter()
        .find(|migration| migration.version == 3)
        .expect("legacy audit migration exists");
    let split = MIGRATOR
        .iter()
        .find(|migration| migration.version == 13)
        .expect("source-IP split migration exists");

    let mut tx = pool.begin().await.expect("transaction starts");
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&mut *tx)
        .await
        .expect("test schema is created");
    sqlx::query(&format!("SET LOCAL search_path TO \"{schema}\", public"))
        .execute(&mut *tx)
        .await
        .expect("test schema is selected");
    sqlx::raw_sql(&legacy.sql)
        .execute(&mut *tx)
        .await
        .expect("legacy audit migration applies");
    sqlx::query(&format!(
        "GRANT USAGE ON SCHEMA \"{schema}\" TO orbisync_runtime"
    ))
    .execute(&mut *tx)
    .await
    .expect("runtime can use the test schema");

    let legacy_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO audit_events (id, occurred_at, action, source_ip, result) VALUES ($1, now(), 'migration.test', '192.0.2.1', 'success')",
    )
    .bind(legacy_id)
    .execute(&mut *tx)
    .await
    .expect("legacy source IP is inserted");
    sqlx::raw_sql(&split.sql)
        .execute(&mut *tx)
        .await
        .expect("source-IP split migration applies");

    let moved_ip: Option<String> = sqlx::query_scalar(&format!(
        "SELECT source_ip::text FROM \"{schema}\".audit_source_ips WHERE audit_event_id = $1"
    ))
    .bind(legacy_id)
    .fetch_optional(&mut *tx)
    .await
    .expect("migrated source IP is readable");
    assert_eq!(moved_ip.as_deref(), Some("192.0.2.1/32"));

    let source_column: Option<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_schema = $1 AND table_name = 'audit_events' AND column_name = 'source_ip'",
    )
    .bind(&schema)
    .fetch_optional(&mut *tx)
    .await
    .expect("audit column metadata is readable");
    assert!(source_column.is_none(), "source_ip must leave audit_events");

    let table_name = format!("{schema}.audit_source_ips");
    let source_select: bool =
        sqlx::query_scalar("SELECT has_table_privilege('orbisync_runtime', $1, 'SELECT')")
            .bind(&table_name)
            .fetch_one(&mut *tx)
            .await
            .expect("source SELECT privilege is inspectable");
    let source_insert: bool =
        sqlx::query_scalar("SELECT has_table_privilege('orbisync_runtime', $1, 'INSERT')")
            .bind(&table_name)
            .fetch_one(&mut *tx)
            .await
            .expect("source INSERT privilege is inspectable");
    let source_delete: bool =
        sqlx::query_scalar("SELECT has_table_privilege('orbisync_runtime', $1, 'DELETE')")
            .bind(&table_name)
            .fetch_one(&mut *tx)
            .await
            .expect("source DELETE privilege is inspectable");
    assert!(source_select && source_insert && source_delete);

    let audit_table = format!("{schema}.audit_events");
    let audit_update: bool =
        sqlx::query_scalar("SELECT has_table_privilege('orbisync_runtime', $1, 'UPDATE')")
            .bind(&audit_table)
            .fetch_one(&mut *tx)
            .await
            .expect("audit UPDATE privilege is inspectable");
    let audit_delete: bool =
        sqlx::query_scalar("SELECT has_table_privilege('orbisync_runtime', $1, 'DELETE')")
            .bind(&audit_table)
            .fetch_one(&mut *tx)
            .await
            .expect("audit DELETE privilege is inspectable");
    assert!(!audit_update && !audit_delete);

    // Exercise the privileges granted to the runtime role without leaving
    // test rows behind: the enclosing transaction is rolled back below.
    let runtime_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO audit_events (id, occurred_at, action, result) VALUES ($1, now(), 'runtime.test', 'success')",
    )
    .bind(runtime_id)
    .execute(&mut *tx)
    .await
    .expect("owner can seed runtime privilege test row");
    sqlx::query("SET LOCAL ROLE orbisync_runtime")
        .execute(&mut *tx)
        .await
        .expect("runtime role is selectable");
    sqlx::query(&format!(
        "INSERT INTO \"{schema}\".audit_source_ips (audit_event_id, source_ip, created_at) VALUES ($1, '198.51.100.7', now())"
    ))
    .bind(runtime_id)
    .execute(&mut *tx)
    .await
    .expect("runtime can insert source IP");
    let runtime_ip: String = sqlx::query_scalar(&format!(
        "SELECT source_ip::text FROM \"{schema}\".audit_source_ips WHERE audit_event_id = $1"
    ))
    .bind(runtime_id)
    .fetch_one(&mut *tx)
    .await
    .expect("runtime can select source IP");
    assert_eq!(runtime_ip, "198.51.100.7/32");
    sqlx::query(&format!(
        "DELETE FROM \"{schema}\".audit_source_ips WHERE audit_event_id = $1"
    ))
    .bind(runtime_id)
    .execute(&mut *tx)
    .await
    .expect("runtime can delete source IP");

    tx.rollback().await.expect("test transaction rolls back");
}
