//! LOW-002 coverage for operator-only audit retention.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::pool_or_skip;
use futures_util::future::join;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

async fn retention_status(pool: &sqlx::PgPool, archive_id: Uuid) -> (i64, Option<OffsetDateTime>) {
    sqlx::query_as(
        "SELECT remaining_count, remaining_oldest FROM audit_operations.retention_status($1)",
    )
    .bind(archive_id)
    .fetch_one(pool)
    .await
    .expect("retention status is available")
}

#[tokio::test]
async fn audit_retention_is_bounded_at_boundary_and_idempotent() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // The epoch boundary keeps this test's rows away from normal application
    // data. Only rows created below are touched; this is not a production data
    // deletion test.
    let cutoff = OffsetDateTime::UNIX_EPOCH;
    let ids: Vec<Uuid> = (0..11).map(|_| Uuid::now_v7()).collect();
    let mut all_ids = ids.clone();
    let occurred_at = [
        cutoff - Duration::seconds(3),
        cutoff - Duration::seconds(2),
        cutoff - Duration::seconds(1),
        cutoff,
        OffsetDateTime::now_utc(),
    ];
    for (id, timestamp) in ids.iter().take(5).zip(occurred_at) {
        sqlx::query(
            "INSERT INTO audit_events (id, occurred_at, action, result) VALUES ($1, $2, 'low002.test', 'success')",
        )
        .bind(id)
        .bind(timestamp)
        .execute(&pool)
        .await
        .expect("test audit event is inserted");
    }

    let archive_id = Uuid::now_v7();
    let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut connection = pool.acquire().await.expect("connection acquired");

    // A maintenance login cannot mutate the table directly, and the runtime
    // login cannot invoke the maintenance API.
    sqlx::query("SET ROLE orbisync_audit_maintenance")
        .execute(&mut *connection)
        .await
        .expect("maintenance role selected");
    assert!(
        sqlx::query("DELETE FROM audit_events WHERE id = $1")
            .bind(ids[0])
            .execute(&mut *connection)
            .await
            .is_err(),
        "maintenance role must not receive direct DELETE"
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT audit_operations.purge_audit_events($1, 0)")
            .bind(archive_id)
            .fetch_one(&mut *connection)
            .await
            .is_err(),
        "invalid batch size must be rejected"
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT audit_operations.purge_audit_events($1, 2)",)
            .bind(Uuid::now_v7())
            .fetch_one(&mut *connection)
            .await
            .is_err(),
        "purge must fail closed without a verified archive"
    );
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset");

    sqlx::query("SET ROLE orbisync_runtime")
        .execute(&mut *connection)
        .await
        .expect("runtime role selected");
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT audit_operations.purge_audit_events($1, 2)",)
            .bind(archive_id)
            .fetch_one(&mut *connection)
            .await
            .is_err(),
        "runtime role must not execute retention"
    );
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset");

    sqlx::query("SET ROLE orbisync_audit_maintenance")
        .execute(&mut *connection)
        .await
        .expect("maintenance role selected");
    let validated: i32 = sqlx::query_scalar("SELECT audit_operations.validate_retention_days($1)")
        .bind(365)
        .fetch_one(&mut *connection)
        .await
        .expect("valid retention days are accepted");
    assert_eq!(validated, 365);
    assert!(
        sqlx::query_scalar::<_, i32>("SELECT audit_operations.validate_retention_days($1)",)
            .bind(3651)
            .fetch_one(&mut *connection)
            .await
            .is_err(),
        "retention days above the policy bound must be rejected"
    );
    let archived: i64 =
        sqlx::query_scalar("SELECT audit_operations.record_verified_archive($1, $2, $3, 3, $4)")
            .bind(archive_id)
            .bind(cutoff)
            .bind(365)
            .bind(sha256)
            .fetch_one(&mut *connection)
            .await
            .expect("archive verification is recorded");
    assert_eq!(archived, 3);
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset");
    drop(connection);

    assert_eq!(retention_status(&pool, archive_id).await.0, 3);

    // Three calls are needed for three eligible rows; no call can delete more
    // than its requested batch.
    let mut connection = pool.acquire().await.expect("connection acquired");
    sqlx::query("SET ROLE orbisync_audit_maintenance")
        .execute(&mut *connection)
        .await
        .expect("maintenance role selected");
    for expected in [2_i64, 1, 0] {
        let deleted: i64 = sqlx::query_scalar("SELECT audit_operations.purge_audit_events($1, 2)")
            .bind(archive_id)
            .fetch_one(&mut *connection)
            .await
            .expect("bounded purge succeeds");
        assert_eq!(deleted, expected);
    }
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset");
    drop(connection);
    assert_eq!(retention_status(&pool, archive_id).await, (0, None));

    // Seed another archive and make the DELETE statement fail after it has
    // changed rows. PostgreSQL must roll back both the deletes and marker
    // update, not leave a partially purged batch.
    let rollback_ids = [Uuid::now_v7(), Uuid::now_v7()];
    all_ids.extend(rollback_ids);
    for id in rollback_ids {
        sqlx::query(
            "INSERT INTO audit_events (id, occurred_at, action, result) VALUES ($1, $2, 'low002.rollback', 'success')",
        )
        .bind(id)
        .bind(cutoff - Duration::seconds(10))
        .execute(&pool)
        .await
        .expect("rollback row is inserted");
    }
    let rollback_archive = Uuid::now_v7();
    sqlx::query(
        "CREATE OR REPLACE FUNCTION public.low002_fail_delete() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER AS $$ BEGIN RAISE EXCEPTION 'LOW-002 rollback test'; END $$",
    )
    .execute(&pool)
    .await
    .expect("rollback trigger function is created");
    sqlx::query(
        "CREATE TRIGGER low002_fail_delete_trigger AFTER DELETE ON audit_events FOR EACH STATEMENT EXECUTE FUNCTION public.low002_fail_delete()",
    )
    .execute(&pool)
    .await
    .expect("rollback trigger is created");

    let mut connection = pool.acquire().await.expect("connection acquired");
    sqlx::query("SET ROLE orbisync_audit_maintenance")
        .execute(&mut *connection)
        .await
        .expect("maintenance role selected");
    sqlx::query_scalar::<_, i64>(
        "SELECT audit_operations.record_verified_archive($1, $2, $3, 2, $4)",
    )
    .bind(rollback_archive)
    .bind(cutoff)
    .bind(365)
    .bind(sha256)
    .fetch_one(&mut *connection)
    .await
    .expect("rollback archive is verified");
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT audit_operations.purge_audit_events($1, 2)",)
            .bind(rollback_archive)
            .fetch_one(&mut *connection)
            .await
            .is_err(),
        "delete trigger must fail the transaction"
    );
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset after rollback");
    drop(connection);
    let rollback_remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE id = ANY($1)")
            .bind(&rollback_ids[..])
            .fetch_one(&pool)
            .await
            .expect("rollback rows are countable");
    assert_eq!(rollback_remaining, 2, "failed purge must roll back deletes");
    sqlx::query("DROP TRIGGER low002_fail_delete_trigger ON audit_events")
        .execute(&pool)
        .await
        .expect("rollback trigger is removed");
    sqlx::query("DROP FUNCTION public.low002_fail_delete()")
        .execute(&pool)
        .await
        .expect("rollback trigger function is removed");

    // Two workers may race, but the transaction advisory lock serializes the
    // job and each worker still gets only one bounded batch. Use a narrower
    // cutoff so the deliberately retained rollback rows are not in this
    // archive's manifest.
    let concurrent_cutoff = cutoff - Duration::seconds(15);
    let concurrent_archive = Uuid::now_v7();
    let concurrent_ids: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
    all_ids.extend(concurrent_ids.iter().copied());
    for id in &concurrent_ids {
        sqlx::query(
            "INSERT INTO audit_events (id, occurred_at, action, result) VALUES ($1, $2, 'low002.concurrent', 'success')",
        )
        .bind(id)
        .bind(cutoff - Duration::seconds(20))
        .execute(&pool)
        .await
        .expect("concurrent row is inserted");
    }
    let mut connection = pool.acquire().await.expect("connection acquired");
    sqlx::query("SET ROLE orbisync_audit_maintenance")
        .execute(&mut *connection)
        .await
        .expect("maintenance role selected");
    sqlx::query_scalar::<_, i64>(
        "SELECT audit_operations.record_verified_archive($1, $2, $3, 4, $4)",
    )
    .bind(concurrent_archive)
    .bind(concurrent_cutoff)
    .bind(365)
    .bind(sha256)
    .fetch_one(&mut *connection)
    .await
    .expect("concurrent archive is verified");
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset");
    drop(connection);

    async fn purge_as_operator(pool: &sqlx::PgPool, archive_id: Uuid) -> Result<i64, sqlx::Error> {
        let mut connection = pool.acquire().await?;
        sqlx::query("SET ROLE orbisync_audit_maintenance")
            .execute(&mut *connection)
            .await?;
        let result = sqlx::query_scalar("SELECT audit_operations.purge_audit_events($1, 1)")
            .bind(archive_id)
            .fetch_one(&mut *connection)
            .await;
        sqlx::query("RESET ROLE").execute(&mut *connection).await?;
        result
    }

    let first = purge_as_operator(&pool, concurrent_archive);
    let second = purge_as_operator(&pool, concurrent_archive);
    let (first, second) = join(first, second).await;
    assert_eq!(first.expect("first concurrent purge"), 1);
    assert_eq!(second.expect("second concurrent purge"), 1);
    assert_eq!(retention_status(&pool, concurrent_archive).await.0, 2);

    // Cleanup is limited to this test's IDs and runs as the test database
    // owner, not through either application role.
    sqlx::query("DELETE FROM audit_events WHERE id = ANY($1)")
        .bind(&all_ids[..])
        .execute(&pool)
        .await
        .expect("test rows are cleaned up");
    sqlx::query("DELETE FROM audit_operations.retention_archives WHERE archive_id = ANY($1)")
        .bind(&[archive_id, rollback_archive, concurrent_archive][..])
        .execute(&pool)
        .await
        .expect("test archive markers are cleaned up");
}
