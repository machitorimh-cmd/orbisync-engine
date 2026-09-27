//! Transport-neutral identity commands, results and outbound ports.

use core::fmt;
use std::collections::BTreeSet;

use orbisync_domain::{
    AuthSession, Credential, LoginId, Permission, Role, RoleId, Timestamp, User, UserId,
};

use crate::ExtensionEvent;

/// Request correlation identifier defined by ADR-014.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Creates `req_<canonical UUIDv7>` after validating the prefix and UUID.
    ///
    /// # Errors
    ///
    /// Returns a domain error if the suffix is not canonical UUIDv7.
    pub fn new(value: impl Into<String>) -> Result<Self, orbisync_domain::DomainError> {
        let value = value.into();
        let suffix = value.strip_prefix("req_").ok_or_else(|| {
            orbisync_domain::DomainError::new(
                orbisync_domain::DomainErrorKind::InvalidIdentifier,
                "request_id must begin with req_",
            )
        })?;
        // Validate suffix is a canonical UUIDv7; discarding the parsed value is intentional
        // but we must not use `let _ =` on a must_use Result. Directly propagate error.
        UserId::parse(suffix)?;
        Ok(Self(value))
    }

    /// Returns the correlation identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Secret text that is redacted from `Debug` and `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps secret text without logging or normalizing it.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Exposes secret text only at an explicit cryptographic or transport boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Login command independent of HTTP.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginCommand {
    /// Administrator-visible login identifier.
    pub login_id: LoginId,
    /// Plain password, redacted from formatting.
    pub password: SecretString,
    /// Correlation identifier.
    pub request_id: RequestId,
}

impl fmt::Debug for LoginCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginCommand")
            .field("login_id", &self.login_id)
            .field("password", &"[REDACTED]")
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// Authenticated result returned to a transport adapter.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginResult {
    /// Authenticated user.
    pub user_id: UserId,
    /// Newly created session.
    pub session: AuthSession,
    /// Signed short-lived access token.
    pub access_token: SecretString,
    /// Opaque rotating refresh token.
    pub refresh_token: SecretString,
    /// Access-token lifetime in seconds.
    pub expires_in: u64,
}

impl fmt::Debug for LoginResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginResult")
            .field("user_id", &self.user_id)
            .field("session", &self.session)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// User creation command independent of HTTP or SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserCommand {
    /// Login identifier.
    pub login_id: LoginId,
    /// Display name.
    pub display_name: String,
    /// Authenticated administrator.
    pub actor_id: UserId,
    /// Request correlation identifier.
    pub request_id: RequestId,
}

/// Role creation command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRoleCommand {
    /// Role name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Allow-only permission set.
    pub permissions: BTreeSet<Permission>,
    /// Authenticated administrator.
    pub actor_id: UserId,
    /// Request correlation identifier.
    pub request_id: RequestId,
}

/// Role update command (PATCH /v1/roles/{role_id}), merge-patch semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRoleCommand {
    /// Role to update.
    pub role_id: RoleId,
    /// `If-Match` revision precondition.
    pub expected_revision: u64,
    /// New name, when present.
    pub name: Option<String>,
    /// New description; `Some(None)` clears it, `None` leaves it unchanged.
    pub description: Option<Option<String>>,
    /// New permission set (full replacement), when present.
    pub permissions: Option<BTreeSet<Permission>>,
    /// Authenticated administrator.
    pub actor_id: UserId,
    /// Request correlation identifier.
    pub request_id: RequestId,
}

/// Complete account state returned by the login lookup port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAccount {
    /// User aggregate.
    pub user: User,
    /// Logically separate credential.
    pub credential: Credential,
}

/// Secret-free outbound adapter failure with three-way separation (V-04).
///
/// Boundaries are documented so future `map_err` call sites do not repeat the
/// bulk-replace misclassification. Each variant documents its HTTP status and
/// whether retry may succeed:
/// - `InvalidRequest` (400 `INVALID_REQUEST`): client-supplied input is
///   syntactically invalid (malformed cursor / UUID / pagination params).
///   Never retry as-is; client must fix the request.
/// - `DataCorruption` (500 `INTERNAL_ERROR`): persisted data or a
///   server-generated invariant is violated (DB row has wrong type, revision
///   negative, `status_code` does not fit in `i16`, write-side `try_from`
///   failure). Covers both read-side and write-side integrity violations.
///   Retrying will not succeed; operator intervention is required.
/// - `Unavailable` (503 `SERVICE_UNAVAILABLE`): storage is transiently
///   unreachable (pool, network, timeout). Retrying may succeed.
/// - `NotFound` (404 `RESOURCE_NOT_FOUND`): requested resource does not exist.
/// - `Conflict` (409 `RESOURCE_CONFLICT`): state conflict, including revision
///   mismatch for optimistic lock (W-G).
///
/// Transport maps these to `INVALID_REQUEST` / `INTERNAL_ERROR` /
/// `SERVICE_UNAVAILABLE` / `RESOURCE_NOT_FOUND` / `RESOURCE_CONFLICT`
/// respectively. `DataCorruption` is deliberately 500 rather than 503:
/// signalling retry for a broken invariant is misleading.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityPortError {
    /// クライアント由来の不正入力。cursor / identifier / pagination パラメータなど。HTTP 400。
    #[error("invalid request")]
    InvalidRequest,
    /// 保存データまたは内部不変条件の破れ。リトライしても回復しない。HTTP 500。
    #[error("identity store data is corrupted")]
    DataCorruption,
    /// ストアが一時的に利用できない。リトライで回復しうる。HTTP 503。
    #[error("identity store unavailable")]
    Unavailable,
    /// Requested resource does not exist. HTTP 404.
    #[error("resource not found")]
    NotFound,
    /// State conflict, including revision mismatch. HTTP 409.
    #[error("resource conflict")]
    Conflict,
}

/// Outcome of an atomic login failure increment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFailureOutcome {
    /// New consecutive failure count after increment.
    pub failed_login_count: u32,
    /// New lock deadline, if the threshold was reached.
    pub locked_until: Option<Timestamp>,
}

/// Repository port for identity aggregates.
#[async_trait::async_trait]
pub trait IdentityRepository: Send + Sync + 'static {
    /// Performs the single credential lookup required by the login path.
    async fn find_login(
        &self,
        login_id: &LoginId,
    ) -> Result<Option<LoginAccount>, IdentityPortError>;

    /// Loads an account by its immutable user identifier.
    async fn find_account(
        &self,
        user_id: UserId,
    ) -> Result<Option<LoginAccount>, IdentityPortError>;

    /// Persists login failure/success state.
    async fn save_credential(&self, credential: &Credential) -> Result<(), IdentityPortError>;

    /// Atomically increments `failed_login_count` and conditionally sets
    /// `locked_until` when the DM-05 threshold is reached (C2).
    ///
    /// The calculation `failed_login_count + 1` and the `CASE` for
    /// `locked_until` are performed in a single `UPDATE ... RETURNING` so
    /// concurrent failures cannot be lost. The threshold (5) and lock
    /// duration (15 min) are the domain constants
    /// `Credential::LOGIN_FAILURE_THRESHOLD` / `LOGIN_LOCKOUT_MILLIS`
    /// (`domain-model.md` DM-05, `auth-authorization.md:92`); no config key
    /// exists, so the value is deliberately hard-coded consistently.
    async fn record_login_failure(
        &self,
        user_id: UserId,
        now: Timestamp,
    ) -> Result<LoginFailureOutcome, IdentityPortError>;

    /// Conditionally clears failure state after a successful authentication
    /// using an optimistic predicate (C2 success path).
    ///
    /// The `UPDATE` only clears `failed_login_count`/`locked_until` when
    /// the row still matches `expected_failed_count` and
    /// `expected_locked_until`. This prevents a successful login from
    /// wiping failures that were recorded after the initial `find_login`
    /// read. The method holds no lock during Argon2 verification; the
    /// predicate is checked atomically at `UPDATE` time. Returns `true`
    /// when the row was updated, `false` when a concurrent modification
    /// prevented the reset (the login still succeeds).
    async fn reset_login_success(
        &self,
        user_id: UserId,
        expected_failed_count: u32,
        expected_locked_until: Option<Timestamp>,
        now: Timestamp,
        new_password_hash: Option<orbisync_domain::PasswordHash>,
    ) -> Result<bool, IdentityPortError>;

    /// Loads all roles assigned to a user.
    async fn roles_for_user(&self, user_id: UserId) -> Result<Vec<Role>, IdentityPortError>;

    /// Loads a session for access-token validation.
    async fn find_session(
        &self,
        session_id: orbisync_domain::AuthSessionId,
    ) -> Result<Option<AuthSession>, IdentityPortError>;

    /// Persists a session created or revoked by an identity use case.
    async fn save_session(&self, session: &AuthSession) -> Result<(), IdentityPortError>;
}

/// Identity mutation paired with its mandatory audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityMutation {
    /// Creates the first administrator, credential, roles, and assignments atomically.
    BootstrapAdministrator {
        /// Initial administrator account.
        user: User,
        /// Initial temporary credential.
        credential: Credential,
        /// Initial namespace-separated roles.
        roles: Vec<Role>,
    },
    /// Creates a user and initial credential atomically.
    CreateUser {
        /// New user aggregate.
        user: User,
        /// Initial credential aggregate.
        credential: Credential,
    },
    /// Replaces role assignments.
    ReplaceUserRoles {
        /// Target user.
        user_id: UserId,
        /// Complete replacement assignment set.
        role_ids: BTreeSet<RoleId>,
    },
    /// Stores a role.
    StoreRole {
        /// Role to insert or update.
        role: Role,
    },
    /// Updates an existing role after an optimistic-concurrency check.
    ///
    /// Unlike `StoreRole` (a blind `INSERT ... ON CONFLICT DO UPDATE` used only
    /// for brand-new roles), `UpdateRole` must be rejected when the row does
    /// not already exist or its current revision does not match
    /// `role.revision() - 1` (the domain has already advanced `role`'s
    /// revision by exactly one via `Role::update`). This mirrors the
    /// `StoreUserStatus` / `StorePasswordChange` convention of deriving the
    /// expected previous revision from the new value rather than carrying a
    /// redundant field.
    UpdateRole {
        /// Role with fields already validated and revision already advanced.
        role: Role,
    },
    /// Stores changed user status and revokes sessions when disabled.
    StoreUserStatus {
        /// Updated user aggregate.
        user: User,
    },
    /// Stores a `PATCH /v1/users/{user_id}` update (`display_name` and/or
    /// `enabled`) under an explicit `If-Match` optimistic lock, revoking
    /// active sessions when the update disables the account (same invariant
    /// as `StoreUserStatus`).
    UpdateUserProfile {
        /// Fully-updated user aggregate to persist.
        user: User,
        /// Revision the caller observed via `If-Match`; a mismatch is `409`.
        expected_revision: u64,
    },
    /// Replaces a credential after password change or reset.
    StoreCredential {
        /// Updated credential aggregate.
        credential: Credential,
    },
    /// Records a failed password-change attempt for audit (no state mutation).
    PasswordChangeRejected {
        /// User whose change was rejected.
        user_id: UserId,
    },
    /// Atomically stores a changed password and clears the user's forced-change flag.
    ///
    /// This mutation updates `users.must_change_password`, `revision` and
    /// `updated_at` together with `user_credentials.password_hash`,
    /// `password_changed_at`, and resets `failed_login_count`/`locked_until`
    /// in a single local transaction (ADR-017).
    StorePasswordChange {
        /// Updated user aggregate with `must_change_password = false`.
        user: User,
        /// Updated credential aggregate with new hash and cleared lock state.
        credential: Credential,
    },
    /// Admin-initiated password reset – sets `must_change_password = true`,
    /// replaces credential hash, clears `failed_login_count`/`locked_until`,
    /// and revokes all active sessions atomically (ADR-002 / ADR-017).
    ///
    /// # 失敗回数とロック状態の扱いの判断
    ///
    /// リセットで `failed_login_count` と `locked_until` をクリアするのが妥当と判断した。
    /// 理由: パスワードリセットは管理者による正規の回復経路であり、攻撃者による
    /// ブルートフォースでロックされたアカウントでも運用者が復旧できる必要がある。
    /// リセット後もロックを維持すると、正当な利用者が一時パスワードでもログインできず、
    /// 復旧手段が存在しない状態が続く。Credential::replace_password は内部で
    /// record_success() を呼び失敗状態をクリアするため、この挙動に一致する。
    ResetPassword {
        /// Updated user aggregate with `must_change_password = true`.
        user: User,
        /// Updated credential aggregate with new hash and cleared lock state.
        credential: Credential,
    },
    /// Stores a created or revoked authentication session.
    StoreSession {
        /// Updated session aggregate.
        session: AuthSession,
    },
    /// Deletes a role and its assignments.
    ///
    /// # Assigned-role decision (W-G)
    ///
    /// When a role is still assigned to users, this mutation **cascades** the delete
    /// by removing the corresponding `user_roles` rows in the same transaction.
    /// Rationale: `admin.roles.delete` is a privileged operation; requiring the caller
    /// to manually unassign all users first adds operational friction without security
    /// benefit (the caller already holds delete privilege), and the historic
    /// implementation (`crates/orbisync-storage-postgres/src/identity.rs:605`) always
    /// cascaded. The alternative of rejecting with 409 would leave orphaned roles that
    /// cannot be removed without enumerating assignments, increasing complexity.
    /// This decision is recorded here so that W-27 / CR-20 auditing is preserved.
    DeleteRole {
        /// Role to remove.
        role_id: RoleId,
        /// Expected revision from `If-Match` for optimistic lock.
        expected_revision: u64,
    },
    /// Revokes every session belonging to a user.
    RevokeUserSessions {
        /// User whose sessions are revoked.
        user_id: UserId,
        /// Revocation instant.
        occurred_at: Timestamp,
    },
}

/// Detailed mandatory identity audit entry without secret fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityAuditEvent {
    /// Event timestamp.
    pub occurred_at: Timestamp,
    /// Authenticated actor.
    pub actor_id: Option<UserId>,
    /// Stable machine-readable action.
    pub action: &'static str,
    /// Target resource identifier, when one exists.
    pub resource_id: Option<String>,
    /// Request correlation identifier.
    pub request_id: RequestId,
    /// Operation result.
    pub succeeded: bool,
}

/// Use-case-specific atomic identity mutation port from ADR-017.
#[async_trait::async_trait]
pub trait IdentityAdministrationStore: Send + Sync + 'static {
    /// Applies one enumerated mutation and its audit insert in one local transaction.
    async fn apply(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
    ) -> Result<(), IdentityPortError>;

    /// Applies a mutation and audit while carrying the already-resolved
    /// client source IP.  The default keeps adapters that do not persist the
    /// optional audit metadata source-compatible.
    async fn apply_with_source_ip(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), IdentityPortError> {
        let _ = source_ip;
        self.apply(mutation, audit).await
    }

    /// Applies an identity mutation, audit row and extension outbox event in
    /// one local transaction when the adapter supports durable outbox writes.
    ///
    /// The default keeps existing in-memory adapters source-compatible; the
    /// PostgreSQL adapter overrides it to preserve atomicity.
    async fn apply_with_event(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        _event: ExtensionEvent,
    ) -> Result<(), IdentityPortError> {
        self.apply(mutation, audit).await
    }

    /// Applies a mutation, audit, extension event and source IP atomically.
    async fn apply_with_event_and_source_ip(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        event: ExtensionEvent,
        source_ip: Option<String>,
    ) -> Result<(), IdentityPortError> {
        let _ = source_ip;
        self.apply_with_event(mutation, audit, event).await
    }

    /// Atomically applies a mutation, its audit, and transitions an
    /// `in_progress` idempotency record to `completed` in one transaction.
    ///
    /// This is used for password reset / change where the temporary
    /// credential must not be lost if `complete` would otherwise fail
    /// separately (AUD-C2). If the idempotency update affects 0 rows,
    /// the whole transaction is rolled back and `Unavailable` is returned.
    /// P2-C2 adds owner-checked completion: only the claim owner may
    /// complete.
    async fn apply_with_idempotency(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        _idempotency_key: String,
        _idempotency_owner: String,
        _completion: IdempotencyCompletion,
    ) -> Result<(), IdentityPortError> {
        // Default non-atomic fallback: apply mutation then try to complete
        // via a separate step is not atomic, but provided so existing
        // fakes that don't override still compile. Production Postgres
        // overrides this to do a single transaction.
        self.apply(mutation, audit).await?;
        // Without access to the idempotency table, we cannot complete
        // atomically; signal that the caller must handle idempotency
        // separately. Returning Unavailable forces the HTTP layer to
        // return 5xx and not treat it as success, which is fail-closed.
        Err(IdentityPortError::Unavailable)
    }

    /// Atomically applies an idempotent mutation while carrying source IP
    /// metadata for the audit row.
    async fn apply_with_idempotency_and_source_ip(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        idempotency_key: String,
        idempotency_owner: String,
        completion: IdempotencyCompletion,
        source_ip: Option<String>,
    ) -> Result<(), IdentityPortError> {
        let _ = source_ip;
        self.apply_with_idempotency(
            mutation,
            audit,
            idempotency_key,
            idempotency_owner,
            completion,
        )
        .await
    }
}

/// Response bytes stored for idempotent replay (AUD-C1).
///
/// Historically named `EncryptedResponse` but it does **not** encrypt.
/// Since AUD-C1, temporary passwords are never persisted; `response_body`
/// contains only non-secret data (e.g., `must_change_password`). Do not
/// put secrets (temporary_password, token digests) into this wrapper –
/// a hygiene gate verifies this.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedResponse(Vec<u8>);

impl EncryptedResponse {
    /// Wraps bytes for idempotent replay.
    ///
    /// The bytes are stored as-is in `idempotency_records.response_body`.
    /// Callers must ensure the payload contains no secrets (AUD-C1).
    #[must_use]
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    /// Exposes ciphertext to a persistence adapter.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the wrapper and returns ciphertext.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Debug for EncryptedResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EncryptedResponse([REDACTED])")
    }
}

/// Input used to atomically claim an idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyClaimCommand {
    /// Canonical UUID key text.
    pub key: String,
    /// Authenticated actor, when present.
    pub actor_user_id: Option<UserId>,
    /// Stable operation name.
    pub operation: String,
    /// Digest of the canonical request bytes.
    pub request_hash: [u8; 32],
    /// Claim time used for deterministic expiry handling.
    pub now: Timestamp,
}

/// A completed idempotent response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyCompletion {
    /// HTTP status retained as neutral numeric metadata.
    pub status_code: u16,
    /// Response media type.
    pub response_content_type: String,
    /// Application-encrypted response ciphertext.
    pub response: EncryptedResponse,
}

/// Result of claiming an idempotency key (P2-C2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyClaim {
    /// The caller owns a new in-progress record with lease.
    Acquired {
        /// Random owner token that must be presented on complete/abandon.
        owner: String,
        /// Lease expiry; after this the key may be stolen by another owner.
        lease_until: Timestamp,
    },
    /// The same request is still executing; retry after the lease.
    InProgress {
        /// Seconds until lease expiry for `Retry-After`.
        retry_after_secs: u64,
    },
    /// The same request has a replayable completed response.
    Completed(IdempotencyCompletion),
    /// The key belongs to a different actor, operation, or request digest.
    Reused,
}

/// Idempotency record port keyed by UUIDv7 text supplied by an adapter.
#[async_trait::async_trait]
pub trait IdempotencyStore: Send + Sync + 'static {
    /// Atomically claims a key or returns its existing state.
    async fn claim(
        &self,
        command: IdempotencyClaimCommand,
    ) -> Result<IdempotencyClaim, IdentityPortError>;

    /// Transitions an owned in-progress record to completed (owner-checked).
    async fn complete(
        &self,
        key: String,
        owner: String,
        completion: IdempotencyCompletion,
    ) -> Result<(), IdentityPortError>;

    /// Abandons an owned in-progress record (owner-checked, P2-C2).
    ///
    /// Deletes the in-progress row so the same key can be retried. Only the
    /// owner may abandon; unconditional DELETE is forbidden.
    async fn abandon(&self, key: String, owner: String) -> Result<(), IdentityPortError>;
}

/// Digest-only replacement supplied to refresh rotation persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshTokenReplacement {
    /// New token-row identifier.
    pub token_id: String,
    /// HMAC-SHA-256 digest; raw token material is forbidden here.
    pub digest: [u8; 32],
    /// Issue timestamp.
    pub issued_at: Timestamp,
    /// Expiry timestamp.
    pub expires_at: Timestamp,
}

/// Atomic refresh rotation command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateRefreshTokenCommand {
    /// Presented HMAC digest, never the raw token.
    pub presented_digest: [u8; 32],
    /// Replacement token metadata.
    pub replacement: RefreshTokenReplacement,
    /// Rotation timestamp.
    pub now: Timestamp,
    /// Correlation identifier for the mandatory audit record.
    pub request_id: RequestId,
}

/// Result of an atomic refresh rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRotationResult {
    /// The token was consumed and replaced.
    Rotated {
        /// Authenticated user owning the session.
        user_id: UserId,
        /// Session whose refresh token was rotated.
        session_id: orbisync_domain::AuthSessionId,
        /// Absolute temporary-subject deadline; absent for permanent subjects.
        absolute_deadline: Option<Timestamp>,
    },
    /// A consumed token was reused and its whole session family was revoked.
    ReuseDetected,
    /// The digest was unknown, expired, or attached to an inactive session.
    Rejected,
}

/// Initial refresh token creation command for login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRefreshTokenCommand {
    /// New token-row identifier (canonical UUIDv7 text).
    pub token_id: String,
    /// Family identifier (canonical UUIDv7 text).
    pub family_id: String,
    /// Session to which the token belongs.
    pub session_id: orbisync_domain::AuthSessionId,
    /// HMAC-SHA-256 digest; raw token material is forbidden here.
    pub digest: [u8; 32],
    /// Issue timestamp.
    pub issued_at: Timestamp,
    /// Expiry timestamp derived from `auth.refresh_token_ttl_seconds`.
    pub expires_at: Timestamp,
}

/// Persistence port for initial refresh-token insertion (login).
#[async_trait::async_trait]
pub trait RefreshTokenCreationStore: Send + Sync + 'static {
    /// Inserts the first refresh token for a newly created session.
    async fn create(&self, command: CreateRefreshTokenCommand) -> Result<(), IdentityPortError>;
}

/// Persistence port for row-locked, audited refresh-token rotation.
#[async_trait::async_trait]
pub trait RefreshTokenRotationStore: Send + Sync + 'static {
    /// Rotates using only keyed digests and rolls back when audit insertion fails.
    async fn rotate(
        &self,
        command: RotateRefreshTokenCommand,
    ) -> Result<RefreshRotationResult, IdentityPortError>;

    /// Rotates a refresh token while carrying the already-resolved client
    /// source IP for the audit record.
    async fn rotate_with_source_ip(
        &self,
        command: RotateRefreshTokenCommand,
        source_ip: Option<String>,
    ) -> Result<RefreshRotationResult, IdentityPortError> {
        let _ = source_ip;
        self.rotate(command).await
    }
}

/// Command to persist a single-use realtime ticket digest (C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRealtimeTicketCommand {
    /// HMAC-SHA-256 digest of the opaque ticket (32 bytes).
    pub token_digest: [u8; 32],
    /// Session the ticket is bound to.
    pub session_id: orbisync_domain::AuthSessionId,
    /// User that owns the session.
    pub user_id: UserId,
    /// Issue timestamp.
    pub issued_at: Timestamp,
    /// Expiry timestamp (issued_at + 60s).
    pub expires_at: Timestamp,
}

/// Result of consuming a realtime ticket (C1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeTicketConsumption {
    /// Ticket was consumed and the session is valid.
    Consumed {
        /// Ticket owner.
        user_id: UserId,
        /// Session that issued the ticket.
        session_id: orbisync_domain::AuthSessionId,
    },
    /// Ticket is unknown, already consumed, expired or session inactive.
    Rejected,
}

/// Port for realtime ticket lifecycle (C1: single-use, session-aware).
///
/// The raw ticket value never crosses this port; only its HMAC digest does.
#[async_trait::async_trait]
pub trait RealtimeTicketStore: Send + Sync + 'static {
    /// Inserts a new ticket digest (called at ticket issuance).
    async fn create(&self, command: CreateRealtimeTicketCommand) -> Result<(), IdentityPortError>;

    /// Atomically consumes the ticket if `token_digest` exists, is not expired
    /// and its session is `active` and not expired. One statement:
    /// `DELETE FROM realtime_tickets USING auth_sessions ... RETURNING`.
    /// Returns `Consumed` on success, `Rejected` otherwise.
    async fn consume(
        &self,
        token_digest: [u8; 32],
        now: Timestamp,
    ) -> Result<RealtimeTicketConsumption, IdentityPortError>;
}

/// Contract implemented by the identity login service.
#[async_trait::async_trait]
pub trait LoginUseCase: Send + Sync {
    /// Authenticates and returns a new session and token pair.
    async fn login(&self, command: LoginCommand) -> Result<LoginResult, crate::ApplicationError>;
}

/// Contract implemented by user administration.
#[async_trait::async_trait]
pub trait UserAdministrationUseCase: Send + Sync {
    /// Creates a user with a server-generated temporary credential.
    async fn create_user(
        &self,
        command: CreateUserCommand,
    ) -> Result<(User, SecretString), crate::ApplicationError>;
}
#[cfg(test)]
mod tests {
    use super::SecretString;

    #[test]
    fn secret_formatting_is_redacted() {
        let secret = SecretString::new("actual-token-value");
        assert_eq!(secret.to_string(), "[REDACTED]");
        assert!(!format!("{secret:?}").contains("actual-token-value"));
    }
}
