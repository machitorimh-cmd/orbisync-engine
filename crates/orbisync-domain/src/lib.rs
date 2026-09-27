//! Pure domain types shared by every OrbiSync module.
//!
//! This crate owns the stable identifiers, the clock abstraction, the revision
//! counter and the domain error taxonomy described in
//! `docs/design/domain-model.md` and `docs/design/repo-crate-conventions.md`
//! §2.2.
//!
//! # Dependency rule
//!
//! `domain` is the root of the dependency graph. It depends on no other
//! workspace crate and on no framework crate (Axum, SQLx, Tokio) or protocol
//! generated code (`repo-crate-conventions.md` §3.2, acceptance condition 1).
//! `scripts/check_architecture.py` enforces this in CI.

pub mod clock;
pub mod entity;
pub mod error;
pub mod id;
pub mod identity;
pub mod instance;
pub mod revision;
pub mod time;
pub mod transform;
pub mod world;

pub use clock::{Clock, SystemClock};
pub use entity::{
    Animation, CORE_ANIMATION, CORE_PRESENCE, CORE_TRANSFORM, CORE_VELOCITY, CustomVisibilityTag,
    Entity, EntityKind, Presence, PresenceState, VisibilityPolicy,
};
pub use error::{DomainError, DomainErrorKind};
pub use id::{
    AccessTokenId, AuthSessionId, CommandId, EntityId, InstanceId, PresenceId,
    RealtimeConnectionId, RefreshTokenFamilyId, RoleId, UserId, WorldId,
};
pub use identity::{
    AuthMethod, AuthSession, AuthSessionStatus, Credential, IdentityEvent, LoginId, PasswordHash,
    Permission, RESERVED_LOGIN_ID_PREFIXES, Role, User, UserKind, UserStatus, is_reserved_login_id,
    validate_supplied_display_name,
};
pub use instance::{InstanceLifecycle, WorldInstance};
pub use revision::Revision;
pub use time::Timestamp;
pub use transform::{Quaternion, Transform, Vec3};
pub use world::{World, WorldStatus};
