//! Activation-time restore must merge `persistent_entities` rows that are
//! newer than the latest saved checkpoint (`state-and-runtime.md` §1.2/§1.3).
//!
//! Rows are written on every persistence-relevant mutation (every tick at
//! `world.server_tick_hz`, default 20Hz) while checkpoints are only taken
//! periodically (`world.checkpoint_interval_secs`, default 300s). A crash
//! between checkpoints therefore leaves durable rows ahead of the last saved
//! checkpoint by up to that interval; before this fix, `ensure_instance_activated`
//! only read `CheckpointStore::load_latest` and never consulted the row
//! tables, so that gap was silently dropped on restore. Graceful shutdown is
//! unaffected (`flush_shutdown_checkpoints` saves a final checkpoint that
//! already reflects every row), so these tests deliberately simulate a crash:
//! they write rows directly through `PersistentEntityStore` and never submit
//! a shutdown/reap, then activate a fresh `RealtimeState` as a new process
//! would.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use orbisync_application::{AppCheckpoint, CheckpointStore, PersistentEntityStore};
use orbisync_domain::{
    Entity, EntityId, EntityKind, InstanceId, Revision, Timestamp, VisibilityPolicy,
};
use orbisync_server::delivery::DeliveryRegistry;
use orbisync_server::realtime_ws::RealtimeState;
use orbisync_storage_postgres::{PgCheckpointStore, PgPersistentEntityStore};
use orbisync_world_runtime::{Checkpoint, RuntimeRegistry};
use uuid::Uuid;

mod common;

async fn insert_instance(pool: &sqlx::PgPool, instance_id: InstanceId, at: Timestamp) -> Uuid {
    let world_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'persistent-entity-restore-test', 'active', 10,
                 '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                 '{}'::jsonb, 1, $2, $2)",
    )
    .bind(world_id)
    .bind(at.as_offset_date_time())
    .execute(pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances
         (id, world_id, lifecycle, capacity, created_at, started_at, revision)
         VALUES ($1, $2, 'running', 10, $3, $3, 1)",
    )
    .bind(instance_id.as_uuid())
    .bind(world_id)
    .bind(at.as_offset_date_time())
    .execute(pool)
    .await
    .expect("insert instance");
    world_id
}

async fn cleanup(pool: &sqlx::PgPool, instance_id: InstanceId, world_id: Uuid) {
    sqlx::query("DELETE FROM instance_checkpoints WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(pool)
        .await
        .expect("delete checkpoints");
    sqlx::query("DELETE FROM persistent_entities WHERE instance_id = $1")
        .bind(instance_id.as_uuid())
        .execute(pool)
        .await
        .expect("delete entities");
    sqlx::query("DELETE FROM world_instances WHERE id = $1")
        .bind(instance_id.as_uuid())
        .execute(pool)
        .await
        .expect("delete instance");
    sqlx::query("DELETE FROM world_definitions WHERE id = $1")
        .bind(world_id)
        .execute(pool)
        .await
        .expect("delete world");
}

/// Crash before the first periodic checkpoint: no `instance_checkpoints` row
/// exists at all, but the tick loop already wrote a `persistent_entities`
/// row. Before this fix, `ensure_instance_activated` fell back to a brand
/// new empty actor whenever `load_latest` returned `None`, silently losing
/// the entity.
#[tokio::test]
async fn ensure_instance_activated_restores_entity_with_no_prior_checkpoint() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let at = Timestamp::from_unix_millis(1_788_100_000_000).expect("timestamp");
    let world_id = insert_instance(&pool, instance_id, at).await;

    let entity_store = PgPersistentEntityStore::new(pool.clone());
    let entity = Entity::new(
        EntityId::generate(),
        instance_id,
        EntityKind::Object,
        None,
        None,
        VisibilityPolicy::Global,
        at,
    );
    // Simulates the tick-loop write (`persist_entity_events`, main.rs) that
    // runs every tick, independent of the periodic checkpoint cadence.
    entity_store.spawn(entity.clone()).await.expect("spawn row");

    let registry = Arc::new(RuntimeRegistry::new());
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(PgCheckpointStore::new(pool.clone()));
    let persistent_entity_store: Arc<dyn PersistentEntityStore> = Arc::new(entity_store);
    let state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::clone(&registry),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_checkpoint_store(Arc::clone(&checkpoint_store))
    .with_persistent_entity_store(Arc::clone(&persistent_entity_store))
    .build();

    let activation = state
        .ensure_instance_activated(instance_id)
        .await
        .expect("activation restores from rows despite no checkpoint");
    let handle = registry
        .handle(instance_id)
        .expect("restored actor is published");
    drop(activation);
    let snapshot = handle
        .read_snapshot()
        .await
        .expect("snapshot after row-only restore");

    assert_eq!(
        snapshot.entities.len(),
        1,
        "the crashed instance's only durable row must survive activation"
    );
    assert_eq!(snapshot.entities[0].id(), entity.id());

    cleanup(&pool, instance_id, world_id).await;
}

/// Crash after a checkpoint but before the next one: a durable
/// `instance_checkpoints` row exists at an older revision, and the tick loop
/// wrote a newer `persistent_entities` row (a new component) in the gap.
/// Before this fix, `ensure_instance_activated` restored strictly from the
/// stale checkpoint payload and the newer row was never read, so the
/// component added after the checkpoint was lost on restore.
#[tokio::test]
async fn ensure_instance_activated_restores_entity_state_newer_than_the_last_checkpoint() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let at = Timestamp::from_unix_millis(1_788_200_000_000).expect("timestamp");
    let world_id = insert_instance(&pool, instance_id, at).await;

    let entity_store = PgPersistentEntityStore::new(pool.clone());
    let entity = Entity::new(
        EntityId::generate(),
        instance_id,
        EntityKind::Object,
        None,
        None,
        VisibilityPolicy::Global,
        at,
    );
    entity_store.spawn(entity.clone()).await.expect("spawn row");

    // A periodic checkpoint fires here, capturing the entity at revision 1
    // with no components.
    let checkpoint_store = PgCheckpointStore::new(pool.clone());
    let first_checkpoint =
        Checkpoint::new(instance_id, entity.revision(), vec![entity.clone()], at);
    checkpoint_store
        .save_checkpoint(AppCheckpoint::new(
            instance_id,
            first_checkpoint.revision,
            first_checkpoint.to_json_bytes().expect("serialize"),
            first_checkpoint.timestamp,
        ))
        .await
        .expect("save first checkpoint");

    // Between this checkpoint and the next (up to
    // `world.checkpoint_interval_secs` later), the tick loop persists a
    // component update to the row table. The process then crashes before the
    // next periodic checkpoint, so this mutation exists only as a row.
    let after_checkpoint = Timestamp::from_unix_millis(1_788_200_000_500).expect("timestamp");
    entity_store
        .upsert_component(
            entity.id(),
            instance_id,
            Revision::from_u64(2),
            after_checkpoint,
            "example.after_checkpoint".to_owned(),
            vec![9, 9, 9],
        )
        .await
        .expect("upsert row past the last checkpoint");

    let registry = Arc::new(RuntimeRegistry::new());
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(checkpoint_store);
    let persistent_entity_store: Arc<dyn PersistentEntityStore> = Arc::new(entity_store);
    let state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::clone(&registry),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_checkpoint_store(Arc::clone(&checkpoint_store))
    .with_persistent_entity_store(Arc::clone(&persistent_entity_store))
    .build();

    let activation = state
        .ensure_instance_activated(instance_id)
        .await
        .expect("activation merges rows newer than the checkpoint");
    let handle = registry
        .handle(instance_id)
        .expect("restored actor is published");
    drop(activation);
    let snapshot = handle
        .read_snapshot()
        .await
        .expect("snapshot after merged restore");

    assert_eq!(snapshot.entities.len(), 1);
    let restored = &snapshot.entities[0];
    assert_eq!(restored.id(), entity.id());
    assert_eq!(
        restored.revision(),
        Revision::from_u64(2),
        "the restored entity must be at the row's revision, not the stale checkpoint's"
    );
    let mut expected_components = HashMap::new();
    expected_components.insert("example.after_checkpoint".to_owned(), vec![9, 9, 9]);
    assert_eq!(
        restored.components(),
        &expected_components,
        "the component written after the last checkpoint must survive restore"
    );

    cleanup(&pool, instance_id, world_id).await;
}
