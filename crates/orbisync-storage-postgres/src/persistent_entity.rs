//! Persistent entity rows for `instance_runtime` (`state-and-runtime.md`
//! §1.2).
//!
//! Persists the durable fields of the [`Entity`] aggregate as rows in
//! `persistent_entities` and its components in
//! `persistent_entity_components`. Ephemeral velocity, animation, and presence
//! state is intentionally not mapped (state-and-runtime.md §1.1). The
//! checkpoint store coexists with these rows unchanged.

use std::collections::HashMap;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use orbisync_application::{
    AUDIT_ACTION_ENTITY_OWNERSHIP_TRANSFERRED, ApplicationError, ApplicationErrorKind,
    EntityOwnershipTransferAudit, PersistentEntityStore,
};
use orbisync_domain::{
    Entity, EntityId, EntityKind, InstanceId, Quaternion, Revision, RoleId, Timestamp, Transform,
    UserId, Vec3, VisibilityPolicy,
};

/// PostgreSQL store for persistent entities and their components.
#[derive(Debug, Clone)]
pub struct PgPersistentEntityStore {
    pool: PgPool,
    writer: Option<orbisync_application::checkpoint_admission::WriterPermit>,
}

impl PgPersistentEntityStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool, writer: None }
    }

    /// Generation reader bound to the same startup capability as activation.
    pub fn with_writer(
        pool: PgPool,
        writer: orbisync_application::checkpoint_admission::WriterPermit,
    ) -> Self {
        Self {
            pool,
            writer: Some(writer),
        }
    }

    pub(crate) async fn list_in_transaction(
        instance_id: InstanceId,
        tx: &mut sqlx::PgConnection,
    ) -> Result<Vec<Entity>, ApplicationError> {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            Uuid,
            Uuid,
            String,
            Option<Uuid>,
            Option<serde_json::Value>,
            serde_json::Value,
            i64,
            OffsetDateTime,
            OffsetDateTime,
        )> = sqlx::query_as(
            "SELECT id, instance_id, kind, owner_id, transform, visibility, revision, \
                    created_at, updated_at \
             FROM persistent_entities WHERE instance_id = $1",
        )
        .bind(instance_id.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        crate::generation_execution::check()?;
        let component_rows: Vec<(Uuid, String, Vec<u8>)> = sqlx::query_as(
            "SELECT c.entity_id, c.component_key, c.payload \
             FROM persistent_entity_components c \
             JOIN persistent_entities e ON e.id = c.entity_id \
             WHERE e.instance_id = $1",
        )
        .bind(instance_id.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        tokio::task::spawn_blocking(move || {
            let mut components_by_entity: HashMap<Uuid, HashMap<String, Vec<u8>>> = HashMap::new();
            for (entity_id, key, payload) in component_rows {
                components_by_entity
                    .entry(entity_id)
                    .or_default()
                    .insert(key, payload);
            }

            let mut entities = Vec::with_capacity(rows.len());
            for (
                id,
                row_instance_id,
                kind,
                owner_id,
                transform,
                visibility,
                revision,
                created_at,
                updated_at,
            ) in rows
            {
                let entity_id =
                    EntityId::new(id).map_err(|error| Self::decode_error(error.to_string()))?;
                let row_instance_id = InstanceId::new(row_instance_id)
                    .map_err(|error| Self::decode_error(error.to_string()))?;
                let kind = EntityKind::parse(&kind)
                    .map_err(|error| Self::decode_error(error.to_string()))?;
                let owner = owner_id
                    .map(UserId::new)
                    .transpose()
                    .map_err(|error| Self::decode_error(error.to_string()))?;
                let transform = Self::parse_transform(transform)?;
                let visibility = Self::parse_visibility(visibility)?;
                let revision = u64::try_from(revision)
                    .map(Revision::from_u64)
                    .map_err(|_| Self::decode_error("revision out of range"))?;
                let created_at = Timestamp::from_offset_date_time(created_at);
                let updated_at = Timestamp::from_offset_date_time(updated_at);
                let components = components_by_entity.remove(&id).unwrap_or_default();

                let entity = Entity::from_persisted(
                    entity_id,
                    row_instance_id,
                    kind,
                    owner,
                    transform,
                    visibility,
                    revision,
                    created_at,
                    updated_at,
                    components,
                )
                .map_err(|error| Self::decode_error(error.to_string()))?;
                entities.push(entity);
            }
            Ok(entities)
        })
        .await
        .map_err(|_| ApplicationError::port_failure("inventory decoding unavailable"))?
    }

    fn map_db_err(error: sqlx::Error) -> ApplicationError {
        ApplicationError::port_failure(error.to_string())
    }

    fn conflict(detail: &'static str) -> ApplicationError {
        ApplicationError::new(ApplicationErrorKind::Conflict, detail)
    }

    fn not_found() -> ApplicationError {
        ApplicationError::new(
            ApplicationErrorKind::NotFound,
            "persistent entity does not exist",
        )
    }

    fn revision_i64(revision: Revision) -> Result<i64, ApplicationError> {
        i64::try_from(revision.as_u64()).map_err(|_| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, "revision out of range")
        })
    }

    /// Advances the durable entity's `revision`/`updated_at` inside `tx`,
    /// rejecting the write if the durable row is not strictly older.
    /// Shared by [`update`](Self::update) and the component operations, all
    /// of which persist a revision the caller already advanced in memory.
    async fn advance_revision(
        tx: &mut sqlx::PgConnection,
        entity_id: EntityId,
        instance_id: InstanceId,
        revision: i64,
        updated_at: OffsetDateTime,
    ) -> Result<(), ApplicationError> {
        let updated = sqlx::query(
            "UPDATE persistent_entities SET revision = $3, updated_at = $4 \
             WHERE id = $1 AND instance_id = $2 AND revision < $3",
        )
        .bind(entity_id.as_uuid())
        .bind(instance_id.as_uuid())
        .bind(revision)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        if updated.rows_affected() == 0 {
            let durable_revision: Option<i64> =
                sqlx::query_scalar("SELECT revision FROM persistent_entities WHERE id = $1")
                    .bind(entity_id.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(Self::map_db_err)?;
            return match durable_revision {
                None => Err(Self::not_found()),
                Some(durable) if durable >= revision => Err(Self::conflict(
                    "entity revision is not older than the durable row",
                )),
                Some(_) => Err(Self::conflict(
                    "persistent entity does not belong to the instance",
                )),
            };
        }
        Ok(())
    }

    /// Replaces durable entity fields under the optimistic revision guard.
    /// The caller owns the transaction so an accompanying audit insert can be
    /// committed atomically for ownership changes.
    async fn update_entity_in_transaction(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        entity: &Entity,
    ) -> Result<(), ApplicationError> {
        let revision = Self::revision_i64(entity.revision())?;
        let updated = sqlx::query(
            "UPDATE persistent_entities \
             SET kind = $3, owner_id = $4, transform = $5, visibility = $6, \
                 revision = $7, created_at = $8, updated_at = $9 \
             WHERE id = $1 AND instance_id = $2 AND revision < $7",
        )
        .bind(entity.id().as_uuid())
        .bind(entity.instance_id().as_uuid())
        .bind(entity.kind().as_str())
        .bind(entity.owner().map(|owner| owner.as_uuid()))
        .bind(entity.transform().map(Self::transform_json))
        .bind(Self::visibility_json(entity.visibility()))
        .bind(revision)
        .bind(entity.created_at().as_offset_date_time())
        .bind(entity.updated_at().as_offset_date_time())
        .execute(&mut **tx)
        .await
        .map_err(Self::map_db_err)?;

        if updated.rows_affected() == 0 {
            let durable_revision: Option<i64> =
                sqlx::query_scalar("SELECT revision FROM persistent_entities WHERE id = $1")
                    .bind(entity.id().as_uuid())
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(Self::map_db_err)?;
            return match durable_revision {
                None => Err(Self::not_found()),
                Some(durable) if durable >= revision => Err(Self::conflict(
                    "entity revision is not older than the durable row",
                )),
                Some(_) => Err(Self::conflict(
                    "persistent entity does not belong to the instance",
                )),
            };
        }
        Ok(())
    }

    /// Canonical JSONB shape; mirrors the durable checkpoint representation.
    pub(crate) fn transform_json(transform: Transform) -> serde_json::Value {
        serde_json::json!({
            "position": {
                "x": transform.position().x(),
                "y": transform.position().y(),
                "z": transform.position().z(),
            },
            "rotation": {
                "x": transform.rotation().x(),
                "y": transform.rotation().y(),
                "z": transform.rotation().z(),
                "w": transform.rotation().w(),
            },
            "scale": {
                "x": transform.scale().x(),
                "y": transform.scale().y(),
                "z": transform.scale().z(),
            },
        })
    }

    /// Canonical JSONB shape; mirrors the durable checkpoint representation.
    pub(crate) fn visibility_json(visibility: &VisibilityPolicy) -> serde_json::Value {
        match visibility {
            VisibilityPolicy::Global => serde_json::json!({ "type": "global" }),
            VisibilityPolicy::Spatial { radius } => {
                serde_json::json!({ "type": "spatial", "radius": radius })
            }
            VisibilityPolicy::OwnerOnly => serde_json::json!({ "type": "owner_only" }),
            VisibilityPolicy::RoleRestricted { roles } => serde_json::json!({
                "type": "role_restricted",
                "roles": roles.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
            VisibilityPolicy::Explicit { users } => serde_json::json!({
                "type": "explicit",
                "users": users.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
            VisibilityPolicy::Custom { tag } => {
                serde_json::json!({ "type": "custom", "tag": tag.as_str() })
            }
        }
    }

    fn decode_error(detail: impl Into<String>) -> ApplicationError {
        ApplicationError::port_failure(format!("corrupt persistent entity row: {}", detail.into()))
    }

    fn json_f32(value: &serde_json::Value, field: &str) -> Result<f32, ApplicationError> {
        value
            .get(field)
            .and_then(serde_json::Value::as_f64)
            .map(|v| v as f32)
            .ok_or_else(|| Self::decode_error(format!("missing or non-numeric field {field}")))
    }

    /// Reverses [`Self::transform_json`].
    fn parse_transform(
        value: Option<serde_json::Value>,
    ) -> Result<Option<Transform>, ApplicationError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let position = value
            .get("position")
            .ok_or_else(|| Self::decode_error("transform missing position"))?;
        let rotation = value
            .get("rotation")
            .ok_or_else(|| Self::decode_error("transform missing rotation"))?;
        let scale = value
            .get("scale")
            .ok_or_else(|| Self::decode_error("transform missing scale"))?;
        let position = Vec3::new(
            Self::json_f32(position, "x")?,
            Self::json_f32(position, "y")?,
            Self::json_f32(position, "z")?,
        )
        .map_err(|error| Self::decode_error(error.to_string()))?;
        let rotation = Quaternion::new(
            Self::json_f32(rotation, "x")?,
            Self::json_f32(rotation, "y")?,
            Self::json_f32(rotation, "z")?,
            Self::json_f32(rotation, "w")?,
        )
        .map_err(|error| Self::decode_error(error.to_string()))?;
        let scale = Vec3::new(
            Self::json_f32(scale, "x")?,
            Self::json_f32(scale, "y")?,
            Self::json_f32(scale, "z")?,
        )
        .map_err(|error| Self::decode_error(error.to_string()))?;
        let transform = Transform::new(position, rotation, scale)
            .map_err(|error| Self::decode_error(error.to_string()))?;
        Ok(Some(transform))
    }

    /// Reverses [`Self::visibility_json`].
    fn parse_visibility(value: serde_json::Value) -> Result<VisibilityPolicy, ApplicationError> {
        let policy_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Self::decode_error("visibility missing type"))?;
        match policy_type {
            "global" => Ok(VisibilityPolicy::Global),
            "spatial" => {
                let radius = Self::json_f32(&value, "radius")?;
                VisibilityPolicy::spatial(radius)
                    .map_err(|error| Self::decode_error(error.to_string()))
            }
            "owner_only" => Ok(VisibilityPolicy::OwnerOnly),
            "role_restricted" => {
                let roles = value
                    .get("roles")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| Self::decode_error("role_restricted missing roles"))?
                    .iter()
                    .map(|role| {
                        role.as_str()
                            .ok_or_else(|| Self::decode_error("role entry is not a string"))
                            .and_then(|s| {
                                RoleId::parse(s)
                                    .map_err(|error| Self::decode_error(error.to_string()))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                VisibilityPolicy::role_restricted(roles)
                    .map_err(|error| Self::decode_error(error.to_string()))
            }
            "explicit" => {
                let users = value
                    .get("users")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| Self::decode_error("explicit missing users"))?
                    .iter()
                    .map(|user| {
                        user.as_str()
                            .ok_or_else(|| Self::decode_error("user entry is not a string"))
                            .and_then(|s| {
                                UserId::parse(s)
                                    .map_err(|error| Self::decode_error(error.to_string()))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                VisibilityPolicy::explicit(users)
                    .map_err(|error| Self::decode_error(error.to_string()))
            }
            "custom" => {
                let tag = value
                    .get("tag")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| Self::decode_error("custom missing tag"))?;
                VisibilityPolicy::custom(tag).map_err(|error| Self::decode_error(error.to_string()))
            }
            other => Err(Self::decode_error(format!(
                "unknown visibility type {other}"
            ))),
        }
    }

    /// Inserts every component of a newly spawned entity. Only valid for a
    /// brand-new row: it does not clear existing component rows first.
    async fn insert_components(
        entity: &Entity,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), ApplicationError> {
        for (key, payload) in entity.components() {
            sqlx::query(
                "INSERT INTO persistent_entity_components (entity_id, component_key, payload) \
                 VALUES ($1, $2, $3)",
            )
            .bind(entity.id().as_uuid())
            .bind(key)
            .bind(payload)
            .execute(&mut **tx)
            .await
            .map_err(Self::map_db_err)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl PersistentEntityStore for PgPersistentEntityStore {
    async fn spawn(&self, entity: Entity) -> Result<(), ApplicationError> {
        let revision = Self::revision_i64(entity.revision())?;

        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        let inserted = sqlx::query(
            "INSERT INTO persistent_entities \
             (id, instance_id, kind, owner_id, transform, visibility, revision, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(entity.id().as_uuid())
        .bind(entity.instance_id().as_uuid())
        .bind(entity.kind().as_str())
        .bind(entity.owner().map(|owner| owner.as_uuid()))
        .bind(entity.transform().map(Self::transform_json))
        .bind(Self::visibility_json(entity.visibility()))
        .bind(revision)
        .bind(entity.created_at().as_offset_date_time())
        .bind(entity.updated_at().as_offset_date_time())
        .execute(&mut *tx)
        .await;

        if let Err(error) = inserted {
            if let sqlx::Error::Database(db) = &error
                && db.is_unique_violation()
            {
                return Err(Self::conflict("persistent entity already exists"));
            }
            return Err(Self::map_db_err(error));
        }

        Self::insert_components(&entity, &mut tx).await?;
        tx.commit().await.map_err(Self::map_db_err)
    }

    async fn update(&self, entity: Entity) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        Self::update_entity_in_transaction(&mut tx, &entity).await?;
        tx.commit().await.map_err(Self::map_db_err)
    }

    async fn transfer_ownership(
        &self,
        entity: Entity,
        audit: EntityOwnershipTransferAudit,
    ) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        Self::update_entity_in_transaction(&mut tx, &entity).await?;

        let metadata = serde_json::json!({
            "instance_id": entity.instance_id().to_string(),
            "previous_owner_id": audit.previous_owner.map(|owner| owner.to_string()),
            "new_owner_id": audit.new_owner.map(|owner| owner.to_string()),
        });
        let request_id = audit.command_id.map(|id| format!("req_{id}"));
        sqlx::query(
            "INSERT INTO audit_events \
             (id, occurred_at, actor_user_id, action, target_type, target_id, request_id, result, metadata) \
             VALUES ($1, $2, $3, $4, 'entity', $5, $6, 'success', $7)",
        )
        .bind(Uuid::now_v7())
        .bind(audit.occurred_at.as_offset_date_time())
        .bind(audit.actor_id.as_uuid())
        .bind(AUDIT_ACTION_ENTITY_OWNERSHIP_TRANSFERRED)
        .bind(entity.id().to_string())
        .bind(request_id)
        .bind(metadata)
        .execute(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        tx.commit().await.map_err(Self::map_db_err)
    }

    async fn upsert_component(
        &self,
        entity_id: EntityId,
        instance_id: InstanceId,
        revision: Revision,
        updated_at: Timestamp,
        component_key: String,
        payload: Vec<u8>,
    ) -> Result<(), ApplicationError> {
        let revision_i64 = Self::revision_i64(revision)?;

        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        Self::advance_revision(
            &mut tx,
            entity_id,
            instance_id,
            revision_i64,
            updated_at.as_offset_date_time(),
        )
        .await?;

        sqlx::query(
            "INSERT INTO persistent_entity_components (entity_id, component_key, payload) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (entity_id, component_key) DO UPDATE SET payload = EXCLUDED.payload",
        )
        .bind(entity_id.as_uuid())
        .bind(&component_key)
        .bind(&payload)
        .execute(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        tx.commit().await.map_err(Self::map_db_err)
    }

    async fn delete_component(
        &self,
        entity_id: EntityId,
        instance_id: InstanceId,
        revision: Revision,
        updated_at: Timestamp,
        component_key: String,
    ) -> Result<(), ApplicationError> {
        let revision_i64 = Self::revision_i64(revision)?;

        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        Self::advance_revision(
            &mut tx,
            entity_id,
            instance_id,
            revision_i64,
            updated_at.as_offset_date_time(),
        )
        .await?;

        let deleted = sqlx::query(
            "DELETE FROM persistent_entity_components WHERE entity_id = $1 AND component_key = $2",
        )
        .bind(entity_id.as_uuid())
        .bind(&component_key)
        .execute(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        if deleted.rows_affected() == 0 {
            return Err(Self::not_found());
        }

        tx.commit().await.map_err(Self::map_db_err)
    }

    async fn delete(
        &self,
        entity_id: EntityId,
        instance_id: InstanceId,
    ) -> Result<(), ApplicationError> {
        let deleted =
            sqlx::query("DELETE FROM persistent_entities WHERE id = $1 AND instance_id = $2")
                .bind(entity_id.as_uuid())
                .bind(instance_id.as_uuid())
                .execute(&self.pool)
                .await
                .map_err(Self::map_db_err)?;

        if deleted.rows_affected() == 0 {
            return Err(Self::not_found());
        }
        Ok(())
    }

    async fn list_by_instance(
        &self,
        instance_id: InstanceId,
    ) -> Result<Vec<Entity>, ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(Self::map_db_err)?;
        let token = self.writer.as_ref().map(|writer| writer.token());
        let readable: bool = sqlx::query_scalar("SELECT checkpoint_projection_readable($1,$2,$3)")
            .bind(instance_id.as_uuid())
            .bind(token.map(|t| t.epoch))
            .bind(token.map(|t| t.boot))
            .fetch_one(&mut *tx)
            .await
            .map_err(Self::map_db_err)?;
        if !readable || self.writer.as_ref().is_some_and(|writer| !writer.is_live()) {
            return Err(ApplicationError::port_failure(
                "generation projection unavailable",
            ));
        }
        let entities = Self::list_in_transaction(instance_id, &mut tx).await?;
        tx.commit().await.map_err(Self::map_db_err)?;
        if self.writer.as_ref().is_some_and(|writer| !writer.is_live()) {
            return Err(ApplicationError::port_failure("generation writer lost"));
        }
        Ok(entities)
    }
}
