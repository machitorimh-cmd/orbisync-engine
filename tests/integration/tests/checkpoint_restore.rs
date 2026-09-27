//! HIGH-001: a durable checkpoint survives the PostgreSQL JSONB round trip and
//! can be used to restore canonical runtime state.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use std::sync::atomic::{AtomicU64, Ordering};

use orbisync_application::metrics::{Counter, Gauge, Histogram, MetricsRecorder};
use orbisync_application::{AppCheckpoint, ApplicationErrorKind, CheckpointStore};
use orbisync_domain::{
    Entity, EntityId, EntityKind, InstanceId, Revision, Timestamp, VisibilityPolicy,
};
use orbisync_server::delivery::DeliveryRegistry;
use orbisync_server::realtime_ws::RealtimeState;
use orbisync_storage_postgres::PgCheckpointStore;
use orbisync_world_runtime::command::{InstanceCommand, WorldPermissions};
use orbisync_world_runtime::{
    Checkpoint, CheckpointDedupEntry, CheckpointDedupResult, RuntimeRegistry,
};
use uuid::Uuid;

mod common;

/// Counts `checkpoint_restore_rejected_total` increments (HIGH-001 round 5):
/// join must classify an oversized `load_latest` failure and count it,
/// whether the rejection came from the pre-fetch length gate or the
/// post-fetch exact check.
#[derive(Default)]
struct RestoreRejectionCounter {
    checkpoint_restore_rejected_total: AtomicU64,
}

impl MetricsRecorder for RestoreRejectionCounter {
    fn incr(&self, counter: Counter) {
        self.add(counter, 1);
    }

    fn add(&self, counter: Counter, n: u64) {
        if matches!(counter, Counter::CheckpointRestoreRejectedTotal) {
            self.checkpoint_restore_rejected_total
                .fetch_add(n, Ordering::SeqCst);
        }
    }

    fn set(&self, _gauge: Gauge, _value: i64) {}

    fn observe(&self, _histogram: Histogram, _value: f64) {}
}

async fn insert_oversized_checkpoint_row(
    pool: &sqlx::PgPool,
    instance_id: InstanceId,
    filler_bytes: usize,
) {
    sqlx::query(
        "INSERT INTO instance_checkpoints
         (id, instance_id, revision, data, created_at)
         VALUES (gen_random_uuid(), $1, 1,
                ('{\"format_version\":2,\"instance_id\":\"' || $1::text || '\",\"revision\":1,\"timestamp\":\"2026-01-01T00:00:00Z\",\"entities\":[],\"filler\":\"' || repeat('x', $2::int) || '\"}')::jsonb,
                 now())",
    )
    .bind(instance_id.as_uuid())
    .bind(filler_bytes as i32)
    .execute(pool)
    .await
    .expect("insert oversized checkpoint row");
}

/// HIGH-001 (round 5): a row that fails the PostgreSQL-side pre-fetch length
/// gate must surface as `CheckpointTooLarge` through `load_latest`, and
/// `ensure_instance_activated` must classify that on join: count
/// `checkpoint_restore_rejected_total` and emit
/// `checkpoint.restore_rejected_too_large`. Before the round-5 fix, the
/// pre-fetch gate returned a generic `PortFailure` and the join path threw
/// away `kind()` entirely, so the counter never moved.
#[tokio::test]
async fn ensure_instance_activated_counts_prefetch_oversized_row_on_join() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let world_id = Uuid::now_v7();
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'checkpoint-restore-prefetch-test', 'active', 10,
                 '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                 '{}'::jsonb, 1, now(), now())",
    )
    .bind(world_id)
    .execute(&pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances
         (id, world_id, lifecycle, capacity, created_at, started_at, revision)
         VALUES ($1, $2, 'running', 10, now(), now(), 1)",
    )
    .bind(instance_id.as_uuid())
    .bind(world_id)
    .execute(&pool)
    .await
    .expect("insert instance");
    // 17 MiB filler: canonical JSONB text exceeds the 16 MiB (2x) pre-fetch gate.
    insert_oversized_checkpoint_row(&pool, instance_id, 17 * 1024 * 1024).await;

    let registry = Arc::new(RuntimeRegistry::new());
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(PgCheckpointStore::new(pool.clone()));
    let metrics = Arc::new(RestoreRejectionCounter::default());
    let state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::clone(&registry),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_checkpoint_store(Arc::clone(&checkpoint_store))
    .with_metrics_recorder(Arc::clone(&metrics) as Arc<dyn MetricsRecorder>)
    .build();

    let error = state
        .ensure_instance_activated(instance_id)
        .await
        .expect_err("oversized checkpoint row must fail activation");
    assert!(error.to_string().contains("exceeds"), "{error}");
    assert_eq!(
        metrics
            .checkpoint_restore_rejected_total
            .load(Ordering::SeqCst),
        1,
        "pre-fetch rejection on join must be counted"
    );

    sqlx::query("DELETE FROM instance_checkpoints WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete checkpoints");
    sqlx::query("DELETE FROM world_instances WHERE id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete instance");
    sqlx::query("DELETE FROM world_definitions WHERE id = $1")
        .bind(world_id)
        .execute(&pool)
        .await
        .expect("delete world");
}

/// HIGH-001 (round 5): a row that passes the pre-fetch gate but fails the
/// exact post-fetch `payload.len()` check must also be classified and
/// counted on join. Before the round-5 fix this path used
/// `error.to_string()` and threw away `kind()`, so
/// `checkpoint_restore_rejected_total` stayed at 0 even though the store
/// itself correctly returned `CheckpointTooLarge`.
#[tokio::test]
async fn ensure_instance_activated_counts_postfetch_oversized_row_on_join() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let world_id = Uuid::now_v7();
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'checkpoint-restore-postfetch-test', 'active', 10,
                 '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                 '{}'::jsonb, 1, now(), now())",
    )
    .bind(world_id)
    .execute(&pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances
         (id, world_id, lifecycle, capacity, created_at, started_at, revision)
         VALUES ($1, $2, 'running', 10, now(), now(), 1)",
    )
    .bind(instance_id.as_uuid())
    .bind(world_id)
    .execute(&pool)
    .await
    .expect("insert instance");
    // Filler sized between the 8 MiB limit and the 16 MiB pre-fetch gate (8500
    // KiB): the row passes the length-only probe and reaches the exact
    // post-fetch check.
    insert_oversized_checkpoint_row(&pool, instance_id, 8500 * 1024).await;

    let registry = Arc::new(RuntimeRegistry::new());
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(PgCheckpointStore::new(pool.clone()));
    let metrics = Arc::new(RestoreRejectionCounter::default());
    let state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::clone(&registry),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_checkpoint_store(Arc::clone(&checkpoint_store))
    .with_metrics_recorder(Arc::clone(&metrics) as Arc<dyn MetricsRecorder>)
    .build();

    let error = state
        .ensure_instance_activated(instance_id)
        .await
        .expect_err("oversized checkpoint row must fail activation");
    assert!(error.to_string().contains("exceeds"), "{error}");
    assert!(
        !error
            .to_string()
            .contains("rejected before reading payload"),
        "this row must reach the exact post-fetch check, not the pre-fetch gate: {error}"
    );
    assert_eq!(
        metrics
            .checkpoint_restore_rejected_total
            .load(Ordering::SeqCst),
        1,
        "post-fetch rejection on join must be counted"
    );

    sqlx::query("DELETE FROM instance_checkpoints WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete checkpoints");
    sqlx::query("DELETE FROM world_instances WHERE id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete instance");
    sqlx::query("DELETE FROM world_definitions WHERE id = $1")
        .bind(world_id)
        .execute(&pool)
        .await
        .expect("delete world");
}

/// HIGH-001 (round 3): an oversized durable row must be rejected by a
/// PostgreSQL-side length probe (`octet_length(data::text)`) BEFORE the JSONB
/// payload is transferred into this process. The store rejects rows whose
/// canonical JSONB text exceeds twice the payload limit (`2 * 8 MiB`, see the
/// derivation there), so the filler inflates the canonical text past that
/// 16 MiB gate without allocating the payload in this process.
#[tokio::test]
async fn postgres_checkpoint_load_rejects_oversized_row_before_fetching_payload() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let world_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'checkpoint-oversized-test', 'active', 10,
                 '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                 '{}'::jsonb, 1, now(), now())",
    )
    .bind(world_id)
    .execute(&pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances
         (id, world_id, lifecycle, capacity, created_at, started_at, revision)
         VALUES ($1, $2, 'running', 10, now(), now(), 1)",
    )
    .bind(instance_id.as_uuid())
    .bind(world_id)
    .execute(&pool)
    .await
    .expect("insert instance");
    sqlx::query(
        "INSERT INTO instance_checkpoints
         (id, instance_id, revision, data, created_at)
         VALUES (gen_random_uuid(), $1, 1,
                ('{\"format_version\":2,\"instance_id\":\"' || $1::text || '\",\"revision\":1,\"timestamp\":\"2026-01-01T00:00:00Z\",\"entities\":[],\"filler\":\"' || repeat('x', 17 * 1024 * 1024) || '\"}')::jsonb,
                 now())",
    )
    .bind(instance_id.as_uuid())
    .execute(&pool)
    .await
    .expect("insert oversized checkpoint row");

    let store = PgCheckpointStore::new(pool.clone());
    let error = store
        .load_latest(instance_id)
        .await
        .expect_err("oversized row must be rejected before the payload is fetched");
    let detail = error.to_string();
    assert!(detail.contains("exceeds"), "detail: {detail}");
    assert!(
        detail.contains("rejected before reading payload"),
        "rejection must come from the length-only pre-check: {detail}"
    );

    sqlx::query("DELETE FROM instance_checkpoints WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete oversized row");
    sqlx::query("DELETE FROM world_instances WHERE id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete instance");
    sqlx::query("DELETE FROM world_definitions WHERE id = $1")
        .bind(world_id)
        .execute(&pool)
        .await
        .expect("delete world");
}

#[tokio::test]
async fn postgres_checkpoint_round_trip_restores_runtime_state() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let world_id = Uuid::now_v7();
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let timestamp = Timestamp::from_unix_millis(1_788_000_000_000).expect("timestamp");
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'checkpoint-test', 'active', 10,
                 '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                 '{}'::jsonb, 1, $2, $2)",
    )
    .bind(world_id)
    .bind(timestamp.as_offset_date_time())
    .execute(&pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances
         (id, world_id, lifecycle, capacity, created_at, started_at, revision)
         VALUES ($1, $2, 'running', 10, $3, $3, 1)",
    )
    .bind(instance_id.as_uuid())
    .bind(world_id)
    .bind(timestamp.as_offset_date_time())
    .execute(&pool)
    .await
    .expect("insert instance");

    let entities = (0..40)
        .map(|entity_index| {
            let mut components = HashMap::new();
            for component_index in 0..8 {
                components.insert(
                    format!("example.state{entity_index}.{component_index}"),
                    vec![255; 4096],
                );
            }
            Entity::from_persisted(
                EntityId::generate(),
                instance_id,
                EntityKind::Object,
                None,
                None,
                VisibilityPolicy::custom("example.policy").expect("visibility"),
                Revision::from_u64(3),
                timestamp,
                timestamp,
                components,
            )
            .expect("entity")
        })
        .collect::<Vec<_>>();
    let runtime_checkpoint =
        Checkpoint::new(instance_id, Revision::from_u64(9), entities, timestamp);
    let durable = AppCheckpoint::new(
        instance_id,
        runtime_checkpoint.revision,
        runtime_checkpoint.to_json_bytes().expect("serialize"),
        timestamp,
    );
    let store = PgCheckpointStore::new(pool.clone());

    store
        .save_checkpoint(durable.clone())
        .await
        .expect("save checkpoint");

    // A rejected command outcome is durable progress even when the actor
    // revision is unchanged. It must be merged into and restored from the
    // same revision's checkpoint.
    let rejected_id = orbisync_domain::CommandId::generate();
    let dedup_timestamp = Timestamp::from_unix_millis(1_788_000_000_001).expect("timestamp");
    let mut rejected_checkpoint = Checkpoint::new(
        instance_id,
        runtime_checkpoint.revision,
        runtime_checkpoint.entities.clone(),
        dedup_timestamp,
    );
    rejected_checkpoint.dedup = vec![CheckpointDedupEntry {
        command_id: rejected_id.to_string(),
        fingerprint: vec![0; 32],
        created_at_millis: dedup_timestamp.to_unix_millis().expect("millis"),
        expires_at_millis: dedup_timestamp.to_unix_millis().expect("millis") + 24 * 60 * 60 * 1_000,
        message_id: rejected_id.to_string(),
        result: CheckpointDedupResult::Rejected {
            code: String::from("CONFLICT"),
            detail: String::from("same revision rejected command"),
        },
    }];
    store
        .save_checkpoint(AppCheckpoint::new(
            instance_id,
            rejected_checkpoint.revision,
            rejected_checkpoint
                .to_json_bytes()
                .expect("serialize rejected"),
            rejected_checkpoint.timestamp,
        ))
        .await
        .expect("same-revision rejected dedup save");

    // Two independent adapter instances represent two server processes. Once
    // a revision is durable, neither a same-revision stale snapshot nor its
    // concurrent twin may become the latest row.
    let stale_a = Checkpoint::new(
        instance_id,
        runtime_checkpoint.revision,
        Vec::new(),
        Timestamp::from_unix_millis(1_788_000_000_001).expect("timestamp"),
    );
    let stale_b = Checkpoint::new(
        instance_id,
        runtime_checkpoint.revision,
        Vec::new(),
        Timestamp::from_unix_millis(1_788_000_000_002).expect("timestamp"),
    );
    let stale_a = AppCheckpoint::new(
        instance_id,
        stale_a.revision,
        stale_a.to_json_bytes().expect("serialize stale a"),
        stale_a.timestamp,
    );
    let stale_b = AppCheckpoint::new(
        instance_id,
        stale_b.revision,
        stale_b.to_json_bytes().expect("serialize stale b"),
        stale_b.timestamp,
    );
    let store_b = store.clone();
    let (result_a, result_b) = tokio::join!(
        store.save_checkpoint(stale_a),
        store_b.save_checkpoint(stale_b),
    );
    assert!(
        result_a.is_err(),
        "stale same-revision writer must be rejected"
    );
    assert!(
        result_b.is_err(),
        "stale same-revision writer must be rejected"
    );

    let loaded = store
        .load_latest(instance_id)
        .await
        .expect("load checkpoint")
        .expect("checkpoint exists");
    let restored = Checkpoint::from_json_bytes(&loaded.payload).expect("decode checkpoint");

    assert_eq!(loaded.instance_id, durable.instance_id);
    assert_eq!(loaded.revision, durable.revision);
    assert_eq!(restored.instance_id, runtime_checkpoint.instance_id);
    assert_eq!(restored.revision, runtime_checkpoint.revision);
    assert_eq!(restored.entities, runtime_checkpoint.entities);
    assert_eq!(restored.dedup.len(), 1);
    assert!(matches!(
        &restored.dedup[0].result,
        CheckpointDedupResult::Rejected { code, detail }
            if code == "CONFLICT" && detail == "same revision rejected command"
    ));
    assert_eq!(restored.dedup[0].command_id, rejected_id.to_string());
    assert!(
        loaded.payload.len() > 2 * 1024 * 1024,
        "the legal large checkpoint must exercise the size headroom"
    );

    // Same actor revision with a different world state is a real conflict,
    // rather than another dedup-only progress update.
    assert_eq!(
        store
            .save_checkpoint(AppCheckpoint::new(
                instance_id,
                runtime_checkpoint.revision,
                Checkpoint::new(
                    instance_id,
                    runtime_checkpoint.revision,
                    Vec::new(),
                    dedup_timestamp,
                )
                .to_json_bytes()
                .expect("serialize conflicting world state"),
                dedup_timestamp,
            ))
            .await
            .expect_err("different same-revision world state must conflict")
            .kind(),
        ApplicationErrorKind::Conflict
    );

    sqlx::query("DELETE FROM instance_checkpoints WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete checkpoints");
    sqlx::query("DELETE FROM world_instances WHERE id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete instance");
    sqlx::query("DELETE FROM world_definitions WHERE id = $1")
        .bind(world_id)
        .execute(&pool)
        .await
        .expect("delete world");
}

#[tokio::test]
async fn postgres_checkpoint_lifecycle_rejoin_restores_entity_and_monotonic_revision() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let world_id = Uuid::now_v7();
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let timestamp = Timestamp::from_unix_millis(1_788_000_100_000).expect("timestamp");
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'checkpoint-lifecycle-test', 'active', 10,
                 '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                 '{}'::jsonb, 1, $2, $2)",
    )
    .bind(world_id)
    .bind(timestamp.as_offset_date_time())
    .execute(&pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances
         (id, world_id, lifecycle, capacity, created_at, started_at, revision)
         VALUES ($1, $2, 'running', 10, $3, $3, 1)",
    )
    .bind(instance_id.as_uuid())
    .bind(world_id)
    .bind(timestamp.as_offset_date_time())
    .execute(&pool)
    .await
    .expect("insert instance");

    let registry = Arc::new(RuntimeRegistry::new());
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(PgCheckpointStore::new(pool.clone()));
    let mut state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::clone(&registry),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_checkpoint_store(Arc::clone(&checkpoint_store))
    .build();
    let initial_activation = state
        .ensure_instance_activated(instance_id)
        .await
        .expect("initial activation reaches the production restore path");
    let handle = registry
        .handle(instance_id)
        .expect("initial actor is published");
    drop(initial_activation);
    let entity_id = EntityId::generate();
    let requester = orbisync_domain::UserId::generate();
    let outcome = handle
        .submit(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: EntityKind::Object,
            owner: None,
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester,
            permissions: WorldPermissions::all(),
        })
        .await
        .expect("spawn request reaches actor");
    assert!(matches!(
        outcome,
        orbisync_world_runtime::command::CommandOutcome::Applied { .. }
    ));
    let before_reap = handle.read_snapshot().await.expect("snapshot before reap");

    let reap = handle
        .reap_if_idle(timestamp)
        .await
        .expect("idle actor is reaped");
    let checkpoint = reap.checkpoint.clone();
    assert_eq!(checkpoint.entity_count(), 1);
    assert_eq!(checkpoint.entities[0].id(), entity_id);
    assert!(checkpoint.revision >= before_reap.revision);

    let durable = AppCheckpoint::new(
        instance_id,
        checkpoint.revision,
        checkpoint.to_json_bytes().expect("serialize checkpoint"),
        checkpoint.timestamp,
    );
    sqlx::query(
        "INSERT INTO users
         (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at)
         VALUES ($1, $2, 'checkpoint-test-user', 'active', false, 1, $3, $3)",
    )
    .bind(requester.as_uuid())
    .bind(format!("checkpoint-{requester}"))
    .bind(timestamp.as_offset_date_time())
    .execute(&pool)
    .await
    .expect("insert checkpoint owner");
    checkpoint_store
        .save_checkpoint(durable)
        .await
        .expect("persist reaped checkpoint");
    assert!(registry.remove_task(instance_id));

    let loaded = checkpoint_store
        .load_latest(instance_id)
        .await
        .expect("load persisted checkpoint")
        .expect("checkpoint exists after reap");
    let restored = Checkpoint::from_json_bytes(&loaded.payload).expect("decode checkpoint");
    let restored_entity = restored.entities[0].clone();
    state.checkpoint_store = None;
    let unavailable = state
        .ensure_instance_activated(instance_id)
        .await
        .expect_err("rejoin must fail closed when its checkpoint store is unavailable");
    assert!(unavailable.to_string().contains("not configured"));
    assert!(!registry.contains(instance_id));
    state.checkpoint_store = Some(Arc::clone(&checkpoint_store));
    let activation = state
        .ensure_instance_activated(instance_id)
        .await
        .expect("rejoin uses the production activation path");
    let rejoined = registry
        .handle(instance_id)
        .expect("restored actor is published");
    let rejoin_outcome = rejoined
        .submit(InstanceCommand::Join {
            presence_id: orbisync_domain::PresenceId::generate(),
            user_id: requester,
            instance_id,
            capacity: 10,
        })
        .await
        .expect("rejoin request reaches restored actor");
    assert!(matches!(
        rejoin_outcome,
        orbisync_world_runtime::command::CommandOutcome::Applied { .. }
    ));
    drop(activation);
    let after_rejoin = rejoined
        .read_snapshot()
        .await
        .expect("snapshot after rejoin");

    assert_eq!(after_rejoin.entities, vec![restored_entity]);
    assert_eq!(after_rejoin.entities[0].id(), entity_id);
    assert!(
        after_rejoin.revision > loaded.revision,
        "rejoin must advance the persisted revision"
    );

    sqlx::query("DELETE FROM instance_checkpoints WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete checkpoints");
    sqlx::query("DELETE FROM world_instances WHERE id = $1")
        .bind(instance_id.as_uuid())
        .execute(&pool)
        .await
        .expect("delete instance");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(requester.as_uuid())
        .execute(&pool)
        .await
        .expect("delete checkpoint owner");
    sqlx::query("DELETE FROM world_definitions WHERE id = $1")
        .bind(world_id)
        .execute(&pool)
        .await
        .expect("delete world");
}
