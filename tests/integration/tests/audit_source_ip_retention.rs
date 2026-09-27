//! A-7 regression coverage for source-IP-only retention.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use orbisync_storage_postgres::PgAuditSourceIpRetentionStore;
use time::OffsetDateTime;
use uuid::Uuid;

mod common;

#[tokio::test]
async fn expired_source_ip_is_deleted_without_deleting_audit_event() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };

    let event_id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let old = now - time::Duration::days(2);
    sqlx::query(
        "INSERT INTO audit_events (id, occurred_at, action, result) VALUES ($1, $2, 'retention.test', 'success')",
    )
    .bind(event_id)
    .bind(old)
    .execute(&pool)
    .await
    .expect("audit event is inserted");
    sqlx::query(
        "INSERT INTO audit_source_ips (audit_event_id, source_ip, created_at) VALUES ($1, '192.0.2.10', $2)",
    )
    .bind(event_id)
    .bind(old)
    .execute(&pool)
    .await
    .expect("source IP is inserted");

    let store = PgAuditSourceIpRetentionStore::new(pool.clone());
    let deleted = store
        .delete_expired(now - time::Duration::days(1), 10)
        .await
        .expect("source IP retention succeeds");
    assert_eq!(deleted, 1);

    let source_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_source_ips WHERE audit_event_id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .expect("source IP count is readable");
    assert_eq!(source_count, 0);

    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .expect("audit event count is readable");
    assert_eq!(audit_count, 1);

    sqlx::query("DELETE FROM audit_events WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("test audit event is cleaned up");
}
