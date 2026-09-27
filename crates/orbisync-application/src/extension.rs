//! Transport-neutral extension events, registration metadata and outbox ports.

use std::collections::BTreeSet;

use orbisync_domain::{DomainError, DomainErrorKind, EntityId, InstanceId, PresenceId, UserId};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

/// Public event kinds supported by the extension webhook contract.
pub const PUBLIC_EVENT_KINDS: &[&str] = &[
    "user.created",
    "user.disabled",
    "instance.started",
    "instance.stopped",
    "member.joined",
    "member.left",
    "entity.spawned",
    "entity.updated",
    "entity.deleted",
    "entity.ownership_transferred",
];

/// Fact emitted after a successful state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionEvent {
    /// A user account was created.
    UserCreated {
        /// Created user.
        user_id: UserId,
    },
    /// A user account was disabled.
    UserDisabled {
        /// Disabled user.
        user_id: UserId,
    },
    /// An instance started accepting runtime work.
    InstanceStarted {
        /// Started instance.
        instance_id: InstanceId,
    },
    /// An instance stopped accepting runtime work.
    InstanceStopped {
        /// Stopped instance.
        instance_id: InstanceId,
    },
    /// A presence joined an instance.
    MemberJoined {
        /// Instance that was joined.
        instance_id: InstanceId,
        /// Presence that was created.
        presence_id: PresenceId,
        /// User that joined.
        user_id: UserId,
    },
    /// A presence left an instance.
    MemberLeft {
        /// Instance that was left.
        instance_id: InstanceId,
        /// Presence that left.
        presence_id: PresenceId,
        /// User that left.
        user_id: UserId,
    },
    /// An entity was spawned.
    EntitySpawned {
        /// Owning instance.
        instance_id: InstanceId,
        /// Spawned entity.
        entity_id: EntityId,
        /// Owner at spawn time, if any.
        owner: Option<UserId>,
    },
    /// An entity was updated.
    EntityUpdated {
        /// Owning instance.
        instance_id: InstanceId,
        /// Updated entity.
        entity_id: EntityId,
    },
    /// An entity was deleted.
    EntityDeleted {
        /// Owning instance.
        instance_id: InstanceId,
        /// Deleted entity.
        entity_id: EntityId,
    },
    /// Ownership of an entity was transferred.
    OwnershipTransferred {
        /// Owning instance.
        instance_id: InstanceId,
        /// Entity whose owner changed.
        entity_id: EntityId,
        /// Owner before transfer.
        previous_owner: Option<UserId>,
        /// Owner after transfer.
        new_owner: Option<UserId>,
    },
}

impl ExtensionEvent {
    /// Returns the stable webhook event name.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UserCreated { .. } => "user.created",
            Self::UserDisabled { .. } => "user.disabled",
            Self::InstanceStarted { .. } => "instance.started",
            Self::InstanceStopped { .. } => "instance.stopped",
            Self::MemberJoined { .. } => "member.joined",
            Self::MemberLeft { .. } => "member.left",
            Self::EntitySpawned { .. } => "entity.spawned",
            Self::EntityUpdated { .. } => "entity.updated",
            Self::EntityDeleted { .. } => "entity.deleted",
            Self::OwnershipTransferred { .. } => "entity.ownership_transferred",
        }
    }

    /// Returns the instance that produced the event, when there is one.
    #[must_use]
    pub const fn instance_id(&self) -> Option<InstanceId> {
        match self {
            Self::UserCreated { .. } | Self::UserDisabled { .. } => None,
            Self::InstanceStarted { instance_id }
            | Self::InstanceStopped { instance_id }
            | Self::MemberJoined { instance_id, .. }
            | Self::MemberLeft { instance_id, .. }
            | Self::EntitySpawned { instance_id, .. }
            | Self::EntityUpdated { instance_id, .. }
            | Self::EntityDeleted { instance_id, .. }
            | Self::OwnershipTransferred { instance_id, .. } => Some(*instance_id),
        }
    }

    /// Builds the public payload. Internal fields and secret material are not
    /// included in this mapping.
    #[must_use]
    pub fn payload(&self) -> Value {
        match self {
            Self::UserCreated { user_id } => json!({ "user_id": user_id.to_string() }),
            Self::UserDisabled { user_id } => json!({ "user_id": user_id.to_string() }),
            Self::InstanceStarted { instance_id } => {
                json!({ "instance_id": instance_id.to_string() })
            }
            Self::InstanceStopped { instance_id } => {
                json!({ "instance_id": instance_id.to_string() })
            }
            Self::MemberJoined {
                instance_id,
                presence_id,
                user_id,
            } => json!({
                "instance_id": instance_id.to_string(),
                "presence_id": presence_id.to_string(),
                "user_id": user_id.to_string(),
            }),
            Self::MemberLeft {
                instance_id,
                presence_id,
                user_id,
            } => json!({
                "instance_id": instance_id.to_string(),
                "presence_id": presence_id.to_string(),
                "user_id": user_id.to_string(),
            }),
            Self::EntitySpawned {
                instance_id,
                entity_id,
                owner,
            } => json!({
                "instance_id": instance_id.to_string(),
                "entity_id": entity_id.to_string(),
                "owner": owner.map(|value| value.to_string()),
            }),
            Self::EntityUpdated {
                instance_id,
                entity_id,
            }
            | Self::EntityDeleted {
                instance_id,
                entity_id,
            } => json!({
                "instance_id": instance_id.to_string(),
                "entity_id": entity_id.to_string(),
            }),
            Self::OwnershipTransferred {
                instance_id,
                entity_id,
                previous_owner,
                new_owner,
            } => json!({
                "instance_id": instance_id.to_string(),
                "entity_id": entity_id.to_string(),
                "previous_owner": previous_owner.map(|value| value.to_string()),
                "new_owner": new_owner.map(|value| value.to_string()),
            }),
        }
    }
}

/// Registration lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionStatus {
    /// Registration can receive events.
    Active,
    /// Registration is retained but cannot receive events.
    Suspended,
}

impl ExtensionStatus {
    /// Returns the database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }
}

/// Persisted extension manifest.
///
/// `signing_secret_ref` is intentionally only a reference understood by the
/// runtime secret provider. The secret value is never part of this type.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionRegistration {
    /// Stable extension identifier.
    pub extension_id: Uuid,
    /// Display name.
    pub name: String,
    /// Optional display description.
    pub description: Option<String>,
    /// Webhook destination. Delivery is implemented by WH2.
    pub endpoint: String,
    /// Event kinds requested by the extension.
    pub subscribed_events: BTreeSet<String>,
    /// Allowed command capabilities.
    pub capabilities: BTreeSet<String>,
    /// Scoped service-token permissions.
    pub token_scopes: BTreeSet<String>,
    /// Registration lifecycle state.
    pub status: ExtensionStatus,
    /// Secret-store reference, never the secret itself.
    pub signing_secret_ref: String,
}

/// Persistence port for extension registrations.
#[async_trait::async_trait]
pub trait ExtensionRegistrationStore: Send + Sync + 'static {
    /// Inserts or updates a registration without exposing secret material.
    ///
    /// Rejects with [`crate::ApplicationErrorKind::Conflict`] when saving
    /// `registration` as `Active` would leave two different `extension_id`s
    /// both `Active` for the same `hooks:*` capability (ADR-025 §2.7). The
    /// caller must not retry by picking a different `extension_id` for the
    /// same logical extension; it must resolve the conflict (suspend or
    /// remove the existing holder) before retrying.
    async fn save_registration(
        &self,
        registration: ExtensionRegistration,
    ) -> Result<(), crate::ApplicationError>;

    /// Loads a registration by its stable identifier.
    async fn find_registration(
        &self,
        extension_id: Uuid,
    ) -> Result<Option<ExtensionRegistration>, crate::ApplicationError>;

    /// Loads the single Active registration subscribed to `capability`, when
    /// one exists (ADR-025).
    ///
    /// At most one Active extension may hold a given pre-commit-hook
    /// capability at a time. `save_registration` enforces this going
    /// forward (it rejects a save that would create a second Active holder
    /// of the same `hooks:*` capability). This read path additionally
    /// detects the case that constraint cannot prevent — a duplicate
    /// created before it existed, e.g. by inserting rows directly with SQL
    /// rather than through `save_registration` — and returns
    /// [`crate::ApplicationErrorKind::Conflict`] rather than silently
    /// picking one of the duplicates to authorize against. A misconfigured
    /// deployment must fail closed here, the same way the pre-commit gate
    /// fails closed on every other error path; it must not become an
    /// outage-avoidance heuristic that authorizes one candidate at random.
    async fn find_active_registration_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, crate::ApplicationError>;
}

/// Persistence port for durable extension outbox events.
#[async_trait::async_trait]
pub trait ExtensionOutboxStore: Send + Sync + 'static {
    /// Appends one event before any delivery attempt.
    async fn append_event(&self, event: ExtensionEvent) -> Result<Uuid, crate::ApplicationError>;
}

/// One persisted event awaiting delivery to one extension.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingExtensionDelivery {
    /// Delivery row identifier.
    pub delivery_id: Uuid,
    /// Stable event identifier shared by retries and duplicate deliveries.
    pub event_id: Uuid,
    /// Public event kind.
    pub event_kind: String,
    /// Public event payload. It never contains signing secret material.
    pub payload: Value,
    /// Destination registration.
    pub registration: ExtensionRegistration,
    /// Number of attempts already made.
    pub attempt_count: u32,
    /// Earliest time at which this row may be attempted.
    pub available_at: OffsetDateTime,
    /// Process/worker owner of the claim that may finalize this row.
    pub lease_owner: Uuid,
    /// Opaque claim token for this individual claim.
    pub lease_token: Uuid,
    /// Time at which the claim becomes available to another worker.
    pub lease_expires_at: OffsetDateTime,
}

/// Persistence port used by the extension delivery worker.
#[async_trait::async_trait]
pub trait ExtensionDeliveryStore: Send + Sync + 'static {
    /// Claims a bounded batch of due delivery rows.
    async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease_owner: Uuid,
        lease_expires_at: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<PendingExtensionDelivery>, crate::ApplicationError>;

    /// Marks a delivery as accepted by the endpoint.
    async fn mark_delivered(
        &self,
        delivery_id: Uuid,
        lease_owner: Uuid,
        lease_token: Uuid,
        lease_expires_at: OffsetDateTime,
    ) -> Result<(), crate::ApplicationError>;

    /// Schedules another attempt without exposing the error detail.
    async fn reschedule(
        &self,
        delivery_id: Uuid,
        lease_owner: Uuid,
        lease_token: Uuid,
        lease_expires_at: OffsetDateTime,
        attempt_count: u32,
        available_at: OffsetDateTime,
        error_code: &str,
    ) -> Result<(), crate::ApplicationError>;

    /// Moves a permanently failed delivery to the dead-letter queue.
    async fn move_to_dead_letter(
        &self,
        delivery_id: Uuid,
        lease_owner: Uuid,
        lease_token: Uuid,
        lease_expires_at: OffsetDateTime,
        attempt_count: u32,
        error_code: &str,
        retained_until: OffsetDateTime,
    ) -> Result<(), crate::ApplicationError>;

    /// Removes dead-letter rows older than `before`.
    async fn purge_dead_letters(
        &self,
        before: OffsetDateTime,
    ) -> Result<u64, crate::ApplicationError>;
}

impl ExtensionRegistration {
    /// Validates manifest metadata without resolving or reading the secret.
    ///
    /// This implementation chooses environment-variable names as secret
    /// references. Requiring an uppercase identifier with an underscore makes
    /// accidentally submitting a raw secret fail closed at the boundary.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.name.trim().is_empty() || self.endpoint.trim().is_empty() {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension name and endpoint must not be empty",
            ));
        }
        let valid_ref = self.signing_secret_ref.len() >= 3
            && self.signing_secret_ref.contains('_')
            && self.signing_secret_ref.chars().all(|character| {
                character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
            });
        if !valid_ref {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "signing_secret_ref must be an environment variable name",
            ));
        }
        if self
            .subscribed_events
            .iter()
            .any(|kind| !is_public_event_kind(kind))
        {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "subscribed_events contains an unknown event kind",
            ));
        }
        Ok(())
    }

    /// Returns true when an active registration requests this event kind.
    #[must_use]
    pub fn subscribes_to(&self, event: &ExtensionEvent) -> bool {
        self.status == ExtensionStatus::Active && self.subscribed_events.contains(event.kind())
    }
}

/// Returns whether a kind is part of the public event contract.
#[must_use]
pub fn is_public_event_kind(kind: &str) -> bool {
    PUBLIC_EVENT_KINDS.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::{ExtensionEvent, is_public_event_kind};
    use orbisync_domain::{InstanceId, UserId};

    #[test]
    fn account_events_have_no_instance_and_public_payloads() {
        let event = ExtensionEvent::UserCreated {
            user_id: UserId::generate(),
        };
        assert_eq!(event.kind(), "user.created");
        assert_eq!(event.instance_id(), None);
        assert!(event.payload().get("user_id").is_some());
        assert!(is_public_event_kind(event.kind()));
    }

    #[test]
    fn instance_events_keep_their_instance_context() {
        let instance_id = InstanceId::generate();
        let event = ExtensionEvent::InstanceStarted { instance_id };
        assert_eq!(event.instance_id(), Some(instance_id));
        assert_eq!(event.kind(), "instance.started");
        assert!(is_public_event_kind(event.kind()));
    }
}
