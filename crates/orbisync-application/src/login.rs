//! Login transaction port and audit types for P2-C4.
//!
//! The port provides the transactional boundary demanded by `auth-authorization.md:317-330`
//! and `observability-and-config.md:260-268`: on success the failure counter
//! reset, session, refresh token and audit must commit together; on failure
//! the failure counter increment and the audit must commit together, without
//! storing raw secrets or raw login identifiers.

use orbisync_domain::{
    AuthMethod, AuthSessionId, LoginId, PasswordHash, RoleId, Timestamp, UserId, UserKind,
};

use crate::{IdentityPortError, RequestId};

/// Session record persisted as part of a successful login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSessionRecord {
    /// Session identifier.
    pub id: AuthSessionId,
    /// Owning user.
    pub user_id: UserId,
    /// Creation instant.
    pub created_at: Timestamp,
    /// Expiry derived from `auth.refresh_token_ttl_seconds`.
    pub expires_at: Timestamp,
}

/// Refresh token record persisted as part of a successful login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRefreshRecord {
    /// Row identifier (`token_id`, UUIDv7 text).
    pub token_id: String,
    /// Family identifier (UUIDv7 text).
    pub family_id: String,
    /// HMAC-SHA-256 digest; raw material never crosses the port.
    pub digest: [u8; 32],
    /// Issue instant.
    pub issued_at: Timestamp,
    /// Expiry instant.
    pub expires_at: Timestamp,
}

/// Enumeration-resistant audit event for login success / failure.
///
/// The event never carries a password, token, or raw login identifier
/// (`auth-authorization.md:334`). Failures for unknown, wrong-password and
/// locked accounts share the same shape so that `actor` existence does not
/// leak account existence; the port implementation is responsible for keeping
/// the persisted form indistinguishable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAuditEvent {
    /// Event instant.
    pub occurred_at: Timestamp,
    /// Correlation identifier.
    pub request_id: RequestId,
    /// Client source IP resolved through the trusted-proxy filter, if available.
    pub source_ip: Option<String>,
    /// Machine-readable action, typically `auth.login`.
    pub action: &'static str,
    /// Authenticated actor for successes; `None` for all failures to preserve
    /// enumeration resistance.
    pub actor_id: Option<UserId>,
    /// `true` for success, `false` for failure.
    pub succeeded: bool,
}

/// Atomic success commit payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginCommit<'a> {
    /// Owning user.
    pub user_id: UserId,
    /// Session to insert.
    pub session: &'a LoginSessionRecord,
    /// Refresh token to insert.
    pub refresh: &'a LoginRefreshRecord,
    /// Success audit to insert in the same transaction.
    pub audit: &'a LoginAuditEvent,
    /// Expected failure count observed at `find_login` time.
    pub expected_failed_count: u32,
    /// Expected lock deadline observed at `find_login` time.
    pub expected_locked_until: Option<Timestamp>,
    /// Upgraded password hash when Argon2 parameters changed.
    pub new_password_hash: Option<&'a PasswordHash>,
}

/// The `users` row to insert for a subject seen for the first time.
///
/// Carries no credential material: guest, name-only and external subjects
/// never get a `user_credentials` row, which is what keeps them off the
/// password login path (`find_login` INNER JOINs that table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSubjectUser {
    /// Server-generated login identifier, derived from the user id.
    pub login_id: LoginId,
    /// Display name: server-generated for guests, visitor-supplied otherwise.
    pub display_name: String,
    /// How the subject was created, stored as `users.kind`.
    pub kind: UserKind,
    /// Creation instant.
    pub created_at: Timestamp,
}

/// Ledger row bounding the life of a temporary subject (ADR-026 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralSubjectRecord {
    /// Which temporary method issued the subject.
    pub method: AuthMethod,
    /// Creation instant.
    pub created_at: Timestamp,
    /// Absolute deadline. Written once and never extended, so repeated
    /// refresh cannot outlive it.
    pub expires_at: Timestamp,
    /// Worlds this subject may join, snapshotted from configuration at issue
    /// time. Never empty: an empty boundary is rejected during startup.
    pub allowed_worlds: Vec<uuid::Uuid>,
}

/// The `(issuer, subject)` pair an external subject was recognised by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentityRecord {
    /// Verified `iss` claim.
    pub issuer: String,
    /// Verified `sub` claim.
    pub subject: String,
    /// Creation instant.
    pub created_at: Timestamp,
}

/// Atomic commit payload for a subject that holds no password credential.
///
/// Kept separate from [`LoginCommit`] because that type carries the
/// credential-only fields (failure counter predicate, password rehash) which
/// have no meaning for these methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectCommit<'a> {
    /// The subject's user id, generated by the server.
    pub user_id: UserId,
    /// The `users` row to insert, or `None` when joining an existing subject
    /// (an external identity that has been seen before).
    pub new_user: Option<&'a NewSubjectUser>,
    /// Roles to grant, resolved from configuration at startup. Only applied
    /// when a new user row is created.
    pub grant_roles: &'a [RoleId],
    /// Temporary-subject ledger row; `None` for permanent subjects.
    pub ephemeral: Option<&'a EphemeralSubjectRecord>,
    /// External identity mapping; `None` for guest and name-only.
    pub external: Option<&'a ExternalIdentityRecord>,
    /// Session to insert.
    pub session: &'a LoginSessionRecord,
    /// Refresh token to insert.
    pub refresh: &'a LoginRefreshRecord,
    /// Success audit to insert in the same transaction.
    pub audit: &'a LoginAuditEvent,
}

/// Transactional boundary for login.
///
/// `commit_success` inserts session, refresh token and audit while resetting
/// the failure counter, all in one local transaction. `record_failure` atomically
/// increments the failure counter (when an account exists) and inserts a
/// failure audit. An audit-write failure must make the whole operation
/// fail-closed (`observability-and-config.md:5`).
#[async_trait::async_trait]
pub trait LoginTransactionStore: Send + Sync + 'static {
    /// Atomically commits a successful authentication.
    async fn commit_success(&self, commit: LoginCommit<'_>) -> Result<(), IdentityPortError>;
    /// Atomically records a failed authentication attempt.
    ///
    /// `user_id` is `Some` when the account exists (so the failure counter
    /// can be incremented) and `None` for unknown accounts. The persisted audit
    /// itself must remain enumeration-resistant and must not contain a raw login
    /// identifier; the port implementation must enforce that.
    async fn record_failure(
        &self,
        user_id: Option<UserId>,
        audit: LoginAuditEvent,
    ) -> Result<(), IdentityPortError>;

    /// Atomically issues a session for a subject that holds no password
    /// credential (ADR-026 §2).
    ///
    /// Inserts the `users` row, its role grants, the temporary-subject or
    /// external-identity ledger row, the session, the refresh token and the
    /// audit in one transaction. A partially created subject -- for instance a
    /// user row without its role grants -- would be indistinguishable from a
    /// subject an operator deliberately stripped of permissions, so the whole
    /// issue fails closed instead.
    /// An external pair already mapped to another user returns `Conflict`
    /// after rolling back the entire transaction. The caller must resolve the
    /// committed mapping and regenerate session/token material before retrying.
    async fn commit_subject_success(
        &self,
        commit: SubjectCommit<'_>,
    ) -> Result<(), IdentityPortError>;
}
