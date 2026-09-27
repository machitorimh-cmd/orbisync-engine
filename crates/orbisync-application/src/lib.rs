//! Use cases, port traits and transaction boundaries.
//!
//! The application layer orchestrates domain types and declares the ports that
//! adapters implement (`architecture.md` §2.1, specification §30.2).
//!
//! # Dependency rule
//!
//! `application` depends on `domain` only (`repo-crate-conventions.md` §3.2).
//! Its public API never names an Axum extractor, an HTTP status, a SQLx
//! transaction or row, or a WebSocket frame (§8 acceptance condition 2); the
//! dependency check in `scripts/check_architecture.py` keeps those crates out
//! of the graph so such a signature cannot be written.

pub mod checkpoint;
pub mod checkpoint_admission;
pub mod checkpoint_record;
pub mod checkpoint_stream;
pub use checkpoint_stream::CheckpointLimits;
pub mod diagnostics;
pub mod entity_persistence;
pub mod error;
pub mod extension;
pub mod extension_command;
pub mod health;
pub mod identity;
pub mod identity_provider;
pub mod login;
pub mod metrics;
pub mod pagination;
pub mod port;
pub mod query;
pub mod world;

pub use checkpoint::{
    Checkpoint as AppCheckpoint, CheckpointSaveReceipt, CheckpointStore,
    MAX_CHECKPOINT_PAYLOAD_BYTES,
};
pub use diagnostics::{
    OperationalDiagnostics, OperationalDiagnosticsPort, OperationalQueueLimits,
    OperationalQueueSnapshot, OperationalRateLimitSnapshot, OperationalRetentionSnapshot,
};
pub use entity_persistence::{
    AUDIT_ACTION_ENTITY_OWNERSHIP_TRANSFERRED, EntityOwnershipTransferAudit,
    EntityPersistenceEvent, PersistentEntityStore,
};
pub use error::{ApplicationError, ApplicationErrorKind};
pub use extension::{
    ExtensionDeliveryStore, ExtensionEvent, ExtensionOutboxStore, ExtensionRegistration,
    ExtensionRegistrationStore, ExtensionStatus, PUBLIC_EVENT_KINDS, PendingExtensionDelivery,
    is_public_event_kind,
};
pub use health::{ReadinessReport, check_readiness};
pub use identity::{
    CreateRealtimeTicketCommand, CreateRefreshTokenCommand, CreateRoleCommand, CreateUserCommand,
    EncryptedResponse, IdempotencyClaim, IdempotencyClaimCommand, IdempotencyCompletion,
    IdempotencyStore, IdentityAdministrationStore, IdentityAuditEvent, IdentityMutation,
    IdentityPortError, IdentityRepository, LoginAccount, LoginCommand, LoginFailureOutcome,
    LoginResult, LoginUseCase, RealtimeTicketConsumption, RealtimeTicketStore,
    RefreshRotationResult, RefreshTokenCreationStore, RefreshTokenReplacement,
    RefreshTokenRotationStore, RequestId, RotateRefreshTokenCommand, SecretString,
    UpdateRoleCommand, UserAdministrationUseCase,
};
pub use identity_provider::{
    EphemeralSubjectScope, ExternalAuthError, ExternalIdentity, ExternalIdentityStore,
    IdentityProvider, ScopeDecision,
};
pub use login::{
    EphemeralSubjectRecord, ExternalIdentityRecord, LoginAuditEvent, LoginCommit,
    LoginRefreshRecord, LoginSessionRecord, LoginTransactionStore, NewSubjectUser, SubjectCommit,
};
pub use port::{AuditEvent, AuditSink, HealthProbe};
pub use query::{
    AuditFilter, AuditQueryPort, AuditView, IdentityQueryPort, Page, PageRequest, QueryCursor,
    RoleView, UserView,
};
pub use world::{
    AUDIT_ACTION_INSTANCE_CREATED, AUDIT_ACTION_INSTANCE_MEMBER_KICKED,
    AUDIT_ACTION_INSTANCE_STARTED, AUDIT_ACTION_INSTANCE_STOPPED, AUDIT_ACTION_WORLD_ARCHIVED,
    AUDIT_ACTION_WORLD_CREATED, AUDIT_ACTION_WORLD_UPDATED, ArchiveWorldCommand,
    CreateInstanceCommand, CreateWorldCommand, InstanceMembershipStore, InstanceMembershipUseCase,
    InstanceView, PERMISSION_ARCHIVE_WORLD, PERMISSION_CREATE_INSTANCE, PERMISSION_CREATE_WORLD,
    PERMISSION_KICK_INSTANCE_MEMBER, PERMISSION_READ_INSTANCE, PERMISSION_READ_WORLD,
    PERMISSION_START_INSTANCE, PERMISSION_STOP_INSTANCE, PERMISSION_UPDATE_WORLD,
    StartInstanceCommand, StopInstanceCommand, UpdateWorldCommand, WorldAuditEvent,
    WorldAuthorizer, WorldDirectoryStore, WorldDirectoryUseCase, WorldView,
};

#[cfg(test)]
pub use world::{AllowAllAuthorizer, DenyAllAuthorizer};
