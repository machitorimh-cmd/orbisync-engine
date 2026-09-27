//! Persistence port for durable entities (`state-and-runtime.md` §1.2).
//!
//! Persistent entity definitions are saved on spawn/delete and persistent
//! components on update. Writes go through this port so `instance_runtime`
//! never touches the database directly. Ephemeral velocity, animation, and
//! presence state has no representation in this port and must never be
//! persisted (`state-and-runtime.md` §1.1).

use orbisync_domain::{CommandId, Entity, EntityId, InstanceId, Revision, Timestamp, UserId};

use crate::ApplicationError;

/// Stable administrative audit action for an entity ownership hand-off.
pub const AUDIT_ACTION_ENTITY_OWNERSHIP_TRANSFERRED: &str = "entity.ownership_transferred";

/// Port for durable entity rows.
///
/// Implementations map only the durable [`Entity`] fields (kind, owner,
/// transform, visibility, revision, timestamps, and the bounded component
/// map). The ephemeral fields of the aggregate are ignored by contract.
#[async_trait::async_trait]
pub trait PersistentEntityStore: Send + Sync + 'static {
    /// Persists a newly spawned entity together with its components.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ApplicationErrorKind::Conflict`](crate::ApplicationErrorKind::Conflict)
    /// when a row for the entity already exists, and
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable.
    async fn spawn(&self, entity: Entity) -> Result<(), ApplicationError>;

    /// Replaces the durable entity fields (kind, owner, transform,
    /// visibility, revision, timestamps). Components are untouched; use
    /// [`upsert_component`](Self::upsert_component) or
    /// [`delete_component`](Self::delete_component) for those
    /// (`state-and-runtime.md` §1.2: components persist on their own update).
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ApplicationErrorKind::NotFound`](crate::ApplicationErrorKind::NotFound)
    /// when no durable row exists for the entity,
    /// [`ApplicationErrorKind::Conflict`](crate::ApplicationErrorKind::Conflict)
    /// when the durable revision is not older than `entity.revision()`, and
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable.
    async fn update(&self, entity: Entity) -> Result<(), ApplicationError>;

    /// Persists an ownership change and its successful administrative audit
    /// record atomically. Components are left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationErrorKind::NotFound`](crate::ApplicationErrorKind::NotFound)
    /// when no durable row exists for the entity,
    /// [`ApplicationErrorKind::Conflict`](crate::ApplicationErrorKind::Conflict)
    /// when the durable revision is not older than `entity.revision()`, and
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when either the entity update or audit insert cannot be committed.
    async fn transfer_ownership(
        &self,
        entity: Entity,
        audit: EntityOwnershipTransferAudit,
    ) -> Result<(), ApplicationError>;

    /// Inserts or updates a single durable component, advancing the owning
    /// entity's revision and `updated_at` in the same transaction.
    ///
    /// `revision` and `updated_at` are the entity's post-mutation values
    /// (as already applied by [`Entity::update_component`]); the store
    /// persists them and rejects the write if the durable row was not older.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ApplicationErrorKind::NotFound`](crate::ApplicationErrorKind::NotFound)
    /// when no durable row exists for the entity,
    /// [`ApplicationErrorKind::Conflict`](crate::ApplicationErrorKind::Conflict)
    /// when the durable revision is not older than `revision`, and
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable.
    async fn upsert_component(
        &self,
        entity_id: EntityId,
        instance_id: InstanceId,
        revision: Revision,
        updated_at: Timestamp,
        component_key: String,
        payload: Vec<u8>,
    ) -> Result<(), ApplicationError>;

    /// Removes a single durable component, advancing the owning entity's
    /// revision and `updated_at` in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ApplicationErrorKind::NotFound`](crate::ApplicationErrorKind::NotFound)
    /// when no durable entity row exists, or when the entity row exists but
    /// no component row matches `component_key`;
    /// [`ApplicationErrorKind::Conflict`](crate::ApplicationErrorKind::Conflict)
    /// when the durable revision is not older than `revision`, and
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable.
    async fn delete_component(
        &self,
        entity_id: EntityId,
        instance_id: InstanceId,
        revision: Revision,
        updated_at: Timestamp,
        component_key: String,
    ) -> Result<(), ApplicationError>;

    /// Removes a deleted entity and its components, scoped to the owning
    /// instance.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ApplicationErrorKind::NotFound`](crate::ApplicationErrorKind::NotFound)
    /// when no durable row exists for the entity, and
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable.
    async fn delete(
        &self,
        entity_id: EntityId,
        instance_id: InstanceId,
    ) -> Result<(), ApplicationError>;

    /// Reads back every durable entity row (with its components) for
    /// `instance_id`, ordered by no particular guarantee.
    ///
    /// Used to reconcile activation-time restore against rows that are newer
    /// than the latest checkpoint (`state-and-runtime.md` §1.2/§1.3): rows are
    /// written on every persistence-relevant mutation while checkpoints are
    /// only taken periodically, so a crash between checkpoints can leave
    /// durable rows ahead of the last saved checkpoint.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable or a stored row cannot be
    /// decoded back into a valid [`Entity`].
    async fn list_by_instance(
        &self,
        instance_id: InstanceId,
    ) -> Result<Vec<Entity>, ApplicationError>;
}

/// Audit data attached to a durable ownership transfer (ADR-024).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityOwnershipTransferAudit {
    /// Time at which the actor applied the transfer.
    pub occurred_at: Timestamp,
    /// Authenticated user that requested the transfer.
    pub actor_id: UserId,
    /// Realtime idempotency key, used as the audit request correlation ID.
    pub command_id: Option<CommandId>,
    /// Owner before the transfer.
    pub previous_owner: Option<UserId>,
    /// Owner after the transfer.
    pub new_owner: Option<UserId>,
}

/// Batched effect the runtime emits for [`PersistentEntityStore`], drained
/// once per actor tick (`state-and-runtime.md` §1.2, HIGH-002 wiring).
///
/// Distinct from `ExtensionEvent`: that type is the public webhook contract
/// (`extension-mechanism.md`) and does not carry the durable fields (`kind`,
/// `transform`, `visibility`, component payloads) this port needs, and this
/// type must never be widened to serve that public contract either.
#[derive(Debug, Clone, PartialEq)]
pub enum EntityPersistenceEvent {
    /// A new entity was spawned; write its row and its (usually empty at
    /// spawn time) component rows.
    Spawned(Entity),
    /// Ownership changed; update the durable entity and append its audit row
    /// in one adapter transaction.
    OwnershipTransferred {
        /// Full post-transfer durable entity state.
        entity: Entity,
        /// Secret-free audit context for the transfer.
        audit: EntityOwnershipTransferAudit,
    },
    /// An entity was deleted; remove its row and component rows.
    Deleted {
        /// Deleted entity.
        entity_id: EntityId,
        /// Owning instance.
        instance_id: InstanceId,
    },
    /// A single component was inserted or updated. `revision`/`updated_at`
    /// are the entity's post-mutation values, matching
    /// [`PersistentEntityStore::upsert_component`].
    ComponentUpserted {
        /// Owning entity.
        entity_id: EntityId,
        /// Owning instance.
        instance_id: InstanceId,
        /// Entity revision after the mutation that produced this event.
        revision: Revision,
        /// Entity `updated_at` after the mutation that produced this event.
        updated_at: Timestamp,
        /// Component key.
        component_key: String,
        /// Component payload.
        payload: Vec<u8>,
    },
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::{EntityOwnershipTransferAudit, PersistentEntityStore};
    use crate::{ApplicationError, ApplicationErrorKind};
    use orbisync_domain::{
        Entity, EntityId, EntityKind, InstanceId, Revision, Timestamp, VisibilityPolicy,
    };

    /// Test double mirroring the documented spawn/update/delete semantics.
    #[derive(Default)]
    struct InMemoryEntityStore {
        entities: Arc<Mutex<HashMap<EntityId, Entity>>>,
    }

    #[async_trait::async_trait]
    impl PersistentEntityStore for InMemoryEntityStore {
        async fn spawn(&self, entity: Entity) -> Result<(), ApplicationError> {
            let mut entities = Self::lock(&self.entities)?;
            if entities.contains_key(&entity.id()) {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::Conflict,
                    "persistent entity already exists",
                ));
            }
            entities.insert(entity.id(), entity);
            Ok(())
        }

        async fn update(&self, entity: Entity) -> Result<(), ApplicationError> {
            let mut entities = Self::lock(&self.entities)?;
            let Some(durable) = entities.get(&entity.id()) else {
                return Err(Self::not_found());
            };
            if durable.revision().as_u64() >= entity.revision().as_u64() {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::Conflict,
                    "entity revision is not older than the durable row",
                ));
            }
            // Durable fields only; the component map is untouched here and
            // must go through `upsert_component`/`delete_component`.
            let components = durable.components().clone();
            let replaced = Entity::from_persisted(
                entity.id(),
                entity.instance_id(),
                entity.kind(),
                entity.owner(),
                entity.transform(),
                entity.visibility().clone(),
                entity.revision(),
                entity.created_at(),
                entity.updated_at(),
                components,
            )
            .map_err(|_| ApplicationError::port_failure("invalid durable entity state"))?;
            entities.insert(entity.id(), replaced);
            Ok(())
        }

        async fn transfer_ownership(
            &self,
            entity: Entity,
            _audit: EntityOwnershipTransferAudit,
        ) -> Result<(), ApplicationError> {
            self.update(entity).await
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
            let mut entities = Self::lock(&self.entities)?;
            let Some(durable) = entities.get(&entity_id) else {
                return Err(Self::not_found());
            };
            if durable.instance_id() != instance_id {
                return Err(Self::not_found());
            }
            if durable.revision().as_u64() >= revision.as_u64() {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::Conflict,
                    "entity revision is not older than the durable row",
                ));
            }
            let mut components = durable.components().clone();
            components.insert(component_key, payload);
            let replaced = Entity::from_persisted(
                durable.id(),
                durable.instance_id(),
                durable.kind(),
                durable.owner(),
                durable.transform(),
                durable.visibility().clone(),
                revision,
                durable.created_at(),
                updated_at,
                components,
            )
            .map_err(|_| ApplicationError::port_failure("invalid durable entity state"))?;
            entities.insert(entity_id, replaced);
            Ok(())
        }

        async fn delete_component(
            &self,
            entity_id: EntityId,
            instance_id: InstanceId,
            revision: Revision,
            updated_at: Timestamp,
            component_key: String,
        ) -> Result<(), ApplicationError> {
            let mut entities = Self::lock(&self.entities)?;
            let Some(durable) = entities.get(&entity_id) else {
                return Err(Self::not_found());
            };
            if durable.instance_id() != instance_id {
                return Err(Self::not_found());
            }
            if !durable.components().contains_key(&component_key) {
                return Err(Self::not_found());
            }
            if durable.revision().as_u64() >= revision.as_u64() {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::Conflict,
                    "entity revision is not older than the durable row",
                ));
            }
            let mut components = durable.components().clone();
            components.remove(&component_key);
            let replaced = Entity::from_persisted(
                durable.id(),
                durable.instance_id(),
                durable.kind(),
                durable.owner(),
                durable.transform(),
                durable.visibility().clone(),
                revision,
                durable.created_at(),
                updated_at,
                components,
            )
            .map_err(|_| ApplicationError::port_failure("invalid durable entity state"))?;
            entities.insert(entity_id, replaced);
            Ok(())
        }

        async fn delete(
            &self,
            entity_id: EntityId,
            instance_id: InstanceId,
        ) -> Result<(), ApplicationError> {
            let mut entities = Self::lock(&self.entities)?;
            match entities.get(&entity_id) {
                Some(entity) if entity.instance_id() == instance_id => {
                    entities.remove(&entity_id);
                    Ok(())
                }
                _ => Err(Self::not_found()),
            }
        }

        async fn list_by_instance(
            &self,
            instance_id: InstanceId,
        ) -> Result<Vec<Entity>, ApplicationError> {
            let entities = Self::lock(&self.entities)?;
            Ok(entities
                .values()
                .filter(|entity| entity.instance_id() == instance_id)
                .cloned()
                .collect())
        }
    }

    impl InMemoryEntityStore {
        fn lock(
            entities: &Mutex<HashMap<EntityId, Entity>>,
        ) -> Result<std::sync::MutexGuard<'_, HashMap<EntityId, Entity>>, ApplicationError>
        {
            entities
                .lock()
                .map_err(|_| ApplicationError::port_failure("lock poisoned"))
        }

        fn not_found() -> ApplicationError {
            ApplicationError::new(
                ApplicationErrorKind::NotFound,
                "persistent entity does not exist",
            )
        }
    }

    fn entity() -> Entity {
        Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            Timestamp::from_unix_millis(1_000).expect("valid"),
        )
    }

    #[test]
    fn spawn_update_and_delete_follow_the_documented_semantics() {
        let store = InMemoryEntityStore::default();
        let entity = entity();
        pollster::block_on(store.spawn(entity.clone())).expect("spawn");

        let mut updated = entity.clone();
        let revision = updated.revision();
        updated
            .update_component(
                revision,
                "com.example.door".to_owned(),
                vec![1, 2],
                Timestamp::from_unix_millis(2_000).expect("valid"),
            )
            .expect("component update");
        pollster::block_on(store.update(updated.clone())).expect("update");

        pollster::block_on(store.delete(updated.id(), updated.instance_id())).expect("delete");
        assert_eq!(
            pollster::block_on(store.update(updated.clone()))
                .expect_err("a deleted entity cannot update")
                .kind(),
            ApplicationErrorKind::NotFound
        );
    }

    #[test]
    fn duplicate_spawn_and_stale_update_conflict() {
        let store = InMemoryEntityStore::default();
        let mut entity = entity();
        pollster::block_on(store.spawn(entity.clone())).expect("spawn");
        // Advance the durable row to revision 2 so a revision-1 snapshot is stale.
        let revision = entity.revision();
        entity
            .update_component(
                revision,
                "com.example.door".to_owned(),
                vec![1],
                Timestamp::from_unix_millis(2_000).expect("valid"),
            )
            .expect("component update");
        pollster::block_on(store.update(entity.clone())).expect("update");

        assert_eq!(
            pollster::block_on(store.spawn(entity.clone()))
                .expect_err("duplicate spawn must conflict")
                .kind(),
            ApplicationErrorKind::Conflict
        );

        // A lagging writer holds a pre-spawn snapshot of the same entity.
        let stale = Entity::from_persisted(
            entity.id(),
            entity.instance_id(),
            entity.kind(),
            entity.owner(),
            entity.transform(),
            entity.visibility().clone(),
            Revision::from_u64(1),
            entity.created_at(),
            entity.updated_at(),
            HashMap::new(),
        )
        .expect("stale snapshot");
        assert_eq!(
            pollster::block_on(store.update(stale))
                .expect_err("a stale revision must conflict")
                .kind(),
            ApplicationErrorKind::Conflict
        );
    }
}
