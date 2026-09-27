//! Instance actor commands (`state-and-runtime.md` §3.1).

use orbisync_domain::{
    CommandId, EntityId, EntityKind, InstanceId, PresenceId, Revision, Transform, UserId,
    VisibilityPolicy,
};

/// World permissions resolved by the authenticated server boundary.
///
/// The runtime receives this value as command data and never reaches into the
/// identity subsystem itself. A missing permission is represented by `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorldPermissions {
    /// Allows creating entities.
    pub entity_spawn: bool,
    /// Allows an owner to mutate their own entity.
    pub entity_update_own: bool,
    /// Allows mutating an entity owned by another user.
    pub entity_update_any: bool,
}

impl WorldPermissions {
    /// Permissions used by isolated runtime tests that do not model RBAC.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            entity_spawn: true,
            entity_update_own: true,
            entity_update_any: true,
        }
    }
}

impl InstanceCommand {
    /// Overwrites the `permissions` carried by this command, for variants
    /// that carry one.
    ///
    /// Used by the realtime transport (ADR-025) to replace a
    /// connection-lifetime permission snapshot with a freshly re-resolved
    /// one immediately before and after a synchronous pre-commit hook call,
    /// without having to rebuild the whole command from scratch. A no-op
    /// for variants that carry no `permissions` field.
    pub fn set_permissions(&mut self, permissions: WorldPermissions) {
        match self {
            Self::UpdateTransform { permissions: p, .. }
            | Self::SpawnEntity { permissions: p, .. }
            | Self::DeleteEntity { permissions: p, .. }
            | Self::UpdateEntityComponent { permissions: p, .. }
            | Self::TransferOwnership { permissions: p, .. } => *p = permissions,
            Self::WithExpectedEntityState { command, .. } => command.set_permissions(permissions),
            Self::Join { .. }
            | Self::Leave { .. }
            | Self::RetainReliable { .. }
            | Self::PublishEvent { .. }
            | Self::Tick
            | Self::Shutdown => {}
        }
    }
}

/// Command sent to the instance actor.
#[derive(Debug, Clone)]
pub enum InstanceCommand {
    /// Internal pre-commit guard, checked at the actual mailbox application
    /// boundary. It is never a client-selectable operation.
    WithExpectedEntityState {
        /// The single entity inspected by the external rule.
        entity_id: EntityId,
        /// Exact state inspected; `None` means it must still be absent.
        expected: Option<Box<orbisync_domain::Entity>>,
        /// The authorized spawn/update/delete command to apply.
        command: Box<InstanceCommand>,
    },
    /// Retain an already applied reliable delivery until the bounded replay window expires.
    RetainReliable {
        /// Stable transport identifier used to find a retained acknowledgement.
        message_id: String,
        /// Revision of the applied mutation.
        revision: Revision,
        /// Opaque transport payload; the runtime does not interpret the wire format.
        payload: std::sync::Arc<[u8]>,
    },
    /// A client requests to join.
    Join {
        /// Presence that will be created.
        presence_id: PresenceId,
        /// User joining.
        user_id: UserId,
        /// Instance being joined.
        instance_id: InstanceId,
        /// Member capacity for this instance (plumbed from `WorldInstance::capacity`).
        capacity: u32,
    },
    /// A member leaves.
    Leave {
        /// Presence leaving.
        presence_id: PresenceId,
    },
    /// A transform input from a member.
    UpdateTransform {
        /// Entity to move (typically avatar).
        entity_id: EntityId,
        /// Authoritative transform.
        transform: Transform,
        /// Expected entity revision.
        expected_revision: Revision,
        /// Actor user for ownership check.
        user_id: UserId,
        /// Timestamp of the request (for speed validation).
        now: orbisync_domain::Timestamp,
        /// Permissions resolved for the requesting user.
        permissions: WorldPermissions,
    },
    /// Spawn a new entity (M4 generic entity runtime).
    SpawnEntity {
        /// Client supplied idempotency key, when the command came from realtime transport.
        command_id: Option<CommandId>,
        /// Entity to create.
        entity_id: EntityId,
        /// Kind of entity.
        kind: EntityKind,
        /// Initial owner (server-authoritative).
        owner: Option<UserId>,
        /// Initial transform.
        transform: Option<Transform>,
        /// Visibility policy.
        visibility: VisibilityPolicy,
        /// Requesting authenticated user.
        requester: UserId,
        /// Permissions resolved for the requesting user.
        permissions: WorldPermissions,
    },
    /// Delete an existing entity with optimistic concurrency.
    DeleteEntity {
        /// Client supplied idempotency key, when the command came from realtime transport.
        command_id: Option<CommandId>,
        /// Entity to delete.
        entity_id: EntityId,
        /// Expected current revision.
        expected_revision: Revision,
        /// Requesting user for ownership check.
        requester: UserId,
        /// Permissions resolved for the requesting user.
        permissions: WorldPermissions,
    },
    /// Update or insert a component on an entity.
    UpdateEntityComponent {
        /// Client supplied idempotency key, when the command came from realtime transport.
        command_id: Option<CommandId>,
        /// Target entity.
        entity_id: EntityId,
        /// Component key (e.g. "core.health" or "com.example.door").
        component_key: String,
        /// Raw payload bytes (core does not interpret).
        payload_bytes: Vec<u8>,
        /// Expected current revision.
        expected_revision: Revision,
        /// Timestamp used by the instance component-update rate limiter.
        now: orbisync_domain::Timestamp,
        /// Requesting user for ownership check.
        requester: UserId,
        /// Permissions resolved for the requesting user.
        permissions: WorldPermissions,
    },
    /// Transfer ownership of an entity.
    TransferOwnership {
        /// Client supplied idempotency key, when the command came from realtime transport.
        command_id: Option<CommandId>,
        /// Target entity.
        entity_id: EntityId,
        /// New owner (None clears ownership).
        new_owner: Option<UserId>,
        /// Expected current revision.
        expected_revision: Revision,
        /// Timestamp recorded on the entity and its administrative audit.
        now: orbisync_domain::Timestamp,
        /// Requesting authenticated user.
        requester: UserId,
        /// Permissions resolved for the requesting user.
        permissions: WorldPermissions,
    },
    /// Accept a custom room event from an authenticated, joined presence.
    PublishEvent {
        /// Presence bound to the sending connection by the server.
        presence_id: PresenceId,
        /// Authenticated sender, never taken from the event payload.
        user_id: UserId,
    },
    /// Periodic tick.
    Tick,
    /// Graceful shutdown.
    Shutdown,
}

/// Result of applying a command.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandOutcome {
    /// Command applied; revision advanced.
    Applied {
        /// New instance revision.
        revision: Revision,
        /// New entity revision when the command affected an entity.
        entity_revision: Option<Revision>,
        /// Owned entity at the mutation boundary (pre-removal for delete); never a later read.
        committed_entity: Option<Box<orbisync_domain::Entity>>,
    },
    /// Command rejected; reason is human readable.
    Rejected {
        /// Machine code.
        code: &'static str,
        /// Detail.
        detail: String,
    },
}
