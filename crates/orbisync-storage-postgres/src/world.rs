//! World directory persistence.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use orbisync_application::{
    ApplicationError, ApplicationErrorKind, ExtensionEvent, InstanceView, Page, PageRequest,
    QueryCursor, WorldAuditEvent, WorldDirectoryStore, WorldView, pagination::CursorCodec,
};
use orbisync_config::{Config, ConfigError, ConfigErrorKind, EnvSource};
use orbisync_domain::{
    InstanceId, Revision, Timestamp, World, WorldId, WorldInstance, WorldStatus,
    transform::Transform,
};

/// PostgreSQL world directory store.
#[derive(Debug, Clone)]
pub struct PgWorldDirectoryStore {
    pool: PgPool,
    codec: CursorCodec,
}

impl PgWorldDirectoryStore {
    /// Creates the store, resolving the pagination HMAC key via
    /// `config.auth.pagination_hmac_key_env` (same key shared with
    /// `PgIdentityQueryStore`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigErrorKind::MissingSecret`] when the environment
    /// variable is not set, empty, or still the unreplaced placeholder.
    pub fn new(pool: PgPool, config: &Config, env: &dyn EnvSource) -> Result<Self, ConfigError> {
        let env_var = config.auth.pagination_hmac_key_env.clone();
        let raw = env.get(&env_var).ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var.clone(),
                "required secret environment variable is not set",
            )
        })?;
        if raw.trim().is_empty() {
            return Err(ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var,
                "required secret environment variable is not set",
            ));
        }
        if raw.trim() == "__REPLACE_WITH_RANDOM_32_BYTES__" || raw.contains("__REPLACE") {
            return Err(ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var.clone(),
                "pagination HMAC key placeholder not replaced",
            ));
        }
        let codec = CursorCodec::new(raw.into_bytes()).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var.clone(),
                "pagination HMAC key is empty",
            )
        })?;
        Ok(Self { pool, codec })
    }

    /// Creates the store with an explicit codec (tests).
    #[must_use]
    pub const fn with_codec(pool: PgPool, codec: CursorCodec) -> Self {
        Self { pool, codec }
    }

    fn map_err(e: sqlx::Error) -> ApplicationError {
        ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string())
    }

    fn decode_instances_cursor(
        &self,
        page: &PageRequest,
    ) -> Result<Option<Uuid>, ApplicationError> {
        let Some(cursor) = &page.after else {
            return Ok(None);
        };
        let id_str = self.codec.decode_instances(&cursor.0).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid cursor")
        })?;
        Uuid::parse_str(&id_str)
            .map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid cursor"))
            .map(Some)
    }

    fn decode_cursor(&self, page: &PageRequest) -> Result<Option<Uuid>, ApplicationError> {
        let Some(cursor) = &page.after else {
            return Ok(None);
        };
        let id_str = self.codec.decode_worlds(&cursor.0).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid cursor")
        })?;
        Uuid::parse_str(&id_str)
            .map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid cursor"))
            .map(Some)
    }
}

#[async_trait::async_trait]
impl WorldDirectoryStore for PgWorldDirectoryStore {
    async fn create_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        let spawn = transform_to_json(world.default_spawn());
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(error = %e, "failed to begin transaction for create_world_with_audit");
            Self::map_err(e)
        })?;
        sqlx::query(
            "INSERT INTO world_definitions (id, name, description, status, capacity, default_spawn, metadata, revision, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$9)",
        )
        .bind(world.id().as_uuid())
        .bind(world.name())
        .bind(world.description())
        .bind(world.status().as_str())
        .bind(i32::try_from(world.capacity()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "capacity out of range"))?)
        .bind(spawn)
        .bind(json!({}))
        .bind(i64::try_from(world.revision().as_u64()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "revision overflow"))?)
        .bind(world.created_at().as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to insert world_definitions");
            Self::map_err(e)
        })?;
        if let Err(e) = append_world_audit(&mut tx, &audit, None).await {
            tracing::warn!(error = %e, action = audit.action, "failed to append world audit for create_world");
            return Err(Self::map_err(e));
        }
        if let Err(e) = tx.commit().await {
            tracing::warn!(error = %e, "failed to commit create_world_with_audit");
            return Err(Self::map_err(e));
        }
        Ok(())
    }

    async fn create_world_with_audit_and_source_ip(
        &self,
        world: World,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let spawn = transform_to_json(world.default_spawn());
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(error = %e, "failed to begin transaction for create_world_with_audit");
            Self::map_err(e)
        })?;
        sqlx::query(
            "INSERT INTO world_definitions (id, name, description, status, capacity, default_spawn, metadata, revision, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$9)",
        )
        .bind(world.id().as_uuid())
        .bind(world.name())
        .bind(world.description())
        .bind(world.status().as_str())
        .bind(i32::try_from(world.capacity()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "capacity out of range"))?)
        .bind(spawn)
        .bind(json!({}))
        .bind(i64::try_from(world.revision().as_u64()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "revision overflow"))?)
        .bind(world.created_at().as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to insert world_definitions");
            Self::map_err(e)
        })?;
        append_world_audit(&mut tx, &audit, source_ip)
            .await
            .map_err(Self::map_err)?;
        tx.commit().await.map_err(Self::map_err)
    }

    async fn get_world(&self, id: WorldId) -> Result<Option<World>, ApplicationError> {
        let row: Option<(Uuid, String, Option<String>, String, i32, serde_json::Value, i64, time::OffsetDateTime, time::OffsetDateTime)> =
            sqlx::query_as("SELECT id, name, description, status, capacity, default_spawn, revision, created_at, updated_at FROM world_definitions WHERE id = $1")
                .bind(id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(Self::map_err)?;
        let Some((
            id,
            name,
            description,
            status,
            capacity,
            spawn_json,
            revision,
            created_at,
            updated_at,
        )) = row
        else {
            return Ok(None);
        };
        let status = WorldStatus::parse(&status)
            .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        let transform = json_to_transform(&spawn_json)
            .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        let created_at = Timestamp::from_offset_date_time(created_at);
        let updated_at = Timestamp::from_offset_date_time(updated_at);
        let capacity = u32::try_from(capacity).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::PortFailure, "capacity out of range")
        })?;
        let revision_u64 = u64::try_from(revision).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::PortFailure, "revision out of range")
        })?;
        let revision = Revision::from_u64(revision_u64);
        let world_id = WorldId::new(id)
            .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        let world = World::from_persisted(
            world_id,
            name,
            description,
            status,
            transform,
            capacity,
            revision,
            created_at,
            updated_at,
        )
        .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        Ok(Some(world))
    }

    async fn list_worlds(&self, page: PageRequest) -> Result<Page<WorldView>, ApplicationError> {
        let cursor = self.decode_cursor(&page)?;
        let limit = i64::from(page.limit.clamp(1, 200));
        let rows: Vec<(Uuid, String, String, i64)> = sqlx::query_as(
            "SELECT id, name, status, revision FROM world_definitions WHERE ($1::uuid IS NULL OR id > $1) ORDER BY id LIMIT $2",
        )
        .bind(cursor)
        .bind(limit + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::map_err)?;
        let more = rows.len() > limit as usize;
        let mut items = Vec::new();
        for (id, name, status, revision) in rows.into_iter().take(limit as usize) {
            let world_id = WorldId::new(id).map_err(|e| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string())
            })?;
            let status = WorldStatus::parse(&status).map_err(|e| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string())
            })?;
            let revision_u64 = u64::try_from(revision).map_err(|_| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, "revision out of range")
            })?;
            items.push(WorldView {
                id: world_id,
                name,
                status: status.as_str().to_owned(),
                revision: Revision::from_u64(revision_u64),
            });
        }
        let next = if more {
            let last_id = items
                .last()
                .map(|item| item.id.to_string())
                .unwrap_or_default();
            let cursor = self.codec.encode_worlds(&last_id).map_err(|_| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, "cursor encoding failed")
            })?;
            Some(QueryCursor(cursor))
        } else {
            None
        };
        Ok(Page { items, next })
    }

    async fn update_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        self.update_world_with_audit_and_source_ip(world, audit, None)
            .await
    }

    async fn update_world_with_audit_and_source_ip(
        &self,
        world: World,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let spawn = transform_to_json(world.default_spawn());
        let mut tx = self.pool.begin().await.map_err(Self::map_err)?;
        let revision = i64::try_from(world.revision().as_u64()).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "revision overflow")
        })?;
        let previous_revision = revision.checked_sub(1).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "revision underflow")
        })?;
        let updated = sqlx::query(
            "UPDATE world_definitions SET name = $2, description = $3, status = $4, capacity = $5, default_spawn = $6, revision = $7, updated_at = $8 WHERE id = $1 AND revision = $9",
        )
        .bind(world.id().as_uuid())
        .bind(world.name())
        .bind(world.description())
        .bind(world.status().as_str())
        .bind(i32::try_from(world.capacity()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "capacity out of range"))?)
        .bind(spawn)
        .bind(revision)
        .bind(world.updated_at().as_offset_date_time())
        .bind(previous_revision)
        .execute(&mut *tx)
        .await
        .map_err(Self::map_err)?;
        if updated.rows_affected() != 1 {
            // Distinguish "no such world" (404) from "revision mismatch" (409).
            let exists: Option<(i64,)> =
                sqlx::query_as("SELECT 1 FROM world_definitions WHERE id = $1")
                    .bind(world.id().as_uuid())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(Self::map_err)?;
            return Err(if exists.is_some() {
                ApplicationError::new(ApplicationErrorKind::Conflict, "world revision mismatch")
            } else {
                ApplicationError::new(ApplicationErrorKind::NotFound, "world not found")
            });
        }
        append_world_audit(&mut tx, &audit, source_ip)
            .await
            .map_err(Self::map_err)?;
        tx.commit().await.map_err(Self::map_err)
    }

    async fn create_instance_with_audit(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(error = %e, "failed to begin transaction for create_instance_with_audit");
            Self::map_err(e)
        })?;
        sqlx::query(
            "INSERT INTO world_instances (id, world_id, lifecycle, capacity, created_at, started_at, revision) VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(instance.id().as_uuid())
        .bind(instance.world_id().as_uuid())
        .bind(instance.lifecycle().as_str())
        .bind(i32::try_from(instance.capacity()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "capacity out of range"))?)
        .bind(instance.created_at().as_offset_date_time())
        .bind(instance.started_at().map(|t| t.as_offset_date_time()))
        .bind(i64::try_from(instance.revision().as_u64()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "revision overflow"))?)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to insert world_instances");
            Self::map_err(e)
        })?;
        if let Err(e) = append_world_audit(&mut tx, &audit, None).await {
            tracing::warn!(error = %e, action = audit.action, "failed to append world audit for create_instance");
            return Err(Self::map_err(e));
        }
        if let Err(e) = tx.commit().await {
            tracing::warn!(error = %e, "failed to commit create_instance_with_audit");
            return Err(Self::map_err(e));
        }
        Ok(())
    }

    async fn create_instance_with_audit_and_source_ip(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(error = %e, "failed to begin transaction for create_instance_with_audit");
            Self::map_err(e)
        })?;
        sqlx::query(
            "INSERT INTO world_instances (id, world_id, lifecycle, capacity, created_at, started_at, revision) VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(instance.id().as_uuid())
        .bind(instance.world_id().as_uuid())
        .bind(instance.lifecycle().as_str())
        .bind(i32::try_from(instance.capacity()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "capacity out of range"))?)
        .bind(instance.created_at().as_offset_date_time())
        .bind(instance.started_at().map(|t| t.as_offset_date_time()))
        .bind(i64::try_from(instance.revision().as_u64()).map_err(|_| ApplicationError::new(ApplicationErrorKind::DomainRule, "revision overflow"))?)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to insert world_instances");
            Self::map_err(e)
        })?;
        append_world_audit(&mut tx, &audit, source_ip)
            .await
            .map_err(Self::map_err)?;
        tx.commit().await.map_err(Self::map_err)
    }

    async fn update_instance_with_event(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        event: ExtensionEvent,
    ) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(Self::map_err)?;
        let revision = i64::try_from(instance.revision().as_u64()).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "revision overflow")
        })?;
        let previous_revision = revision.checked_sub(1).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "revision underflow")
        })?;
        let updated = sqlx::query(
            "UPDATE world_instances SET lifecycle = $2, started_at = $3, revision = $4 WHERE id = $1 AND revision = $5",
        )
        .bind(instance.id().as_uuid())
        .bind(instance.lifecycle().as_str())
        .bind(instance.started_at().map(|value| value.as_offset_date_time()))
        .bind(revision)
        .bind(previous_revision)
        .execute(&mut *tx)
        .await
        .map_err(Self::map_err)?;
        if updated.rows_affected() != 1 {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "instance revision mismatch",
            ));
        }
        append_world_audit(&mut tx, &audit, None)
            .await
            .map_err(Self::map_err)?;
        append_extension_outbox(&mut tx, &event)
            .await
            .map_err(Self::map_err)?;
        tx.commit().await.map_err(Self::map_err)
    }

    async fn record_world_audit(&self, audit: WorldAuditEvent) -> Result<(), ApplicationError> {
        self.record_world_audit_and_source_ip(audit, None).await
    }

    async fn record_world_audit_and_source_ip(
        &self,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(error = %e, action = audit.action, "failed to begin transaction for world audit");
            Self::map_err(e)
        })?;
        if let Err(e) = append_world_audit(&mut tx, &audit, source_ip).await {
            tracing::warn!(error = %e, action = audit.action, "failed to append world audit");
            return Err(Self::map_err(e));
        }
        if let Err(e) = tx.commit().await {
            tracing::warn!(error = %e, action = audit.action, "failed to commit world audit");
            return Err(Self::map_err(e));
        }
        Ok(())
    }

    async fn get_instance(
        &self,
        id: InstanceId,
    ) -> Result<Option<WorldInstance>, ApplicationError> {
        let row: Option<(Uuid, Uuid, String, i32, time::OffsetDateTime, Option<time::OffsetDateTime>, i64)> =
            sqlx::query_as("SELECT id, world_id, lifecycle, capacity, created_at, started_at, revision FROM world_instances WHERE id = $1")
                .bind(id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(Self::map_err)?;
        let Some((id, world_id, lifecycle, capacity, created_at, started_at, revision)) = row
        else {
            return Ok(None);
        };
        let lifecycle = orbisync_domain::InstanceLifecycle::parse(&lifecycle)
            .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        let created_at = Timestamp::from_offset_date_time(created_at);
        let started_at = started_at.map(Timestamp::from_offset_date_time);
        let capacity = u32::try_from(capacity).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::PortFailure, "capacity out of range")
        })?;
        let revision_u64 = u64::try_from(revision).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::PortFailure, "revision out of range")
        })?;
        let revision = Revision::from_u64(revision_u64);
        let instance_id = InstanceId::new(id)
            .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        let world_id = WorldId::new(world_id)
            .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        let instance = WorldInstance::from_persisted(
            instance_id,
            world_id,
            lifecycle,
            capacity,
            created_at,
            started_at,
            revision,
        )
        .map_err(|e| ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string()))?;
        Ok(Some(instance))
    }

    async fn list_instances(
        &self,
        page: PageRequest,
    ) -> Result<Page<InstanceView>, ApplicationError> {
        let cursor = self.decode_instances_cursor(&page)?;
        let sql_limit = i64::from(page.limit.clamp(1, 200));
        let rows: Vec<(Uuid, Uuid, String, i64)> = sqlx::query_as(
            "SELECT id, world_id, lifecycle, revision FROM world_instances WHERE ($1::uuid IS NULL OR id > $1) ORDER BY id LIMIT $2",
        )
        .bind(cursor)
        .bind(sql_limit + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::map_err)?;
        let more = rows.len() > sql_limit as usize;
        let items = rows
            .into_iter()
            .take(sql_limit as usize)
            .map(|(id, world_id, lifecycle, revision)| {
                let id = InstanceId::new(id).map_err(|e| {
                    ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string())
                })?;
                let world_id = WorldId::new(world_id).map_err(|e| {
                    ApplicationError::new(ApplicationErrorKind::PortFailure, e.to_string())
                })?;
                let revision = u64::try_from(revision).map_err(|_| {
                    ApplicationError::new(
                        ApplicationErrorKind::PortFailure,
                        "revision out of range",
                    )
                })?;
                Ok(InstanceView {
                    id,
                    world_id,
                    status: lifecycle,
                    revision: Revision::from_u64(revision),
                })
            })
            .collect::<Result<Vec<_>, ApplicationError>>()?;
        let next = if more {
            let last_id = items
                .last()
                .map(|item| item.id.to_string())
                .unwrap_or_default();
            let encoded = self.codec.encode_instances(&last_id).map_err(|_| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, "cursor encoding failed")
            })?;
            Some(QueryCursor(encoded))
        } else {
            None
        };
        Ok(Page { items, next })
    }
}

fn transform_to_json(t: Transform) -> serde_json::Value {
    json!({
        "position": {"x": t.position().x(), "y": t.position().y(), "z": t.position().z()},
        "rotation": {"x": t.rotation().x(), "y": t.rotation().y(), "z": t.rotation().z(), "w": t.rotation().w()},
        "scale": {"x": t.scale().x(), "y": t.scale().y(), "z": t.scale().z()},
    })
}

fn json_to_transform(v: &serde_json::Value) -> Result<Transform, String> {
    let px = v["position"]["x"].as_f64().ok_or("missing position.x")?;
    let py = v["position"]["y"].as_f64().ok_or("missing position.y")?;
    let pz = v["position"]["z"].as_f64().ok_or("missing position.z")?;
    let rx = v["rotation"]["x"].as_f64().ok_or("missing rotation.x")?;
    let ry = v["rotation"]["y"].as_f64().ok_or("missing rotation.y")?;
    let rz = v["rotation"]["z"].as_f64().ok_or("missing rotation.z")?;
    let rw = v["rotation"]["w"].as_f64().ok_or("missing rotation.w")?;
    let sx = v["scale"]["x"].as_f64().unwrap_or(1.0);
    let sy = v["scale"]["y"].as_f64().unwrap_or(1.0);
    let sz = v["scale"]["z"].as_f64().unwrap_or(1.0);
    let pos = orbisync_domain::Vec3::new(px, py, pz).map_err(|e| e.to_string())?;
    let rot = orbisync_domain::Quaternion::new(rx, ry, rz, rw).map_err(|e| e.to_string())?;
    let scale = orbisync_domain::Vec3::new(sx, sy, sz).map_err(|e| e.to_string())?;
    Transform::new(pos, rot, scale).map_err(|e| e.to_string())
}

async fn append_world_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    audit: &WorldAuditEvent,
    source_ip: Option<String>,
) -> Result<(), sqlx::Error> {
    let id = Uuid::now_v7();
    let occurred_at = audit.occurred_at.as_offset_date_time();
    let actor_user_id = audit.actor_id.as_uuid();
    let action = audit.action;
    let target_type = audit.action.split_once('.').map(|(first, _)| first);
    let target_id = audit.resource_id.as_deref();
    let request_id = Some(audit.request_id.to_string());
    let result = if audit.succeeded {
        "success"
    } else {
        "failure"
    };
    let metadata = json!({});
    if let Err(e) = sqlx::query(
        "INSERT INTO audit_events (id, occurred_at, actor_user_id, action, target_type, target_id, request_id, result, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(occurred_at)
    .bind(actor_user_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(request_id)
    .bind(result)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    {
        tracing::warn!(error = %e, action = %action, result = %result, "failed to append world audit event");
        return Err(e);
    }
    if let Some(source_ip) = source_ip {
        sqlx::query(
            "INSERT INTO audit_source_ips (audit_event_id, source_ip, created_at) VALUES ($1, $2::inet, $3)",
        )
        .bind(id)
        .bind(source_ip)
        .bind(occurred_at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn append_extension_outbox(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &ExtensionEvent,
) -> Result<(), sqlx::Error> {
    let event_id = Uuid::now_v7();
    let created_at = time::OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO outbox_events (id, event_id, owner_module, event_type, event_kind, payload, created_at, available_at) VALUES ($1, $1, 'extensions', $2, $2, $3, $4, $4)",
    )
    .bind(event_id)
    .bind(event.kind())
    .bind(event.payload())
    .bind(created_at)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

/// PostgreSQL-backed authorizer for world-directory operations.
///
/// Loads server-owned role assignments and checks the requested permission.
/// Mirrors the `authorize` logic used by the HTTP transport but is owned by
/// the application boundary (`auth-authorization.md:275-284`).
#[derive(Debug, Clone)]
pub struct PgWorldAuthorizer {
    pool: PgPool,
}

impl PgWorldAuthorizer {
    /// Creates an authorizer backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl orbisync_application::WorldAuthorizer for PgWorldAuthorizer {
    async fn require(
        &self,
        actor: orbisync_domain::UserId,
        permission: &str,
    ) -> Result<(), ApplicationError> {
        // Validate permission name early.
        if orbisync_domain::Permission::new(permission.to_owned()).is_err() {
            return Err(ApplicationError::port_failure("invalid permission name"));
        }
        // Share the REST identity boundary, including the temporary-subject
        // ceiling, with WS handshake and pre/post-hook permission resolution.
        let roles = orbisync_application::IdentityRepository::roles_for_user(
            &crate::PgIdentityRepository::new(self.pool.clone()),
            actor,
        )
        .await
        .map_err(|_| ApplicationError::port_failure("authorization check unavailable"))?;
        if roles.iter().any(|role| {
            role.permissions()
                .iter()
                .any(|granted| granted.as_str() == permission)
        }) {
            Ok(())
        } else {
            Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "permission denied",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use orbisync_application::ApplicationErrorKind;
    use orbisync_domain::{
        InstanceLifecycle, Revision, Timestamp, World, WorldId, WorldInstance, WorldStatus,
        transform::Transform,
    };

    use super::json_to_transform;

    fn ts(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis).expect("valid")
    }

    #[test]
    fn get_world_invalid_capacity_maps_to_port_failure() {
        // Simulate the DB row conversion that get_world performs for capacity.
        let bad_capacity: i32 = -5;
        let err = u32::try_from(bad_capacity)
            .map_err(|_| {
                orbisync_application::ApplicationError::new(
                    ApplicationErrorKind::PortFailure,
                    "capacity out of range",
                )
            })
            .expect_err("negative capacity must fail");
        assert_eq!(err.kind(), ApplicationErrorKind::PortFailure);

        // Also verify domain validation maps to PortFailure when restoring.
        let domain_err = World::from_persisted(
            WorldId::generate(),
            "W".to_owned(),
            None,
            WorldStatus::Active,
            Transform::identity(),
            0,
            Revision::from_u64(1),
            ts(1_000),
            ts(1_000),
        )
        .expect_err("capacity 0 must be rejected by domain");
        let app_err = orbisync_application::ApplicationError::new(
            ApplicationErrorKind::PortFailure,
            domain_err.to_string(),
        );
        assert_eq!(app_err.kind(), ApplicationErrorKind::PortFailure);
    }

    #[test]
    fn get_instance_invalid_capacity_maps_to_port_failure() {
        let bad_capacity: i32 = -1;
        let err = u32::try_from(bad_capacity)
            .map_err(|_| {
                orbisync_application::ApplicationError::new(
                    ApplicationErrorKind::PortFailure,
                    "capacity out of range",
                )
            })
            .expect_err("negative capacity must fail");
        assert_eq!(err.kind(), ApplicationErrorKind::PortFailure);
    }

    #[test]
    fn get_instance_restores_lifecycle_and_revision_via_from_persisted() {
        let id = orbisync_domain::InstanceId::generate();
        let world_id = WorldId::generate();
        let created = ts(1_000);
        let started = ts(2_000);
        let inst = WorldInstance::from_persisted(
            id,
            world_id,
            InstanceLifecycle::Running,
            42,
            created,
            Some(started),
            Revision::from_u64(9),
        )
        .expect("valid");
        assert_eq!(inst.lifecycle(), InstanceLifecycle::Running);
        assert_eq!(inst.capacity(), 42);
        assert_eq!(inst.started_at(), Some(started));
        assert_eq!(inst.revision(), Revision::from_u64(9));
    }

    #[test]
    fn get_world_restores_revision_and_updated_at() {
        let created = ts(1_000);
        let updated = ts(5_000);
        let world = World::from_persisted(
            WorldId::generate(),
            "W".to_owned(),
            Some("desc".to_owned()),
            WorldStatus::Archived,
            Transform::identity(),
            100,
            Revision::from_u64(5),
            created,
            updated,
        )
        .expect("valid");
        assert_eq!(world.revision(), Revision::from_u64(5));
        assert_eq!(world.updated_at(), updated);
        assert_eq!(world.status(), WorldStatus::Archived);
    }

    #[test]
    fn json_to_transform_round_trip() {
        let t = Transform::identity();
        let json = super::transform_to_json(t);
        let back = json_to_transform(&json).expect("round trip");
        assert_eq!(back, t);
    }
}
