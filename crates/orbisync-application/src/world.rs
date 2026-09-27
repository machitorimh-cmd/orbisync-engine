//! World and instance administration use cases.
//!
//! # Secure-by-default construction
//!
//! The use case **requires** an authorizer at construction time. There is no
//! `new(store)` that silently allows every request. The following does **not**
//! compile:
//!
//! ```compile_fail
//! use orbisync_application::{WorldDirectoryUseCase, WorldDirectoryStore};
//! use orbisync_testkit::FakeWorldDirectoryStore;
//! // missing authorizer -> compile error (secure-by-default)
//! let store = FakeWorldDirectoryStore::new();
//! let _uc = WorldDirectoryUseCase::new(store);
//! ```

use orbisync_domain::{
    DomainError, DomainErrorKind, InstanceId, Revision, Timestamp, UserId, World, WorldId,
    WorldInstance, transform::Transform,
};

use crate::{ApplicationError, ApplicationErrorKind, ExtensionEvent};

/// Permission required to create a world definition.
///
/// Administrative operation analogous to `admin.users.create` and
/// `admin.roles.create`. The naming follows the existing `admin.*` plural
/// convention (`admin.users.create`) because a world definition is a
/// server-owned administrative resource. The bootstrap administrator is
/// granted this permission.
pub const PERMISSION_CREATE_WORLD: &str = "admin.worlds.create";

/// Permission required to read a world definition or list the directory.
pub const PERMISSION_READ_WORLD: &str = "admin.worlds.read";

/// Permission required to update a world definition.
pub const PERMISSION_UPDATE_WORLD: &str = "admin.worlds.update";

/// Permission required to archive a world definition.
pub const PERMISSION_ARCHIVE_WORLD: &str = "admin.worlds.archive";

/// Permission required to create an instance from a world.
///
/// The design document `auth-authorization.md:228-241` exemplifies
/// `world.instance.create` for this operation. It uses the `world.*`
/// namespace to separate it from administrative `admin.*` permissions,
/// matching the spec's `world.join` / `entity.spawn` examples. The
/// singular `instance` matches the spec example verbatim; the alternative
/// plural `world.instances.create` would diverge from the documented
/// example without benefit.
pub const PERMISSION_CREATE_INSTANCE: &str = "world.instance.create";

/// Permission required to read instance state (`GET /v1/instances`,
/// `GET /v1/instances/{instance_id}`, `GET /v1/instances/{instance_id}/members`).
///
/// Follows the `world.instance.*` namespace established by
/// [`PERMISSION_CREATE_INSTANCE`].
pub const PERMISSION_READ_INSTANCE: &str = "world.instance.read";

/// Permission required to start an instance.
pub const PERMISSION_START_INSTANCE: &str = "world.instance.start";

/// Permission required to stop an instance.
pub const PERMISSION_STOP_INSTANCE: &str = "world.instance.stop";

/// Permission required to remove a member from a running instance.
///
/// Uses the exact string from the `moderation.*` namespace example in
/// `auth-authorization.md` §8.
pub const PERMISSION_KICK_INSTANCE_MEMBER: &str = "moderation.kick";

/// Permission required to create an entity in a world instance.
pub const PERMISSION_ENTITY_SPAWN: &str = "entity.spawn";

/// Permission required for an owner to mutate their own entity.
pub const PERMISSION_ENTITY_UPDATE_OWN: &str = "entity.update.own";

/// Permission that allows mutating entities owned by another user.
pub const PERMISSION_ENTITY_UPDATE_ANY: &str = "entity.update.any";

/// Permissions a role granted to a temporary subject may contain (ADR-026 §7).
///
/// Startup rejects a configured role holding anything outside this set, so an
/// operator cannot hand a guest instance lifecycle control, moderation powers
/// or administrative access by naming the wrong role.
///
/// A denylist of `admin.*` would not work here. ADR-020 forbids a role from
/// mixing `admin.*` with a world-side namespace, so a role named for a guest is
/// necessarily world-side and would always pass such a check, while
/// `world.instance.create`, `world.instance.start`, `world.instance.stop` and
/// `moderation.kick` are not `admin.*` at all and would slip through.
///
/// [`PERMISSION_ENTITY_UPDATE_ANY`] is present deliberately: editing another
/// participant's entity is what collaborative editing means, and the RBAC layer
/// is the wrong place to forbid it. Keeping an owner-only action owner-only is
/// the job of an ADR-025 pre-commit rule, which is enforced independently of
/// this list. Being listed here does not grant it -- only a configured role
/// that actually holds the permission does.
pub const EPHEMERAL_SUBJECT_PERMISSION_ALLOWLIST: &[&str] = &[
    PERMISSION_READ_INSTANCE,
    PERMISSION_ENTITY_SPAWN,
    PERMISSION_ENTITY_UPDATE_OWN,
    PERMISSION_ENTITY_UPDATE_ANY,
];

/// Returns whether every permission in `permissions` may be held by a
/// temporary subject.
///
/// An unrecognised permission string is rejected rather than ignored: a
/// permission this build does not know about cannot be reasoned about, so it
/// is not safe to grant.
pub fn ephemeral_permissions_are_allowed<'a>(
    permissions: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    for permission in permissions {
        if !EPHEMERAL_SUBJECT_PERMISSION_ALLOWLIST.contains(&permission) {
            return Err(permission.to_owned());
        }
    }
    Ok(())
}

/// Audit action for successful or failed world creation.
pub const AUDIT_ACTION_WORLD_CREATED: &str = "world.created";

/// Audit action for successful or failed world update.
pub const AUDIT_ACTION_WORLD_UPDATED: &str = "world.updated";

/// Audit action for successful or failed world archival.
pub const AUDIT_ACTION_WORLD_ARCHIVED: &str = "world.archived";

/// Audit action for successful or failed instance creation.
pub const AUDIT_ACTION_INSTANCE_CREATED: &str = "world.instance.created";

/// Audit action for an instance start.
pub const AUDIT_ACTION_INSTANCE_STARTED: &str = "instance.started";

/// Audit action for an instance stop request.
pub const AUDIT_ACTION_INSTANCE_STOPPED: &str = "instance.stopped";

/// Audit action for a member kicked from an instance.
pub const AUDIT_ACTION_INSTANCE_MEMBER_KICKED: &str = "instance.member_kicked";

/// Audit event for world-directory mutations, coupled with state in one
/// transaction (`identity` `admin.rs` pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldAuditEvent {
    /// Time the action was performed.
    pub occurred_at: Timestamp,
    /// Actor that performed the action.
    pub actor_id: UserId,
    /// Stable action name (`world.created` / `world.instance.created`).
    pub action: &'static str,
    /// Target resource identifier, when one exists.
    pub resource_id: Option<String>,
    /// Request correlation identifier (ADR-014) – proves `request_id` is used.
    pub request_id: crate::RequestId,
    /// Operation result.
    pub succeeded: bool,
}

/// Injected authorizer port for world-directory use cases.
///
/// The application layer owns authorization (design `auth-authorization.md:275-284`);
/// transport must not be the sole gate. The authorizer loads server-owned
/// roles and checks the requested permission.
#[async_trait::async_trait]
pub trait WorldAuthorizer: Send + Sync + 'static {
    /// Requires the permission for `actor`; returns `NotAuthorized` when denied.
    async fn require(&self, actor: UserId, permission: &str) -> Result<(), ApplicationError>;
}

#[async_trait::async_trait]
impl<T> WorldAuthorizer for std::sync::Arc<T>
where
    T: WorldAuthorizer + ?Sized,
{
    async fn require(&self, actor: UserId, permission: &str) -> Result<(), ApplicationError> {
        (**self).require(actor, permission).await
    }
}

/// Authorizer that allows every request – only for tests.
///
/// Production code must use a real authorizer such as `PgWorldAuthorizer`
/// that checks `user_roles` / `role_permissions`. This helper lives behind
/// `#[cfg(test)]` so that a production build cannot accidentally use an
/// allow-all constructor.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllAuthorizer;

#[cfg(test)]
#[async_trait::async_trait]
impl WorldAuthorizer for AllowAllAuthorizer {
    async fn require(&self, _actor: UserId, _permission: &str) -> Result<(), ApplicationError> {
        Ok(())
    }
}

/// Authorizer that denies every request – only for tests (mutation helper).
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllAuthorizer;

#[cfg(test)]
#[async_trait::async_trait]
impl WorldAuthorizer for DenyAllAuthorizer {
    async fn require(&self, _actor: UserId, _permission: &str) -> Result<(), ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorKind::NotAuthorized,
            "permission denied",
        ))
    }
}

/// Command to create a world definition.
#[derive(Debug, Clone)]
pub struct CreateWorldCommand {
    /// Actor performing the operation.
    pub actor_id: UserId,
    /// Desired name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Spawn transform.
    pub default_spawn: Transform,
    /// Capacity 1..1000.
    pub capacity: u32,
    /// Now.
    pub now: Timestamp,
    /// Request id for audit.
    pub request_id: crate::RequestId,
}

/// Command to create an instance from a world.
#[derive(Debug, Clone)]
pub struct CreateInstanceCommand {
    /// Actor.
    pub actor_id: UserId,
    /// World to instantiate.
    pub world_id: WorldId,
    /// Capacity override (None uses world's capacity).
    pub capacity: Option<u32>,
    /// Now.
    pub now: Timestamp,
    /// Request id.
    pub request_id: crate::RequestId,
}

/// Command to update a world definition (PATCH /v1/worlds/{world_id}),
/// merge-patch semantics.
#[derive(Debug, Clone)]
pub struct UpdateWorldCommand {
    /// Actor performing the operation.
    pub actor_id: UserId,
    /// World to update.
    pub world_id: WorldId,
    /// `If-Match` revision precondition.
    pub expected_revision: Revision,
    /// New name, when present.
    pub name: Option<String>,
    /// New description; `Some(None)` clears it, `None` leaves it unchanged.
    pub description: Option<Option<String>>,
    /// New default spawn transform, when present.
    pub default_spawn: Option<Transform>,
    /// New capacity, when present.
    pub capacity: Option<u32>,
    /// Now.
    pub now: Timestamp,
    /// Request id for audit.
    pub request_id: crate::RequestId,
}

/// Command to archive a world (POST /v1/worlds/{world_id}/archive).
///
/// Archiving is idempotent at the domain level (`World::archive`), so unlike
/// `UpdateWorldCommand` this carries no `If-Match` precondition.
#[derive(Debug, Clone)]
pub struct ArchiveWorldCommand {
    /// Actor performing the operation.
    pub actor_id: UserId,
    /// World to archive.
    pub world_id: WorldId,
    /// Now.
    pub now: Timestamp,
    /// Request id for audit.
    pub request_id: crate::RequestId,
}

/// Command to start an instance.
#[derive(Debug, Clone)]
pub struct StartInstanceCommand {
    /// Actor performing the operation.
    pub actor_id: UserId,
    /// Instance to start.
    pub instance_id: InstanceId,
    /// Transition timestamp.
    pub now: Timestamp,
    /// Request id for audit.
    pub request_id: crate::RequestId,
}

/// Command to stop an instance.
#[derive(Debug, Clone)]
pub struct StopInstanceCommand {
    /// Actor performing the operation.
    pub actor_id: UserId,
    /// Instance to stop.
    pub instance_id: InstanceId,
    /// Transition timestamp is retained for the audit row.
    pub now: Timestamp,
    /// Request id for audit.
    pub request_id: crate::RequestId,
}

/// View of a world for queries.
#[derive(Debug, Clone)]
pub struct WorldView {
    /// Id.
    pub id: WorldId,
    /// Name.
    pub name: String,
    /// Status string.
    pub status: String,
    /// Revision.
    pub revision: Revision,
}

/// View of an instance.
#[derive(Debug, Clone)]
pub struct InstanceView {
    /// Id.
    pub id: InstanceId,
    /// World id.
    pub world_id: WorldId,
    /// Lifecycle string.
    pub status: String,
    /// Revision.
    pub revision: Revision,
}

/// Port for world/instance persistence.
///
/// All methods are required. Implementations must couple state and audit in
/// one transaction (`identity::admin.rs` pattern) and must not silently drop
/// audits. There are no default bodies – a new adapter cannot compile while
/// forgetting to handle `record_world_audit` or providing a non-atomic
/// two-step `create_*_with_audit`.
#[async_trait::async_trait]
pub trait WorldDirectoryStore: Send + Sync + 'static {
    /// Fetches a world by id.
    async fn get_world(&self, id: WorldId) -> Result<Option<World>, ApplicationError>;

    /// Lists worlds using stable keyset pagination.
    async fn list_worlds(
        &self,
        page: crate::query::PageRequest,
    ) -> Result<crate::query::Page<WorldView>, ApplicationError>;

    /// Fetches an instance by id.
    async fn get_instance(&self, id: InstanceId)
    -> Result<Option<WorldInstance>, ApplicationError>;

    /// Lists instances using stable keyset pagination (ADR-003).
    ///
    /// # Errors
    ///
    /// Returns a persistence error, or `DomainRule` when `page.after` is not
    /// a valid cursor for this list.
    async fn list_instances(
        &self,
        page: crate::PageRequest,
    ) -> Result<crate::Page<InstanceView>, ApplicationError>;

    /// Persists a world and its audit event in one transaction.
    ///
    /// The implementation must insert both rows in the same database
    /// transaction and roll back the world insert when audit insertion
    /// fails (`identity::admin.rs` pattern).
    ///
    /// # Errors
    ///
    /// Returns persistence error.
    async fn create_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError>;

    /// Persists a world and audit with resolved source-IP metadata.
    async fn create_world_with_audit_and_source_ip(
        &self,
        world: World,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let _ = source_ip;
        self.create_world_with_audit(world, audit).await
    }

    /// Persists an updated world (from `World::update` or `World::archive`)
    /// and its audit event in one transaction.
    ///
    /// The implementation must enforce optimistic concurrency (the world's
    /// current stored revision must equal `world.revision() - 1`, mirroring
    /// `update_instance_with_event`) and roll back the audit insert when the
    /// state update fails.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when the world does not exist, `Conflict` on a
    /// revision mismatch, or persistence error.
    async fn update_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError>;

    /// Persists a world update and audit with resolved source-IP metadata.
    async fn update_world_with_audit_and_source_ip(
        &self,
        world: World,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let _ = source_ip;
        self.update_world_with_audit(world, audit).await
    }

    /// Persists an instance and its audit event in one transaction.
    ///
    /// # Errors
    ///
    /// Returns persistence error.
    async fn create_instance_with_audit(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError>;

    /// Persists an instance and audit with resolved source-IP metadata.
    async fn create_instance_with_audit_and_source_ip(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let _ = source_ip;
        self.create_instance_with_audit(instance, audit).await
    }

    /// Persists an instance lifecycle transition, audit and extension outbox
    /// event in one local transaction.
    async fn update_instance_with_event(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        event: ExtensionEvent,
    ) -> Result<(), ApplicationError> {
        // Existing test adapters that do not model lifecycle persistence still
        // get a visible audit rather than silently claiming durable delivery.
        let _ = (instance, event);
        self.record_world_audit(audit).await
    }

    /// Records a world-directory audit event without a state mutation.
    ///
    /// Used for authorization-failure audits (`succeeded = false`).
    ///
    /// # Errors
    ///
    /// Returns persistence error.
    async fn record_world_audit(&self, audit: WorldAuditEvent) -> Result<(), ApplicationError>;

    /// Records a world audit with resolved source-IP metadata.
    async fn record_world_audit_and_source_ip(
        &self,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        let _ = source_ip;
        self.record_world_audit(audit).await
    }
}

#[async_trait::async_trait]
impl<T> WorldDirectoryStore for std::sync::Arc<T>
where
    T: WorldDirectoryStore + ?Sized,
{
    async fn get_world(&self, id: WorldId) -> Result<Option<World>, ApplicationError> {
        (**self).get_world(id).await
    }

    async fn list_worlds(
        &self,
        page: crate::query::PageRequest,
    ) -> Result<crate::query::Page<WorldView>, ApplicationError> {
        (**self).list_worlds(page).await
    }

    async fn update_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        (**self).update_world_with_audit(world, audit).await
    }

    async fn update_world_with_audit_and_source_ip(
        &self,
        world: World,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        (**self)
            .update_world_with_audit_and_source_ip(world, audit, source_ip)
            .await
    }

    async fn get_instance(
        &self,
        id: InstanceId,
    ) -> Result<Option<WorldInstance>, ApplicationError> {
        (**self).get_instance(id).await
    }

    async fn list_instances(
        &self,
        page: crate::PageRequest,
    ) -> Result<crate::Page<InstanceView>, ApplicationError> {
        (**self).list_instances(page).await
    }

    async fn create_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        (**self).create_world_with_audit(world, audit).await
    }

    async fn create_world_with_audit_and_source_ip(
        &self,
        world: World,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        (**self)
            .create_world_with_audit_and_source_ip(world, audit, source_ip)
            .await
    }

    async fn create_instance_with_audit(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        (**self).create_instance_with_audit(instance, audit).await
    }

    async fn create_instance_with_audit_and_source_ip(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        (**self)
            .create_instance_with_audit_and_source_ip(instance, audit, source_ip)
            .await
    }

    async fn update_instance_with_event(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        event: ExtensionEvent,
    ) -> Result<(), ApplicationError> {
        (**self)
            .update_instance_with_event(instance, audit, event)
            .await
    }

    async fn record_world_audit(&self, audit: WorldAuditEvent) -> Result<(), ApplicationError> {
        (**self).record_world_audit(audit).await
    }

    async fn record_world_audit_and_source_ip(
        &self,
        audit: WorldAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        (**self)
            .record_world_audit_and_source_ip(audit, source_ip)
            .await
    }
}

/// World administration use case.
///
/// The authorizer is a required type parameter. There is no `new(store)`
/// that defaults to allow-all – every composition root must supply an
/// authorizer explicitly, so a new binary or background job cannot compile
/// while silently bypassing authorization or audit.
pub struct WorldDirectoryUseCase<S, A> {
    store: S,
    authorizer: A,
}

impl<S, A> WorldDirectoryUseCase<S, A>
where
    S: WorldDirectoryStore,
    A: WorldAuthorizer,
{
    /// Creates the use case with an injected authorizer.
    ///
    /// Authorization is enforced at the application boundary
    /// (`auth-authorization.md:275-284`); transport must not be the sole gate.
    #[must_use]
    pub fn new(store: S, authorizer: A) -> Self {
        Self { store, authorizer }
    }

    /// Returns a reference to the authorizer.
    #[must_use]
    pub fn authorizer(&self) -> &A {
        &self.authorizer
    }

    /// Creates a world.
    ///
    /// Authorization is enforced via the injected `WorldAuthorizer` and
    /// `actor_id` / `request_id` are used for the mandatory audit event.
    /// State and audit share one storage transaction.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` (mapped to 403) when the actor lacks
    /// `admin.worlds.create`, otherwise validation or persistence error.
    pub async fn create_world(
        &self,
        command: CreateWorldCommand,
    ) -> Result<WorldView, ApplicationError> {
        self.create_world_with_source_ip(command, None).await
    }

    /// Creates a world and records the trusted-proxy-filtered source IP.
    pub async fn create_world_with_source_ip(
        &self,
        command: CreateWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        // Authorization – must be server-owned (application layer).
        if let Err(error) = self
            .authorizer
            .require(command.actor_id, PERMISSION_CREATE_WORLD)
            .await
        {
            let audit = WorldAuditEvent {
                occurred_at: command.now,
                actor_id: command.actor_id,
                action: AUDIT_ACTION_WORLD_CREATED,
                resource_id: None,
                request_id: command.request_id.clone(),
                succeeded: false,
            };
            // Failure audit – best effort, but must not hide the original
            // NotAuthorized. If audit insertion itself fails, we still return
            // the authorization error (the audit port maps to PortFailure, but
            // the caller already gets NotAuthorized). The audit is still
            // attempted so that `result = failure` is persisted when
            // possible (existing identity pattern).
            // Audit failure is observable via `tracing::warn!` (A-3 fix for C4:
            // previously `let _ =` silently discarded the error with a false
            // claim that "storage logs"; now both application and storage log).
            if let Err(err) = self
                .store
                .record_world_audit_and_source_ip(audit, source_ip.clone())
                .await
            {
                tracing::warn!(
                    error = %err,
                    action = AUDIT_ACTION_WORLD_CREATED,
                    "failed to persist world audit for authorization failure (best-effort, primary error is NotAuthorized)"
                );
            }
            return Err(error);
        }
        let world = World::new(
            WorldId::generate(),
            command.name,
            command.description,
            command.default_spawn,
            command.capacity,
            command.now,
        )
        .map_err(|e| ApplicationError::new(ApplicationErrorKind::DomainRule, e.to_string()))?;
        let view = WorldView {
            id: world.id(),
            name: world.name().to_owned(),
            status: world.status().as_str().to_owned(),
            revision: world.revision(),
        };
        let audit = WorldAuditEvent {
            occurred_at: command.now,
            actor_id: command.actor_id,
            action: AUDIT_ACTION_WORLD_CREATED,
            resource_id: Some(world.id().to_string()),
            request_id: command.request_id.clone(),
            succeeded: true,
        };
        self.store
            .create_world_with_audit_and_source_ip(world, audit, source_ip)
            .await?;
        Ok(view)
    }

    /// Fetches a single world after checking `admin.worlds.read`.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks the permission or
    /// `NotFound` when the world does not exist.
    pub async fn get_world(
        &self,
        actor_id: UserId,
        world_id: WorldId,
    ) -> Result<WorldView, ApplicationError> {
        self.authorizer
            .require(actor_id, PERMISSION_READ_WORLD)
            .await?;
        let world = self.store.get_world(world_id).await?.ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::NotFound, "world not found")
        })?;
        Ok(world_view(&world))
    }

    /// Lists worlds after checking `admin.worlds.read`.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks the permission, otherwise
    /// a query port failure.
    pub async fn list_worlds(
        &self,
        actor_id: UserId,
        page: crate::query::PageRequest,
    ) -> Result<crate::query::Page<WorldView>, ApplicationError> {
        self.authorizer
            .require(actor_id, PERMISSION_READ_WORLD)
            .await?;
        self.store.list_worlds(page).await
    }

    /// Updates a world definition (merge-patch semantics).
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized`, `NotFound`, `Conflict` on revision mismatch,
    /// or validation/persistence error.
    pub async fn update_world(
        &self,
        command: UpdateWorldCommand,
    ) -> Result<WorldView, ApplicationError> {
        self.update_world_with_source_ip(command, None).await
    }

    /// Updates a world and records the trusted-proxy-filtered source IP.
    pub async fn update_world_with_source_ip(
        &self,
        command: UpdateWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        if let Err(error) = self
            .authorizer
            .require(command.actor_id, PERMISSION_UPDATE_WORLD)
            .await
        {
            self.audit_failure(
                AUDIT_ACTION_WORLD_UPDATED,
                command.actor_id,
                Some(command.world_id.to_string()),
                command.request_id.clone(),
                command.now,
                source_ip.clone(),
            )
            .await;
            return Err(error);
        }
        let mut world = self
            .store
            .get_world(command.world_id)
            .await?
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorKind::NotFound, "world not found")
            })?;
        world
            .update(
                command.expected_revision,
                command.name,
                command.description,
                command.default_spawn,
                command.capacity,
                command.now,
            )
            .map_err(map_world_domain_error)?;
        let view = world_view(&world);
        let audit = WorldAuditEvent {
            occurred_at: command.now,
            actor_id: command.actor_id,
            action: AUDIT_ACTION_WORLD_UPDATED,
            resource_id: Some(world.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .update_world_with_audit_and_source_ip(world, audit, source_ip)
            .await?;
        Ok(view)
    }

    /// Archives a world. Idempotent: archiving an already-archived world
    /// succeeds without persisting a redundant state write or audit event.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized`, `NotFound`, or persistence error.
    pub async fn archive_world(
        &self,
        command: ArchiveWorldCommand,
    ) -> Result<WorldView, ApplicationError> {
        self.archive_world_with_source_ip(command, None).await
    }

    /// Archives a world and records the trusted-proxy-filtered source IP.
    pub async fn archive_world_with_source_ip(
        &self,
        command: ArchiveWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        if let Err(error) = self
            .authorizer
            .require(command.actor_id, PERMISSION_ARCHIVE_WORLD)
            .await
        {
            self.audit_failure(
                AUDIT_ACTION_WORLD_ARCHIVED,
                command.actor_id,
                Some(command.world_id.to_string()),
                command.request_id.clone(),
                command.now,
                source_ip.clone(),
            )
            .await;
            return Err(error);
        }
        let mut world = self
            .store
            .get_world(command.world_id)
            .await?
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorKind::NotFound, "world not found")
            })?;
        let changed = world.archive(command.now).map_err(map_world_domain_error)?;
        let view = world_view(&world);
        if !changed {
            return Ok(view);
        }
        let audit = WorldAuditEvent {
            occurred_at: command.now,
            actor_id: command.actor_id,
            action: AUDIT_ACTION_WORLD_ARCHIVED,
            resource_id: Some(world.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .update_world_with_audit_and_source_ip(world, audit, source_ip)
            .await?;
        Ok(view)
    }

    /// Records a best-effort authorization-failure audit, matching the
    /// pattern already used by `create_world_with_source_ip` /
    /// `create_instance_with_source_ip`.
    async fn audit_failure(
        &self,
        action: &'static str,
        actor_id: UserId,
        resource_id: Option<String>,
        request_id: crate::RequestId,
        occurred_at: Timestamp,
        source_ip: Option<String>,
    ) {
        let audit = WorldAuditEvent {
            occurred_at,
            actor_id,
            action,
            resource_id,
            request_id,
            succeeded: false,
        };
        if let Err(err) = self
            .store
            .record_world_audit_and_source_ip(audit, source_ip)
            .await
        {
            tracing::warn!(
                error = %err,
                action,
                "failed to persist world audit for authorization failure (best-effort, primary error is NotAuthorized)"
            );
        }
    }

    /// Creates an instance from a world.
    ///
    /// Authorization is enforced for `world.instance.create` and audit uses
    /// `actor_id` / `request_id`. State and audit are one transaction.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks the permission, `NotFound`
    /// when the world does not exist, or validation/persistence error.
    pub async fn create_instance(
        &self,
        command: CreateInstanceCommand,
    ) -> Result<InstanceView, ApplicationError> {
        self.create_instance_with_source_ip(command, None).await
    }

    /// Creates an instance and records the trusted-proxy-filtered source IP.
    pub async fn create_instance_with_source_ip(
        &self,
        command: CreateInstanceCommand,
        source_ip: Option<String>,
    ) -> Result<InstanceView, ApplicationError> {
        if let Err(error) = self
            .authorizer
            .require(command.actor_id, PERMISSION_CREATE_INSTANCE)
            .await
        {
            let audit = WorldAuditEvent {
                occurred_at: command.now,
                actor_id: command.actor_id,
                action: AUDIT_ACTION_INSTANCE_CREATED,
                resource_id: Some(command.world_id.to_string()),
                request_id: command.request_id.clone(),
                succeeded: false,
            };
            // Same as create_world – best-effort audit for authorization failure.
            // Failure is observable via `tracing::warn!` (A-3 fix for C4).
            if let Err(err) = self
                .store
                .record_world_audit_and_source_ip(audit, source_ip.clone())
                .await
            {
                tracing::warn!(
                    error = %err,
                    action = AUDIT_ACTION_INSTANCE_CREATED,
                    "failed to persist instance audit for authorization failure (best-effort, primary error is NotAuthorized)"
                );
            }
            return Err(error);
        }
        let world = self
            .store
            .get_world(command.world_id)
            .await?
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorKind::NotFound, "world not found")
            })?;
        if world.status() != orbisync_domain::WorldStatus::Active {
            return Err(ApplicationError::new(
                ApplicationErrorKind::DomainRule,
                "world is archived",
            ));
        }
        let capacity = command.capacity.unwrap_or(world.capacity());
        let instance =
            WorldInstance::new(InstanceId::generate(), world.id(), capacity, command.now).map_err(
                |e| ApplicationError::new(ApplicationErrorKind::DomainRule, e.to_string()),
            )?;
        let view = InstanceView {
            id: instance.id(),
            world_id: instance.world_id(),
            status: instance.lifecycle().as_str().to_owned(),
            revision: instance.revision(),
        };
        let audit = WorldAuditEvent {
            occurred_at: command.now,
            actor_id: command.actor_id,
            action: AUDIT_ACTION_INSTANCE_CREATED,
            resource_id: Some(instance.id().to_string()),
            request_id: command.request_id.clone(),
            succeeded: true,
        };
        self.store
            .create_instance_with_audit_and_source_ip(instance, audit, source_ip)
            .await?;
        Ok(view)
    }

    /// Starts an instance and emits `instance.started` after the state change.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks `world.instance.start`,
    /// `NotFound` when the instance does not exist, or a domain/persistence
    /// error.
    pub async fn start_instance(
        &self,
        command: StartInstanceCommand,
    ) -> Result<InstanceView, ApplicationError> {
        if let Err(error) = self
            .authorizer
            .require(command.actor_id, PERMISSION_START_INSTANCE)
            .await
        {
            let audit = WorldAuditEvent {
                occurred_at: command.now,
                actor_id: command.actor_id,
                action: AUDIT_ACTION_INSTANCE_STARTED,
                resource_id: Some(command.instance_id.to_string()),
                request_id: command.request_id.clone(),
                succeeded: false,
            };
            if let Err(err) = self.store.record_world_audit(audit).await {
                tracing::warn!(
                    error = %err,
                    action = AUDIT_ACTION_INSTANCE_STARTED,
                    "failed to persist instance-start audit for authorization failure (best-effort, primary error is NotAuthorized)"
                );
            }
            return Err(error);
        }
        let mut instance = self
            .store
            .get_instance(command.instance_id)
            .await?
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorKind::NotFound, "instance not found")
            })?;
        let changed = instance.start(command.now).map_err(|error| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, error.to_string())
        })?;
        if !changed {
            return Ok(InstanceView {
                id: instance.id(),
                world_id: instance.world_id(),
                status: instance.lifecycle().as_str().to_owned(),
                revision: instance.revision(),
            });
        }
        let audit = WorldAuditEvent {
            occurred_at: command.now,
            actor_id: command.actor_id,
            action: AUDIT_ACTION_INSTANCE_STARTED,
            resource_id: Some(instance.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .update_instance_with_event(
                instance.clone(),
                audit,
                ExtensionEvent::InstanceStarted {
                    instance_id: instance.id(),
                },
            )
            .await?;
        Ok(InstanceView {
            id: instance.id(),
            world_id: instance.world_id(),
            status: instance.lifecycle().as_str().to_owned(),
            revision: instance.revision(),
        })
    }

    /// Requests an instance stop and emits `instance.stopped` after the state
    /// transition. The aggregate remains `stopping` while members drain.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks `world.instance.stop`,
    /// `NotFound` when the instance does not exist, or a domain/persistence
    /// error.
    pub async fn stop_instance(
        &self,
        command: StopInstanceCommand,
    ) -> Result<InstanceView, ApplicationError> {
        if let Err(error) = self
            .authorizer
            .require(command.actor_id, PERMISSION_STOP_INSTANCE)
            .await
        {
            let audit = WorldAuditEvent {
                occurred_at: command.now,
                actor_id: command.actor_id,
                action: AUDIT_ACTION_INSTANCE_STOPPED,
                resource_id: Some(command.instance_id.to_string()),
                request_id: command.request_id.clone(),
                succeeded: false,
            };
            if let Err(err) = self.store.record_world_audit(audit).await {
                tracing::warn!(
                    error = %err,
                    action = AUDIT_ACTION_INSTANCE_STOPPED,
                    "failed to persist instance-stop audit for authorization failure (best-effort, primary error is NotAuthorized)"
                );
            }
            return Err(error);
        }
        let mut instance = self
            .store
            .get_instance(command.instance_id)
            .await?
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorKind::NotFound, "instance not found")
            })?;
        let changed = instance.stop().map_err(|error| {
            ApplicationError::new(ApplicationErrorKind::DomainRule, error.to_string())
        })?;
        if !changed {
            return Ok(InstanceView {
                id: instance.id(),
                world_id: instance.world_id(),
                status: instance.lifecycle().as_str().to_owned(),
                revision: instance.revision(),
            });
        }
        let audit = WorldAuditEvent {
            occurred_at: command.now,
            actor_id: command.actor_id,
            action: AUDIT_ACTION_INSTANCE_STOPPED,
            resource_id: Some(instance.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .update_instance_with_event(
                instance.clone(),
                audit,
                ExtensionEvent::InstanceStopped {
                    instance_id: instance.id(),
                },
            )
            .await?;
        Ok(InstanceView {
            id: instance.id(),
            world_id: instance.world_id(),
            status: instance.lifecycle().as_str().to_owned(),
            revision: instance.revision(),
        })
    }

    /// Fetches a single instance.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks `world.instance.read`,
    /// `NotFound` when the instance does not exist, or a persistence error.
    pub async fn get_instance(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
    ) -> Result<InstanceView, ApplicationError> {
        self.authorizer
            .require(actor_id, PERMISSION_READ_INSTANCE)
            .await?;
        let instance = self.store.get_instance(instance_id).await?.ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::NotFound, "instance not found")
        })?;
        Ok(InstanceView {
            id: instance.id(),
            world_id: instance.world_id(),
            status: instance.lifecycle().as_str().to_owned(),
            revision: instance.revision(),
        })
    }

    /// Lists instances using stable keyset pagination.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks `world.instance.read`,
    /// or a persistence error.
    pub async fn list_instances(
        &self,
        actor_id: UserId,
        page: crate::PageRequest,
    ) -> Result<crate::Page<InstanceView>, ApplicationError> {
        self.authorizer
            .require(actor_id, PERMISSION_READ_INSTANCE)
            .await?;
        self.store.list_instances(page).await
    }
}

fn world_view(world: &World) -> WorldView {
    WorldView {
        id: world.id(),
        name: world.name().to_owned(),
        status: world.status().as_str().to_owned(),
        revision: world.revision(),
    }
}

/// Maps a `World::update` / `World::archive` domain error onto the
/// application taxonomy, routing `RevisionMismatch` to `Conflict` (409)
/// instead of the generic `DomainRule` (400) that `ApplicationError::from`
/// would otherwise produce.
fn map_world_domain_error(error: DomainError) -> ApplicationError {
    match error.kind() {
        DomainErrorKind::RevisionMismatch => {
            ApplicationError::new(ApplicationErrorKind::Conflict, error.to_string())
        }
        _ => ApplicationError::new(ApplicationErrorKind::DomainRule, error.to_string()),
    }
}

impl<S, A> core::fmt::Debug for WorldDirectoryUseCase<S, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorldDirectoryUseCase")
            .field("store", &"WorldDirectoryStore")
            .field("authorizer", &"WorldAuthorizer")
            .finish()
    }
}

impl<S, A> Clone for WorldDirectoryUseCase<S, A>
where
    S: Clone,
    A: Clone,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            authorizer: self.authorizer.clone(),
        }
    }
}

/// Port for live instance membership, backed by the running instance actor
/// (`instance_runtime`, `state-and-runtime.md`) rather than PostgreSQL.
///
/// Presence is process-local and ephemeral (`session_store.rs` D-24): an
/// instance with no running actor simply has no live members, which is not
/// an error. Implementations live in the composition root that also owns the
/// runtime registry (`orbisync-server`), because `orbisync-application`
/// cannot depend on `orbisync-world-runtime` (DAG, `check_architecture.py`).
#[async_trait::async_trait]
pub trait InstanceMembershipStore: Send + Sync + 'static {
    /// Returns the users currently present in `instance_id`, in no
    /// particular order. Returns an empty list when the instance has no
    /// running actor.
    async fn list_members(&self, instance_id: InstanceId) -> Result<Vec<UserId>, ApplicationError>;

    /// Removes every live presence held by `user_id` in `instance_id`.
    ///
    /// Returns `true` when at least one presence was removed, `false` when
    /// the user was not a live member (including when the instance has no
    /// running actor).
    async fn kick_member(
        &self,
        instance_id: InstanceId,
        user_id: UserId,
    ) -> Result<bool, ApplicationError>;
}

#[async_trait::async_trait]
impl<T> InstanceMembershipStore for std::sync::Arc<T>
where
    T: InstanceMembershipStore + ?Sized,
{
    async fn list_members(&self, instance_id: InstanceId) -> Result<Vec<UserId>, ApplicationError> {
        (**self).list_members(instance_id).await
    }

    async fn kick_member(
        &self,
        instance_id: InstanceId,
        user_id: UserId,
    ) -> Result<bool, ApplicationError> {
        (**self).kick_member(instance_id, user_id).await
    }
}

/// Instance membership administration use case.
///
/// Holds a [`WorldDirectoryStore`] alongside the [`InstanceMembershipStore`]
/// so instance existence (`world_instances`, the durable truth) can be
/// checked without granting the caller `world.instance.read` – kicking a
/// member and reading an instance are different permissions
/// (`auth-authorization.md` §8).
pub struct InstanceMembershipUseCase<S, M, A> {
    store: S,
    membership: M,
    authorizer: A,
}

impl<S, M, A> InstanceMembershipUseCase<S, M, A>
where
    S: WorldDirectoryStore,
    M: InstanceMembershipStore,
    A: WorldAuthorizer,
{
    /// Creates the use case with an injected authorizer (secure-by-default,
    /// same rationale as [`WorldDirectoryUseCase::new`]).
    #[must_use]
    pub fn new(store: S, membership: M, authorizer: A) -> Self {
        Self {
            store,
            membership,
            authorizer,
        }
    }

    async fn require_instance(&self, instance_id: InstanceId) -> Result<(), ApplicationError> {
        self.store.get_instance(instance_id).await?.ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::NotFound, "instance not found")
        })?;
        Ok(())
    }

    /// Lists the live members of an instance using in-memory keyset
    /// pagination over the user ids reported by the runtime actor.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks `world.instance.read`,
    /// `NotFound` when the instance does not exist, or a persistence error.
    pub async fn list_members(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
        page: crate::PageRequest,
    ) -> Result<crate::Page<UserId>, ApplicationError> {
        self.authorizer
            .require(actor_id, PERMISSION_READ_INSTANCE)
            .await?;
        self.require_instance(instance_id).await?;
        let mut members = self.membership.list_members(instance_id).await?;
        members.sort_by_key(UserId::to_string);
        members.dedup();
        let limit = page.limit as usize;
        let after = page.after.map(|c| c.0);
        let start = match after {
            Some(cursor) => members
                .iter()
                .position(|id| id.to_string() > cursor)
                .unwrap_or(members.len()),
            None => 0,
        };
        let remaining = &members[start..];
        let items: Vec<UserId> = remaining.iter().take(limit).copied().collect();
        let next = if remaining.len() > limit {
            items.last().map(|id| crate::QueryCursor(id.to_string()))
        } else {
            None
        };
        Ok(crate::Page { items, next })
    }

    /// Removes every live presence held by `target_user_id` in `instance_id`
    /// and records the moderation audit event.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the actor lacks `moderation.kick`,
    /// `NotFound` when the instance does not exist or the target is not
    /// currently a live member, or a persistence error.
    pub async fn kick_member(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
        target_user_id: UserId,
        now: Timestamp,
        request_id: crate::RequestId,
    ) -> Result<(), ApplicationError> {
        if let Err(error) = self
            .authorizer
            .require(actor_id, PERMISSION_KICK_INSTANCE_MEMBER)
            .await
        {
            let audit = WorldAuditEvent {
                occurred_at: now,
                actor_id,
                action: AUDIT_ACTION_INSTANCE_MEMBER_KICKED,
                resource_id: Some(instance_id.to_string()),
                request_id: request_id.clone(),
                succeeded: false,
            };
            if let Err(err) = self.store.record_world_audit(audit).await {
                tracing::warn!(
                    error = %err,
                    action = AUDIT_ACTION_INSTANCE_MEMBER_KICKED,
                    "failed to persist kick audit for authorization failure (best-effort, primary error is NotAuthorized)"
                );
            }
            return Err(error);
        }
        self.require_instance(instance_id).await?;
        let removed = self
            .membership
            .kick_member(instance_id, target_user_id)
            .await?;
        if !removed {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotFound,
                "user is not a live member of this instance",
            ));
        }
        let audit = WorldAuditEvent {
            occurred_at: now,
            actor_id,
            action: AUDIT_ACTION_INSTANCE_MEMBER_KICKED,
            resource_id: Some(instance_id.to_string()),
            request_id,
            succeeded: true,
        };
        self.store.record_world_audit(audit).await
    }
}

impl<S, M, A> core::fmt::Debug for InstanceMembershipUseCase<S, M, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InstanceMembershipUseCase")
            .field("store", &"WorldDirectoryStore")
            .field("membership", &"InstanceMembershipStore")
            .field("authorizer", &"WorldAuthorizer")
            .finish()
    }
}
