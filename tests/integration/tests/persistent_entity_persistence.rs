//! Persistent entity and component rows survive real PostgreSQL round trips
//! (`state-and-runtime.md` §1.2).
//!
//! Covers the port contract (spawn/update/delete) plus the schema-level DM-04
//! limits: 16 components per entity and 4096 bytes per payload.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use orbisync_application::{ApplicationErrorKind, PersistentEntityStore};
use orbisync_domain::{
    Entity, EntityId, EntityKind, InstanceId, Quaternion, Revision, Timestamp, Transform, Vec3,
    VisibilityPolicy,
};
use orbisync_storage_postgres::PgPersistentEntityStore;
use uuid::Uuid;

mod common;

async fn insert_instance(pool: &sqlx::PgPool, instance_id: InstanceId, at: Timestamp) -> Uuid {
    let world_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO world_definitions
         (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
         VALUES ($1, 'persistent-entity-test', 'active', 10,
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

#[tokio::test]
async fn postgres_persistent_entity_spawn_update_delete_round_trip() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let at = Timestamp::from_unix_millis(1_788_000_000_000).expect("timestamp");
    let world_id = insert_instance(&pool, instance_id, at).await;
    let store = PgPersistentEntityStore::new(pool.clone());

    let mut components = HashMap::new();
    components.insert("com.example.door".to_owned(), vec![0, 1, 2, 255]);
    components.insert("org.school.board".to_owned(), vec![7; 16]);
    let transform = Transform::new(
        Vec3::new(1.5, 2.5, 3.5).expect("position"),
        Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("rotation"),
        Vec3::new(1.0, 1.0, 1.0).expect("scale"),
    )
    .expect("transform");
    let entity = Entity::from_persisted(
        EntityId::generate(),
        instance_id,
        EntityKind::Object,
        None,
        Some(transform),
        VisibilityPolicy::custom("example.policy").expect("visibility"),
        Revision::from_u64(3),
        at,
        at,
        components,
    )
    .expect("entity");

    store.spawn(entity.clone()).await.expect("spawn");
    let spawned: (String, Option<serde_json::Value>, serde_json::Value, i64) = sqlx::query_as(
        "SELECT kind, transform, visibility, revision FROM persistent_entities WHERE id = $1",
    )
    .bind(entity.id().as_uuid())
    .fetch_one(&pool)
    .await
    .expect("entity row exists");
    assert_eq!(spawned.0, "object");
    assert_eq!(
        spawned.1,
        Some(serde_json::json!({
            "position": {"x": 1.5, "y": 2.5, "z": 3.5},
            "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
            "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
        }))
    );
    assert_eq!(
        spawned.2,
        serde_json::json!({"type": "custom", "tag": "example.policy"})
    );
    assert_eq!(spawned.3, 3);

    let stored_payloads: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT component_key, payload FROM persistent_entity_components WHERE entity_id = $1 ORDER BY component_key",
    )
    .bind(entity.id().as_uuid())
    .fetch_all(&pool)
    .await
    .expect("component rows exist");
    assert_eq!(
        stored_payloads,
        vec![
            (String::from("com.example.door"), vec![0, 1, 2, 255]),
            (String::from("org.school.board"), vec![7; 16]),
        ]
    );

    // Duplicate spawn must conflict, then an update replaces the component set.
    assert_eq!(
        store
            .spawn(entity.clone())
            .await
            .expect_err("duplicate spawn must conflict")
            .kind(),
        ApplicationErrorKind::Conflict
    );

    let mut updated = entity.clone();
    let revision = updated.revision();
    updated
        .update_component(
            revision,
            "com.example.door".to_owned(),
            vec![9],
            Timestamp::from_unix_millis(1_788_000_000_001).expect("timestamp"),
        )
        .expect("component update");
    store
        .upsert_component(
            updated.id(),
            instance_id,
            updated.revision(),
            updated.updated_at(),
            "com.example.door".to_owned(),
            vec![9],
        )
        .await
        .expect("component upsert");
    let revision_after_update: i64 =
        sqlx::query_scalar("SELECT revision FROM persistent_entities WHERE id = $1")
            .bind(updated.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("entity row exists");
    assert_eq!(revision_after_update, 4);
    let payload_after_update: Vec<u8> =
        sqlx::query_scalar("SELECT payload FROM persistent_entity_components WHERE entity_id = $1 AND component_key = 'com.example.door'")
            .bind(updated.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("component row exists");
    assert_eq!(payload_after_update, vec![9]);

    // Updating one component must not touch the other component's row.
    let other_component_payload: Vec<u8> =
        sqlx::query_scalar("SELECT payload FROM persistent_entity_components WHERE entity_id = $1 AND component_key = 'org.school.board'")
            .bind(updated.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("other component row exists");
    assert_eq!(other_component_payload, vec![7; 16]);

    // A write at the same revision as the durable row must conflict, not
    // silently overwrite (strict optimistic locking).
    assert_eq!(
        store
            .upsert_component(
                updated.id(),
                instance_id,
                updated.revision(),
                updated.updated_at(),
                "com.example.door".to_owned(),
                vec![0],
            )
            .await
            .expect_err("same-revision component write must conflict")
            .kind(),
        ApplicationErrorKind::Conflict
    );
    assert_eq!(
        store
            .update(updated.clone())
            .await
            .expect_err("same-revision entity update must conflict")
            .kind(),
        ApplicationErrorKind::Conflict
    );

    // A stale snapshot must not overwrite the durable row.
    let stale = Entity::from_persisted(
        updated.id(),
        instance_id,
        EntityKind::Object,
        None,
        None,
        VisibilityPolicy::Global,
        Revision::from_u64(2),
        at,
        at,
        HashMap::new(),
    )
    .expect("stale snapshot");
    assert_eq!(
        store
            .update(stale)
            .await
            .expect_err("stale update must conflict")
            .kind(),
        ApplicationErrorKind::Conflict
    );

    // Updating or deleting an unknown entity fails closed.
    let unknown = EntityId::generate();
    assert_eq!(
        store
            .delete(unknown, instance_id)
            .await
            .expect_err("unknown delete must fail")
            .kind(),
        ApplicationErrorKind::NotFound
    );
    let unknown_entity = Entity::new(
        unknown,
        instance_id,
        EntityKind::Trigger,
        None,
        None,
        VisibilityPolicy::Global,
        at,
    );
    assert_eq!(
        store
            .update(unknown_entity)
            .await
            .expect_err("unknown update must fail")
            .kind(),
        ApplicationErrorKind::NotFound
    );

    // Deleting a single component leaves its sibling intact, and repeating
    // the delete against an already-absent component fails closed rather
    // than silently succeeding.
    let delete_component_at = Timestamp::from_unix_millis(1_788_000_000_002).expect("timestamp");
    store
        .delete_component(
            updated.id(),
            instance_id,
            Revision::from_u64(5),
            delete_component_at,
            "org.school.board".to_owned(),
        )
        .await
        .expect("component delete");
    let remaining_component_keys: Vec<String> = sqlx::query_scalar(
        "SELECT component_key FROM persistent_entity_components WHERE entity_id = $1 ORDER BY component_key",
    )
    .bind(updated.id().as_uuid())
    .fetch_all(&pool)
    .await
    .expect("component keys");
    assert_eq!(
        remaining_component_keys,
        vec![String::from("com.example.door")]
    );
    let revision_after_delete: i64 =
        sqlx::query_scalar("SELECT revision FROM persistent_entities WHERE id = $1")
            .bind(updated.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("entity row exists");
    assert_eq!(revision_after_delete, 5);

    assert_eq!(
        store
            .delete_component(
                updated.id(),
                instance_id,
                Revision::from_u64(6),
                delete_component_at,
                "org.school.board".to_owned(),
            )
            .await
            .expect_err("deleting an already-absent component must fail")
            .kind(),
        ApplicationErrorKind::NotFound
    );

    assert_eq!(
        store
            .delete_component(
                unknown,
                instance_id,
                Revision::from_u64(2),
                delete_component_at,
                "com.example.door".to_owned(),
            )
            .await
            .expect_err("deleting a component of an unknown entity must fail")
            .kind(),
        ApplicationErrorKind::NotFound
    );

    store
        .delete(updated.id(), instance_id)
        .await
        .expect("delete");
    let remaining_entity: Option<i64> =
        sqlx::query_scalar("SELECT count(*) FROM persistent_entities WHERE id = $1")
            .bind(updated.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count entities");
    assert_eq!(remaining_entity, Some(0));
    let remaining_components: Option<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM persistent_entity_components WHERE entity_id = $1",
    )
    .bind(updated.id().as_uuid())
    .fetch_one(&pool)
    .await
    .expect("count components");
    assert_eq!(remaining_components, Some(0));
    assert_eq!(
        store
            .delete(updated.id(), instance_id)
            .await
            .expect_err("second delete must fail")
            .kind(),
        ApplicationErrorKind::NotFound
    );

    cleanup(&pool, instance_id, world_id).await;
}

#[tokio::test]
async fn schema_enforces_component_limits() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
    let at = Timestamp::from_unix_millis(1_788_000_000_000).expect("timestamp");
    let world_id = insert_instance(&pool, instance_id, at).await;

    let entity_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO persistent_entities
         (id, instance_id, kind, owner_id, transform, visibility, revision, created_at, updated_at)
         VALUES ($1, $2, 'object', NULL, NULL, '{\"type\":\"global\"}'::jsonb, 1, $3, $3)",
    )
    .bind(entity_id)
    .bind(instance_id.as_uuid())
    .bind(at.as_offset_date_time())
    .execute(&pool)
    .await
    .expect("insert entity row");

    // 16 components are accepted, the 17th must fail.
    for index in 0..16i32 {
        sqlx::query(
            "INSERT INTO persistent_entity_components (entity_id, component_key, payload)
             VALUES ($1, 'com.example.' || ($2::int)::text, '\\x00'::bytea)",
        )
        .bind(entity_id)
        .bind(index)
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("component {index} within limit must insert: {error}"));
    }
    let overflow = sqlx::query(
        "INSERT INTO persistent_entity_components (entity_id, component_key, payload)
         VALUES ($1, 'com.example.overflow', '\\x00'::bytea)",
    )
    .bind(entity_id)
    .execute(&pool)
    .await;
    assert!(overflow.is_err(), "the 17th component must be rejected");

    // Free two slots so the remaining rejections are attributable to their own
    // CHECK constraints instead of the count trigger.
    sqlx::query("DELETE FROM persistent_entity_components WHERE entity_id = $1 AND component_key IN ('com.example.0', 'com.example.1')")
        .bind(entity_id)
        .execute(&pool)
        .await
        .expect("remove two components");

    let oversized = sqlx::query(
        "INSERT INTO persistent_entity_components (entity_id, component_key, payload)
         VALUES ($1, repeat('x', 100), repeat('x', 4097))",
    )
    .bind(entity_id)
    .execute(&pool)
    .await;
    assert!(
        oversized.is_err(),
        "payloads above 4096 bytes must be rejected"
    );

    let server_owned = sqlx::query(
        "INSERT INTO persistent_entity_components (entity_id, component_key, payload)
         VALUES ($1, 'core.transform', '\\x00'::bytea)",
    )
    .bind(entity_id)
    .execute(&pool)
    .await;
    assert!(
        server_owned.is_err(),
        "core.* component keys must be rejected"
    );

    cleanup(&pool, instance_id, world_id).await;
}
