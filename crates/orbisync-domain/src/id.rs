//! Stable identifiers.
//!
//! Every persistent or publicly visible identifier is a UUIDv7 wrapped in its
//! own newtype (ADR-001, specification §31.3). Distinct identifier kinds are
//! distinct Rust types, so passing an [`InstanceId`] where a [`UserId`] is
//! expected is a compile error:
//!
//! ```compile_fail
//! use orbisync_domain::{InstanceId, UserId};
//!
//! fn kick(_user: UserId) {}
//! kick(InstanceId::generate());
//! ```
//!
//! The wire representation is the canonical lowercase hyphenated UUID string in
//! JSON, OpenAPI and Protocol Buffers (ADR-001). Parsing rejects any other
//! rendering, the nil UUID, and UUID versions other than 7.

use core::fmt;
use core::str::FromStr;

use uuid::Uuid;

use crate::error::{DomainError, DomainErrorKind};

/// UUID version required by ADR-001 for persistent and public identifiers.
const REQUIRED_UUID_VERSION: usize = 7;

fn validate_uuid_v7(value: Uuid, label: &'static str) -> Result<Uuid, DomainError> {
    if value.is_nil() {
        return Err(DomainError::new(
            DomainErrorKind::InvalidIdentifier,
            format!("{label} must not be the nil UUID"),
        ));
    }
    if value.get_version_num() != REQUIRED_UUID_VERSION {
        return Err(DomainError::new(
            DomainErrorKind::InvalidIdentifier,
            format!(
                "{label} must be a UUIDv{REQUIRED_UUID_VERSION}, found v{}",
                value.get_version_num()
            ),
        ));
    }
    Ok(value)
}

fn parse_uuid_v7(value: &str, label: &'static str) -> Result<Uuid, DomainError> {
    let parsed = Uuid::try_parse(value).map_err(|_| {
        DomainError::new(
            DomainErrorKind::InvalidIdentifier,
            format!("{label} is not a UUID"),
        )
    })?;
    if parsed.hyphenated().to_string() != value {
        return Err(DomainError::new(
            DomainErrorKind::InvalidIdentifier,
            format!("{label} must use the canonical lowercase hyphenated form"),
        ));
    }
    validate_uuid_v7(parsed, label)
}

macro_rules! define_id {
    ($(#[$attr:meta])* $name:ident, $label:literal) => {
        $(#[$attr])*
        ///
        /// Internally a UUIDv7 (ADR-001).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            /// Label used in error details and structured log fields.
            pub const LABEL: &'static str = $label;

            /// Generates a new time ordered identifier.
            ///
            /// The embedded timestamp is not the business creation time; a
            /// dedicated `created_at` column stays the source of truth
            /// (ADR-001).
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID.
            ///
            /// # Errors
            ///
            /// Returns [`DomainErrorKind::InvalidIdentifier`] when the value is
            /// the nil UUID or is not a UUIDv7.
            pub fn new(value: Uuid) -> Result<Self, DomainError> {
                validate_uuid_v7(value, $label).map(Self)
            }

            /// Parses a canonical lowercase hyphenated UUIDv7 string.
            ///
            /// # Errors
            ///
            /// Returns [`DomainErrorKind::InvalidIdentifier`] when the input is
            /// not a UUID, is not in canonical form, is the nil UUID, or is not
            /// a UUIDv7.
            pub fn parse(value: &str) -> Result<Self, DomainError> {
                parse_uuid_v7(value, $label).map(Self)
            }

            /// Returns the underlying UUID for adapter conversions.
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
    };
}

define_id!(
    /// Identifies a user account.
    UserId,
    "user_id"
);
define_id!(
    /// Identifies a role in the RBAC model.
    RoleId,
    "role_id"
);
define_id!(
    /// Identifies a world definition.
    WorldId,
    "world_id"
);
define_id!(
    /// Identifies a world instance.
    InstanceId,
    "instance_id"
);
define_id!(
    /// Identifies a generic entity inside an instance.
    EntityId,
    "entity_id"
);
define_id!(
    /// Identifies an identity session established by authentication.
    AuthSessionId,
    "auth_session_id"
);
define_id!(
    /// Identifies one signed access token through its `jti` claim.
    AccessTokenId,
    "access_token_id"
);
define_id!(
    /// Identifies a refresh-token rotation family.
    RefreshTokenFamilyId,
    "refresh_token_family_id"
);
define_id!(
    /// Identifies a realtime (WebSocket) connection.
    RealtimeConnectionId,
    "connection_id"
);
define_id!(
    /// Identifies a state-changing realtime command.
    ///
    /// Command IDs are client supplied UUIDv7 values.  They are distinct from
    /// envelope message IDs because a command may be replayed on another
    /// connection after reconnect.
    CommandId,
    "command_id"
);
define_id!(
    /// Identifies a presence record binding a user to an instance.
    PresenceId,
    "presence_id"
);

#[cfg(test)]
mod tests {
    use super::{InstanceId, UserId};
    use crate::error::DomainErrorKind;
    use uuid::Uuid;

    #[test]
    fn test_generate_produces_uuid_v7() {
        let id = UserId::generate();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn test_display_round_trips_through_parse() {
        let id = InstanceId::generate();
        let rendered = id.to_string();
        let parsed = InstanceId::parse(&rendered).expect("canonical form must parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn test_parse_rejects_uuid_v4() {
        let v4 = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").expect("valid v4 literal");
        let error = UserId::new(v4).expect_err("v4 must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidIdentifier);
    }

    #[test]
    fn test_parse_rejects_nil_uuid() {
        let error = UserId::new(Uuid::nil()).expect_err("nil must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidIdentifier);
    }

    #[test]
    fn test_parse_rejects_non_uuid_string() {
        let error = UserId::parse("not-a-uuid").expect_err("garbage must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidIdentifier);
    }

    #[test]
    fn test_parse_rejects_non_canonical_rendering() {
        let canonical = UserId::generate().to_string();
        let uppercase = canonical.to_uppercase();
        let simple = canonical.replace('-', "");
        assert_eq!(
            UserId::parse(&uppercase)
                .expect_err("uppercase must be rejected")
                .kind(),
            DomainErrorKind::InvalidIdentifier
        );
        assert_eq!(
            UserId::parse(&simple)
                .expect_err("unhyphenated must be rejected")
                .kind(),
            DomainErrorKind::InvalidIdentifier
        );
    }

    #[test]
    fn test_distinct_id_types_do_not_share_values() {
        let user = UserId::generate();
        let instance = InstanceId::new(user.as_uuid()).expect("same uuid is valid for both types");
        assert_eq!(user.as_uuid(), instance.as_uuid());
        assert_eq!(UserId::LABEL, "user_id");
        assert_eq!(InstanceId::LABEL, "instance_id");
    }
}
