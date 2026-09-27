//! PostgreSQL coverage for WH1 extension registration and outbox persistence.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::OnceLock;

use orbisync_application::{
    ApplicationErrorKind, ExtensionDeliveryStore, ExtensionEvent, ExtensionOutboxStore,
    ExtensionRegistration, ExtensionRegistrationStore, ExtensionStatus, RequestId,
    StartInstanceCommand, StopInstanceCommand, WorldAuditEvent, WorldDirectoryStore,
    WorldDirectoryUseCase,
};
use orbisync_domain::{
    InstanceId, Timestamp, UserId, World, WorldId, WorldInstance, transform::Transform,
};
use orbisync_storage_postgres::{
    PgExtensionOutboxStore, PgExtensionRegistrationStore, PgWorldDirectoryStore,
};
use sqlx::PgPool;
use tokio::sync::{Barrier, Mutex};
use uuid::Uuid;

mod common;

static EXTENSION_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

async fn extension_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    EXTENSION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await
}

async fn database() -> Option<PgPool> {
    common::pool_or_skip().await
}

fn registration(extension_id: Uuid) -> ExtensionRegistration {
    ExtensionRegistration {
        extension_id,
        name: "Audit extension".to_owned(),
        description: Some("Receives lifecycle facts".to_owned()),
        endpoint: "https://extension.example.test/events".to_owned(),
        subscribed_events: ["user.created".to_owned(), "instance.started".to_owned()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        capabilities: ["read".to_owned()].into_iter().collect(),
        token_scopes: ["users:read".to_owned()].into_iter().collect(),
        status: ExtensionStatus::Active,
        signing_secret_ref: "ORBISYNC_EXTENSION_SIGNING_KEY".to_owned(),
    }
}

fn lease_registration(extension_id: Uuid, event_kind: &str) -> ExtensionRegistration {
    let mut value = registration(extension_id);
    value.name = format!("Lease test {extension_id}");
    value.subscribed_events = [event_kind.to_owned()].into_iter().collect();
    value
}

async fn seed_lease_registration(pool: &PgPool, extension_id: Uuid, event_kind: &str) {
    PgExtensionRegistrationStore::new(pool.clone())
        .save_registration(lease_registration(extension_id, event_kind))
        .await
        .expect("lease test registration saves");
}

async fn insert_lease_event(
    pool: &PgPool,
    event_id: Uuid,
    event_kind: &str,
    available_at: time::OffsetDateTime,
) {
    let created_at = time::OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO outbox_events
         (id, event_id, owner_module, event_type, event_kind, payload,
          created_at, available_at)
         VALUES ($1, $1, 'extensions', $2, $2, '{}'::jsonb, $3, $4)",
    )
    .bind(event_id)
    .bind(event_kind)
    .bind(created_at)
    .bind(available_at)
    .execute(pool)
    .await
    .expect("lease test event inserts");
}

async fn insert_lease_delivery(
    pool: &PgPool,
    delivery_id: Uuid,
    event_id: Uuid,
    extension_id: Uuid,
    available_at: time::OffsetDateTime,
    lease: Option<(Uuid, Uuid, time::OffsetDateTime)>,
    delivered_at: Option<time::OffsetDateTime>,
    dead_lettered_at: Option<time::OffsetDateTime>,
) {
    let (lease_owner, lease_token, lease_expires_at) = lease
        .map_or((None, None, None), |(owner, token, expires_at)| {
            (Some(owner), Some(token), Some(expires_at))
        });
    sqlx::query(
        "INSERT INTO extension_deliveries
         (delivery_id, event_id, extension_id, available_at,
          delivered_at, dead_lettered_at, lease_owner, lease_token, lease_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(delivery_id)
    .bind(event_id)
    .bind(extension_id)
    .bind(available_at)
    .bind(delivered_at)
    .bind(dead_lettered_at)
    .bind(lease_owner)
    .bind(lease_token)
    .bind(lease_expires_at)
    .execute(pool)
    .await
    .expect("lease test delivery inserts");
}

async fn cleanup_lease_fixture(pool: &PgPool, event_ids: &[Uuid], extension_ids: &[Uuid]) {
    if !event_ids.is_empty() {
        sqlx::query("DELETE FROM extension_dead_letters WHERE event_id = ANY($1)")
            .bind(event_ids)
            .execute(pool)
            .await
            .expect("lease test dead letters clean up");
        sqlx::query("DELETE FROM extension_deliveries WHERE event_id = ANY($1)")
            .bind(event_ids)
            .execute(pool)
            .await
            .expect("lease test deliveries clean up");
        sqlx::query("DELETE FROM outbox_events WHERE event_id = ANY($1)")
            .bind(event_ids)
            .execute(pool)
            .await
            .expect("lease test events clean up");
    }
    if !extension_ids.is_empty() {
        sqlx::query("DELETE FROM extension_registrations WHERE extension_id = ANY($1)")
            .bind(extension_ids)
            .execute(pool)
            .await
            .expect("lease test registrations clean up");
    }
}

async fn parent_delivered_at(pool: &PgPool, event_id: Uuid) -> Option<time::OffsetDateTime> {
    sqlx::query_scalar("SELECT delivered_at FROM outbox_events WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .expect("lease test parent query")
}

#[tokio::test]
async fn expired_lease_is_reclaimed_and_stale_finalize_is_rejected() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_kind = "entity.deleted";
    let extension_id = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let delivery_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    seed_lease_registration(&pool, extension_id, event_kind).await;
    insert_lease_event(
        &pool,
        event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;
    insert_lease_delivery(
        &pool,
        delivery_id,
        event_id,
        extension_id,
        now - time::Duration::seconds(1),
        None,
        None,
        None,
    )
    .await;

    let store = PgExtensionOutboxStore::new(pool.clone());
    let owner_a = Uuid::now_v7();
    let first = store
        .claim_due(now, owner_a, now + time::Duration::hours(1), 10)
        .await
        .expect("first lease claim");
    let first = first
        .into_iter()
        .find(|delivery| delivery.delivery_id == delivery_id)
        .expect("first worker claims the test delivery");

    sqlx::query(
        "UPDATE extension_deliveries
            SET lease_expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second'
          WHERE delivery_id = $1",
    )
    .bind(delivery_id)
    .execute(&pool)
    .await
    .expect("lease expiry is forced into the past");

    let owner_b = Uuid::now_v7();
    let second = store
        .claim_due(
            time::OffsetDateTime::now_utc(),
            owner_b,
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            10,
        )
        .await
        .expect("expired lease is reclaimed");
    let second = second
        .into_iter()
        .find(|delivery| delivery.delivery_id == delivery_id)
        .expect("second worker claims the expired delivery");
    assert_eq!(second.lease_owner, owner_b);
    assert_ne!(second.lease_token, first.lease_token);

    let stale = store
        .mark_delivered(
            delivery_id,
            first.lease_owner,
            first.lease_token,
            first.lease_expires_at,
        )
        .await
        .expect_err("old worker must be fenced after expiry reclaim");
    assert_eq!(stale.kind(), ApplicationErrorKind::Conflict);
    store
        .mark_delivered(
            delivery_id,
            second.lease_owner,
            second.lease_token,
            second.lease_expires_at,
        )
        .await
        .expect("current worker finalizes the delivery");
    assert!(parent_delivered_at(&pool, event_id).await.is_some());
    cleanup_lease_fixture(&pool, &[event_id], &[extension_id]).await;
}

#[tokio::test]
async fn concurrent_workers_claim_one_delivery() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_kind = "entity.spawned";
    let extension_id = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let delivery_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    seed_lease_registration(&pool, extension_id, event_kind).await;
    insert_lease_event(
        &pool,
        event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;
    insert_lease_delivery(
        &pool,
        delivery_id,
        event_id,
        extension_id,
        now - time::Duration::seconds(1),
        None,
        None,
        None,
    )
    .await;

    let barrier = std::sync::Arc::new(Barrier::new(2));
    let left_barrier = std::sync::Arc::clone(&barrier);
    let right_barrier = std::sync::Arc::clone(&barrier);
    let left_store = PgExtensionOutboxStore::new(pool.clone());
    let right_store = PgExtensionOutboxStore::new(pool.clone());
    let left_owner = Uuid::now_v7();
    let right_owner = Uuid::now_v7();
    let left = async move {
        left_barrier.wait().await;
        left_store
            .claim_due(now, left_owner, now + time::Duration::hours(1), 10)
            .await
            .expect("left worker claim")
    };
    let right = async move {
        right_barrier.wait().await;
        right_store
            .claim_due(now, right_owner, now + time::Duration::hours(1), 10)
            .await
            .expect("right worker claim")
    };
    let (left, right) = tokio::join!(left, right);
    let claimed_ids: Vec<Uuid> = left
        .into_iter()
        .chain(right)
        .map(|delivery| delivery.delivery_id)
        .filter(|id| *id == delivery_id)
        .collect();
    assert_eq!(claimed_ids, vec![delivery_id]);
    cleanup_lease_fixture(&pool, &[event_id], &[extension_id]).await;
}

#[tokio::test]
async fn dead_letter_finalization_waits_for_siblings_and_materializes_late_registration() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let store = PgExtensionOutboxStore::new(pool.clone());
    let now = time::OffsetDateTime::now_utc();
    let event_kind = "entity.updated";
    let extension_a = Uuid::now_v7();
    let extension_b = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let delivery_a = Uuid::now_v7();
    let delivery_b = Uuid::now_v7();
    seed_lease_registration(&pool, extension_a, event_kind).await;
    seed_lease_registration(&pool, extension_b, event_kind).await;
    insert_lease_event(
        &pool,
        event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;
    let owner_a = Uuid::now_v7();
    let token_a = Uuid::now_v7();
    let expires_a = now + time::Duration::hours(1);
    let owner_b = Uuid::now_v7();
    let token_b = Uuid::now_v7();
    let expires_b = now + time::Duration::hours(1);
    insert_lease_delivery(
        &pool,
        delivery_a,
        event_id,
        extension_a,
        now - time::Duration::seconds(1),
        Some((owner_a, token_a, expires_a)),
        None,
        None,
    )
    .await;
    insert_lease_delivery(
        &pool,
        delivery_b,
        event_id,
        extension_b,
        now - time::Duration::seconds(1),
        Some((owner_b, token_b, expires_b)),
        None,
        None,
    )
    .await;
    store
        .move_to_dead_letter(
            delivery_a,
            owner_a,
            token_a,
            expires_a,
            1,
            "terminal-a",
            now + time::Duration::days(1),
        )
        .await
        .expect("first sibling moves to DLQ");
    assert!(parent_delivered_at(&pool, event_id).await.is_none());
    store
        .mark_delivered(delivery_b, owner_b, token_b, expires_b)
        .await
        .expect("pending sibling completes");
    assert!(parent_delivered_at(&pool, event_id).await.is_some());

    let extension_c = Uuid::now_v7();
    let extension_d = Uuid::now_v7();
    let late_event_id = Uuid::now_v7();
    let late_delivery_c = Uuid::now_v7();
    let late_event_kind = "entity.deleted";
    seed_lease_registration(&pool, extension_c, late_event_kind).await;
    seed_lease_registration(&pool, extension_d, late_event_kind).await;
    insert_lease_event(
        &pool,
        late_event_id,
        late_event_kind,
        now - time::Duration::seconds(1),
    )
    .await;
    let owner_c = Uuid::now_v7();
    let token_c = Uuid::now_v7();
    let expires_c = now + time::Duration::hours(1);
    insert_lease_delivery(
        &pool,
        late_delivery_c,
        late_event_id,
        extension_c,
        now - time::Duration::seconds(1),
        Some((owner_c, token_c, expires_c)),
        None,
        None,
    )
    .await;
    store
        .move_to_dead_letter(
            late_delivery_c,
            owner_c,
            token_c,
            expires_c,
            1,
            "terminal-c",
            now + time::Duration::days(1),
        )
        .await
        .expect("delivery C moves to DLQ");
    assert!(parent_delivered_at(&pool, late_event_id).await.is_none());

    let claimed = store
        .claim_due(
            time::OffsetDateTime::now_utc(),
            Uuid::now_v7(),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            10,
        )
        .await
        .expect("late eligible registration materializes");
    let late_delivery_d = claimed
        .into_iter()
        .find(|delivery| {
            delivery.event_id == late_event_id && delivery.registration.extension_id == extension_d
        })
        .expect("late registration child is claimed");
    store
        .move_to_dead_letter(
            late_delivery_d.delivery_id,
            late_delivery_d.lease_owner,
            late_delivery_d.lease_token,
            late_delivery_d.lease_expires_at,
            1,
            "terminal-d",
            now + time::Duration::days(1),
        )
        .await
        .expect("late child moves to DLQ");
    assert!(parent_delivered_at(&pool, late_event_id).await.is_some());
    let duplicate = store
        .move_to_dead_letter(
            late_delivery_c,
            owner_c,
            token_c,
            expires_c,
            1,
            "duplicate",
            now + time::Duration::days(1),
        )
        .await
        .expect_err("same lease cannot create a duplicate DLQ row");
    assert_eq!(duplicate.kind(), ApplicationErrorKind::Conflict);
    let dlq_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM extension_dead_letters WHERE delivery_id = $1")
            .bind(late_delivery_c)
            .fetch_one(&pool)
            .await
            .expect("DLQ count query");
    assert_eq!(dlq_count, 1);
    cleanup_lease_fixture(
        &pool,
        &[event_id, late_event_id],
        &[extension_a, extension_b, extension_c, extension_d],
    )
    .await;
}

#[tokio::test]
async fn concurrent_materialize_and_reconcile_leave_a_pending_parent() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_kind = "member.joined";
    let extension_id = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    seed_lease_registration(&pool, extension_id, event_kind).await;
    insert_lease_event(
        &pool,
        event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;

    let barrier = std::sync::Arc::new(Barrier::new(2));
    let claim_barrier = std::sync::Arc::clone(&barrier);
    let reconcile_barrier = std::sync::Arc::clone(&barrier);
    let claim_store = PgExtensionOutboxStore::new(pool.clone());
    let reconcile_store = PgExtensionOutboxStore::new(pool.clone());
    let claim = async move {
        claim_barrier.wait().await;
        claim_store
            .claim_due(now, Uuid::now_v7(), now + time::Duration::hours(1), 10)
            .await
            .expect("concurrent materialization claim")
    };
    let reconcile = async move {
        reconcile_barrier.wait().await;
        reconcile_store
            .reconcile_terminal_states(10)
            .await
            .expect("concurrent reconciliation")
    };
    let (claimed, _) = tokio::join!(claim, reconcile);
    if claimed.iter().all(|delivery| delivery.event_id != event_id) {
        let retry = PgExtensionOutboxStore::new(pool.clone())
            .claim_due(
                time::OffsetDateTime::now_utc(),
                Uuid::now_v7(),
                time::OffsetDateTime::now_utc() + time::Duration::hours(1),
                10,
            )
            .await
            .expect("materialization retry after SKIP LOCKED");
        assert!(retry.iter().any(|delivery| delivery.event_id == event_id));
    }
    let child_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM extension_deliveries WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .expect("materialized child count query");
    assert_eq!(child_count, 1);
    assert!(parent_delivered_at(&pool, event_id).await.is_none());
    cleanup_lease_fixture(&pool, &[event_id], &[extension_id]).await;
}

#[tokio::test]
async fn terminal_transition_rolls_back_when_parent_update_fails() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_kind = "member.left";
    let extension_id = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let delivery_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    seed_lease_registration(&pool, extension_id, event_kind).await;
    insert_lease_event(
        &pool,
        event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let expires_at =
        now.replace_nanosecond(0).expect("timestamp precision") + time::Duration::hours(1);
    insert_lease_delivery(
        &pool,
        delivery_id,
        event_id,
        extension_id,
        now - time::Duration::seconds(1),
        Some((owner, token, expires_at)),
        None,
        None,
    )
    .await;

    let suffix = Uuid::now_v7().simple().to_string();
    let function_name = format!("lease_test_fail_{suffix}");
    let trigger_name = format!("lease_test_fail_trigger_{suffix}");
    let create_function = format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $$\nBEGIN\n  IF OLD.event_id = '{event_id}'::uuid AND NEW.delivered_at IS DISTINCT FROM OLD.delivered_at THEN\n    RAISE EXCEPTION 'lease test parent update failure';\n  END IF;\n  RETURN NEW;\nEND\n$$"
    );
    sqlx::query(&create_function)
        .execute(&pool)
        .await
        .expect("failure trigger function creates");
    let create_trigger = format!(
        "CREATE TRIGGER {trigger_name} BEFORE UPDATE OF delivered_at ON outbox_events FOR EACH ROW EXECUTE FUNCTION {function_name}()"
    );
    sqlx::query(&create_trigger)
        .execute(&pool)
        .await
        .expect("failure trigger creates");

    let result = PgExtensionOutboxStore::new(pool.clone())
        .mark_delivered(delivery_id, owner, token, expires_at)
        .await
        .expect_err("parent update failure is returned");
    assert_eq!(result.kind(), ApplicationErrorKind::PortFailure);
    let (delivered_at, retained_owner, retained_token, retained_expiry): (
        Option<time::OffsetDateTime>,
        Option<Uuid>,
        Option<Uuid>,
        Option<time::OffsetDateTime>,
    ) = sqlx::query_as(
        "SELECT delivered_at, lease_owner, lease_token, lease_expires_at
           FROM extension_deliveries WHERE delivery_id = $1",
    )
    .bind(delivery_id)
    .fetch_one(&pool)
    .await
    .expect("rolled back delivery query");
    assert!(delivered_at.is_none());
    assert_eq!(retained_owner, Some(owner));
    assert_eq!(retained_token, Some(token));
    assert_eq!(retained_expiry, Some(expires_at));
    assert!(parent_delivered_at(&pool, event_id).await.is_none());
    sqlx::query(&format!("DROP TRIGGER {trigger_name} ON outbox_events"))
        .execute(&pool)
        .await
        .expect("failure trigger drops");
    sqlx::query(&format!("DROP FUNCTION {function_name}()"))
        .execute(&pool)
        .await
        .expect("failure trigger function drops");
    cleanup_lease_fixture(&pool, &[event_id], &[extension_id]).await;
}

#[tokio::test]
async fn not_valid_lease_constraints_allow_legacy_rows_but_reject_new_invalid_rows() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_kind = "entity.ownership_transferred";
    let extension_id = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let second_event_id = Uuid::now_v7();
    let legacy_delivery_id = Uuid::now_v7();
    let new_delivery_id = Uuid::now_v7();
    let orphan_delivery_id = Uuid::now_v7();
    let new_orphan_delivery_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    seed_lease_registration(&pool, extension_id, event_kind).await;
    insert_lease_event(
        &pool,
        event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;
    insert_lease_event(
        &pool,
        second_event_id,
        event_kind,
        now - time::Duration::seconds(1),
    )
    .await;

    sqlx::query("ALTER TABLE extension_deliveries DROP CONSTRAINT extension_deliveries_terminal_state_check")
        .execute(&pool)
        .await
        .expect("terminal check drops for legacy seed");
    insert_lease_delivery(
        &pool,
        legacy_delivery_id,
        event_id,
        extension_id,
        now,
        None,
        Some(now),
        Some(now),
    )
    .await;
    sqlx::query(
        "ALTER TABLE extension_deliveries
           ADD CONSTRAINT extension_deliveries_terminal_state_check
           CHECK (NOT (delivered_at IS NOT NULL AND dead_lettered_at IS NOT NULL))
           NOT VALID",
    )
    .execute(&pool)
    .await
    .expect("terminal check re-adds as not valid");
    let (validated,): (bool,) = sqlx::query_as(
        "SELECT convalidated FROM pg_constraint
          WHERE conname = 'extension_deliveries_terminal_state_check'",
    )
    .fetch_one(&pool)
    .await
    .expect("terminal check validation state");
    assert!(!validated);
    let rejected_check = sqlx::query(
        "INSERT INTO extension_deliveries
         (delivery_id, event_id, extension_id, available_at, delivered_at, dead_lettered_at)
         VALUES ($1, $2, $3, $4, $4, $4)",
    )
    .bind(new_delivery_id)
    .bind(second_event_id)
    .bind(extension_id)
    .bind(now)
    .execute(&pool)
    .await;
    assert!(
        rejected_check.is_err(),
        "new invalid terminal row is rejected"
    );

    sqlx::query(
        "ALTER TABLE extension_dead_letters DROP CONSTRAINT extension_dead_letters_delivery_fk",
    )
    .execute(&pool)
    .await
    .expect("delivery FK drops for legacy seed");
    sqlx::query(
        "INSERT INTO extension_dead_letters
         (delivery_id, event_id, extension_id, event_kind, payload, attempt_count, error_code, retained_until)
         VALUES ($1, $2, $3, $4, '{}'::jsonb, 1, 'legacy', $5)",
    )
    .bind(orphan_delivery_id)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(event_kind)
    .bind(now + time::Duration::days(1))
    .execute(&pool)
    .await
    .expect("legacy orphan DLQ row inserts");
    sqlx::query(
        "ALTER TABLE extension_dead_letters
           ADD CONSTRAINT extension_dead_letters_delivery_fk
           FOREIGN KEY (delivery_id) REFERENCES extension_deliveries (delivery_id)
           NOT VALID",
    )
    .execute(&pool)
    .await
    .expect("delivery FK re-adds as not valid");
    let (fk_validated,): (bool,) = sqlx::query_as(
        "SELECT convalidated FROM pg_constraint
          WHERE conname = 'extension_dead_letters_delivery_fk'",
    )
    .fetch_one(&pool)
    .await
    .expect("delivery FK validation state");
    assert!(!fk_validated);
    let rejected_fk = sqlx::query(
        "INSERT INTO extension_dead_letters
         (delivery_id, event_id, extension_id, event_kind, payload, attempt_count, error_code, retained_until)
         VALUES ($1, $2, $3, $4, '{}'::jsonb, 1, 'new', $5)",
    )
    .bind(new_orphan_delivery_id)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(event_kind)
    .bind(now + time::Duration::days(1))
    .execute(&pool)
    .await;
    assert!(rejected_fk.is_err(), "new orphan DLQ row is rejected");

    sqlx::query("DELETE FROM extension_dead_letters WHERE delivery_id = $1")
        .bind(orphan_delivery_id)
        .execute(&pool)
        .await
        .expect("legacy orphan DLQ cleans up");
    cleanup_lease_fixture(&pool, &[event_id, second_event_id], &[extension_id]).await;
}

#[tokio::test]
async fn registration_round_trip_stores_only_secret_reference() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let extension_id = Uuid::now_v7();
    let registration = registration(extension_id);
    let store = PgExtensionRegistrationStore::new(pool.clone());
    store
        .save_registration(registration.clone())
        .await
        .expect("registration saves");

    let loaded = store
        .find_registration(extension_id)
        .await
        .expect("registration loads")
        .expect("registration exists");
    assert_eq!(loaded, registration);

    let (secret_ref,): (String,) = sqlx::query_as(
        "SELECT signing_secret_ref FROM extension_registrations WHERE extension_id = $1",
    )
    .bind(extension_id)
    .fetch_one(&pool)
    .await
    .expect("registration row exists");
    assert_eq!(secret_ref, "ORBISYNC_EXTENSION_SIGNING_KEY");
    assert_ne!(secret_ref, "raw-secret-value");
}

#[tokio::test]
async fn durable_outbox_assigns_unique_ids_and_public_payloads() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let store = PgExtensionOutboxStore::new(pool.clone());
    let registrations = PgExtensionRegistrationStore::new(pool.clone());
    registrations
        .save_registration(registration(Uuid::now_v7()))
        .await
        .expect("active registration saves");
    store
        .refresh_active_registration_cache()
        .await
        .expect("active registration cache refreshes");
    let user_id = UserId::generate();
    let first = store
        .append_event(ExtensionEvent::UserCreated { user_id })
        .await
        .expect("first event appends");
    let second = store
        .append_event(ExtensionEvent::InstanceStarted {
            instance_id: InstanceId::generate(),
        })
        .await
        .expect("second event appends");
    assert_ne!(first, second);

    let (event_count, distinct_count): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT event_id) FROM outbox_events WHERE event_id IN ($1, $2)",
    )
    .bind(first)
    .bind(second)
    .fetch_one(&pool)
    .await
    .expect("outbox rows query");
    assert_eq!(event_count, 2);
    assert_eq!(distinct_count, 2);

    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM outbox_events WHERE event_id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .expect("event payload exists");
    assert_eq!(payload["user_id"], user_id.to_string());
    assert!(payload.get("signing_secret").is_none());
}

#[tokio::test]
async fn instance_lifecycle_persists_started_and_stopped_events() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let registrations = PgExtensionRegistrationStore::new(pool.clone());
    registrations
        .save_registration(registration(Uuid::now_v7()))
        .await
        .expect("active registration saves");
    let outbox_store = PgExtensionOutboxStore::new(pool.clone());
    outbox_store
        .refresh_active_registration_cache()
        .await
        .expect("active registration cache refreshes");
    let now = Timestamp::from_offset_date_time(time::OffsetDateTime::now_utc());
    let world = World::new(
        WorldId::generate(),
        format!("WH1 world {}", Uuid::now_v7()),
        None,
        Transform::identity(),
        16,
        now,
    )
    .expect("world is valid");
    let instance =
        WorldInstance::new(InstanceId::generate(), world.id(), 16, now).expect("instance is valid");
    let instance_id = instance.id();
    let store =
        PgWorldDirectoryStore::with_codec(pool.clone(), orbisync_testkit::insecure_test_codec());
    store
        .create_world_with_audit(
            world,
            WorldAuditEvent {
                occurred_at: now,
                actor_id: UserId::generate(),
                action: "world.created",
                resource_id: None,
                request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request id"),
                succeeded: true,
            },
        )
        .await
        .expect("world persists");
    store
        .create_instance_with_audit(
            instance,
            WorldAuditEvent {
                occurred_at: now,
                actor_id: UserId::generate(),
                action: "world.instance.created",
                resource_id: Some(instance_id.to_string()),
                request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request id"),
                succeeded: true,
            },
        )
        .await
        .expect("instance persists");

    let actor_id = UserId::generate();
    let use_case = WorldDirectoryUseCase::new(store, orbisync_testkit::AllowAllAuthorizer);
    let started = use_case
        .start_instance(StartInstanceCommand {
            actor_id,
            instance_id,
            now,
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request id"),
        })
        .await
        .expect("instance starts");
    assert_eq!(started.status, "running");
    let stopped = use_case
        .stop_instance(StopInstanceCommand {
            actor_id,
            instance_id,
            now,
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request id"),
        })
        .await
        .expect("instance stops");
    assert_eq!(stopped.status, "stopping");

    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT event_kind FROM outbox_events WHERE payload->>'instance_id' = $1 ORDER BY event_kind",
    )
    .bind(instance_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("lifecycle events query");
    assert_eq!(kinds, ["instance.started", "instance.stopped"]);
}

async fn insert_outbox_row(
    pool: &PgPool,
    event_id: Uuid,
    delivered_at: Option<time::OffsetDateTime>,
) {
    let now = time::OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO outbox_events
         (id, event_id, owner_module, event_type, event_kind, payload,
          created_at, available_at, delivered_at)
         VALUES ($1, $1, 'extensions', 'user.created', 'user.created', '{}', $2, $2, $3)",
    )
    .bind(event_id)
    .bind(now)
    .bind(delivered_at)
    .execute(pool)
    .await
    .expect("outbox row inserts");
}

#[tokio::test]
async fn retention_deletes_old_delivered_events() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_id = Uuid::now_v7();
    let delivered_at = time::OffsetDateTime::now_utc() - time::Duration::days(2);
    insert_outbox_row(&pool, event_id, Some(delivered_at)).await;
    let store = PgExtensionOutboxStore::new(pool.clone());
    let deleted = store
        .delete_expired_delivered(
            time::OffsetDateTime::now_utc() - time::Duration::days(1),
            100,
        )
        .await
        .expect("delivered row retention succeeds");
    assert_eq!(deleted, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .expect("outbox row query");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn retention_keeps_pending_events() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let event_id = Uuid::now_v7();
    insert_outbox_row(&pool, event_id, None).await;
    let store = PgExtensionOutboxStore::new(pool.clone());
    let _deleted = store
        .delete_expired_delivered(time::OffsetDateTime::now_utc(), 100)
        .await
        .expect("pending row retention succeeds");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .expect("pending row query");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn outbox_does_not_persist_without_active_registration() {
    let _guard = extension_test_lock().await;
    let Some(pool) = database().await else { return };
    let active_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT extension_id FROM extension_registrations WHERE status = 'active'",
    )
    .fetch_all(&pool)
    .await
    .expect("active registration ids query");
    sqlx::query("UPDATE extension_registrations SET status = 'suspended' WHERE status = 'active'")
        .execute(&pool)
        .await
        .expect("active registrations suspend");

    let store = PgExtensionOutboxStore::new(pool.clone());
    store
        .refresh_active_registration_cache()
        .await
        .expect("empty active registration cache refreshes");
    let event_id = store
        .append_event(ExtensionEvent::UserCreated {
            user_id: UserId::generate(),
        })
        .await
        .expect("event append returns an id");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .expect("outbox row query");
    assert_eq!(count, 0);

    for extension_id in active_ids {
        sqlx::query("UPDATE extension_registrations SET status = 'active' WHERE extension_id = $1")
            .bind(extension_id)
            .execute(&pool)
            .await
            .expect("active registration restores");
    }
}
