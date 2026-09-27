//! Pure identity, credential, RBAC and session domain values.

use core::fmt;
use std::collections::BTreeSet;

use crate::{AuthSessionId, DomainError, DomainErrorKind, Revision, RoleId, Timestamp, UserId};

fn invalid(detail: impl Into<String>) -> DomainError {
    DomainError::new(DomainErrorKind::InvalidValue, detail)
}

/// Administrator-visible login identifier, distinct from immutable [`UserId`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LoginId(String);

impl LoginId {
    /// Creates a login identifier containing 1 through 128 non-control characters.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error for an empty, overlong, surrounding-whitespace,
    /// or control-character-containing identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let chars = value.chars().count();
        if !(1..=128).contains(&chars) {
            return Err(invalid("login_id must contain 1 through 128 characters"));
        }
        if value.trim() != value || value.chars().any(char::is_control) {
            return Err(invalid(
                "login_id must not contain control or surrounding whitespace",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Builds the login identifier for a server-generated subject (ADR-026 §3).
    ///
    /// The identifier is derived from the immutable [`UserId`], never from the
    /// display name, so two visitors who type the same name still get distinct
    /// identifiers and the `users.login_id` uniqueness constraint holds.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when `method` owns no reserved prefix,
    /// which is the case for [`AuthMethod::Local`].
    pub fn for_generated_subject(method: AuthMethod, user_id: UserId) -> Result<Self, DomainError> {
        let prefix = method
            .login_id_prefix()
            .ok_or_else(|| invalid("local accounts use an administrator-chosen login_id"))?;
        Self::new(format!("{prefix}{user_id}"))
    }

    /// Returns whether this identifier uses a server-reserved prefix.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        is_reserved_login_id(&self.0)
    }
}

impl fmt::Display for LoginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Login identifier prefixes reserved for server-generated subjects.
///
/// A subject created by ADR-026 gets a login identifier the server derives
/// from its own user id. Reserving the prefixes keeps an administrator from
/// creating an account that collides with, or impersonates, one of them
/// through `POST /v1/users`.
pub const RESERVED_LOGIN_ID_PREFIXES: &[&str] = &["guest:", "name:", "ext:"];

/// Returns whether `value` starts with a prefix reserved for the server.
///
/// ```
/// use orbisync_domain::is_reserved_login_id;
///
/// assert!(is_reserved_login_id("guest:0192d43d"));
/// assert!(!is_reserved_login_id("guestbook"));
/// ```
#[must_use]
pub fn is_reserved_login_id(value: &str) -> bool {
    RESERVED_LOGIN_ID_PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

/// How a subject proved who it is (ADR-026 §2).
///
/// The method is recorded for auditing and to decide lifetime handling. It is
/// deliberately not an input to authorization: every method converges on the
/// same `UserId` and the same RBAC lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthMethod {
    /// Local login identifier and password.
    Local,
    /// Anonymous participation with a server-generated display name.
    Guest,
    /// Participation carrying a visitor-supplied display name only.
    NameOnly,
    /// Signed token from a configured external issuer.
    External,
}

impl AuthMethod {
    /// Returns the stable wire and storage name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Guest => "guest",
            Self::NameOnly => "name_only",
            Self::External => "external",
        }
    }

    /// Parses a configured or stored method name.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error for any unrecognised name, so an
    /// unknown method can never be silently treated as an enabled one.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "local" => Ok(Self::Local),
            "guest" => Ok(Self::Guest),
            "name_only" => Ok(Self::NameOnly),
            "external" => Ok(Self::External),
            other => Err(invalid(format!("`{other}` is not a known auth method"))),
        }
    }

    /// Returns whether the method issues a temporary subject.
    ///
    /// Temporary subjects carry an absolute deadline and a world boundary;
    /// permanent ones keep the pre-ADR-026 lifetime rules unchanged.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::Guest | Self::NameOnly)
    }

    /// Returns the login identifier prefix for a server-generated subject.
    ///
    /// `None` for [`Self::Local`], whose identifier the administrator chooses.
    #[must_use]
    pub const fn login_id_prefix(self) -> Option<&'static str> {
        match self {
            Self::Local => None,
            Self::Guest => Some("guest:"),
            Self::NameOnly => Some("name:"),
            Self::External => Some("ext:"),
        }
    }
}

impl fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a stored user was created, mirroring `users.kind` (ADR-026 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UserKind {
    /// Administrator-created account with a password credential.
    #[default]
    Account,
    /// Temporary anonymous subject.
    Guest,
    /// Temporary subject carrying a visitor-supplied display name.
    NameOnly,
    /// Subject backed by an external issuer.
    External,
}

impl UserKind {
    /// Returns the stable storage name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Guest => "guest",
            Self::NameOnly => "name_only",
            Self::External => "external",
        }
    }

    /// Parses a stored `users.kind` value.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error for an unrecognised value rather than
    /// falling back to [`Self::Account`], which would grant a temporary
    /// subject the lifetime rules of a permanent one.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "account" => Ok(Self::Account),
            "guest" => Ok(Self::Guest),
            "name_only" => Ok(Self::NameOnly),
            "external" => Ok(Self::External),
            other => Err(invalid(format!("`{other}` is not a known user kind"))),
        }
    }

    /// Returns the kind a subject created by `method` is stored as.
    #[must_use]
    pub const fn for_method(method: AuthMethod) -> Self {
        match method {
            AuthMethod::Local => Self::Account,
            AuthMethod::Guest => Self::Guest,
            AuthMethod::NameOnly => Self::NameOnly,
            AuthMethod::External => Self::External,
        }
    }

    /// Returns whether a user of this kind holds a temporary subject row.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::Guest | Self::NameOnly)
    }
}

impl fmt::Display for UserKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// User account lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserStatus {
    /// Login and authorized operations are allowed.
    Active,
    /// Login and every existing session are rejected.
    Disabled,
}

/// User aggregate root without credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    id: UserId,
    login_id: LoginId,
    display_name: String,
    status: UserStatus,
    must_change_password: bool,
    roles: BTreeSet<RoleId>,
    created_at: Timestamp,
    updated_at: Timestamp,
    revision: Revision,
}

impl User {
    /// Creates an active user that must replace the temporary password.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when the display name is empty, over 128
    /// characters, or contains a control character.
    pub fn new(
        id: UserId,
        login_id: LoginId,
        display_name: impl Into<String>,
        now: Timestamp,
    ) -> Result<Self, DomainError> {
        let display_name = validate_display_name(display_name.into())?;
        Ok(Self {
            id,
            login_id,
            display_name,
            status: UserStatus::Active,
            must_change_password: true,
            roles: BTreeSet::new(),
            created_at: now,
            updated_at: now,
            revision: Revision::from_u64(1),
        })
    }

    /// Returns the immutable user identifier.
    #[must_use]
    pub const fn id(&self) -> UserId {
        self.id
    }

    /// Returns the login identifier.
    #[must_use]
    pub const fn login_id(&self) -> &LoginId {
        &self.login_id
    }

    /// Returns the display name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Returns the lifecycle state.
    #[must_use]
    pub const fn status(&self) -> UserStatus {
        self.status
    }

    /// Returns whether the temporary password should be changed (advisory).
    ///
    /// The server signals this condition but does not enforce it; clients
    /// should prompt for a password change when true (W-27 advisory decision).
    #[must_use]
    pub const fn must_change_password(&self) -> bool {
        self.must_change_password
    }

    /// Returns assigned role identifiers.
    #[must_use]
    pub const fn roles(&self) -> &BTreeSet<RoleId> {
        &self.roles
    }

    /// Returns the creation instant.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns the last update instant.
    #[must_use]
    pub const fn updated_at(&self) -> Timestamp {
        self.updated_at
    }

    /// Returns the optimistic-concurrency revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Replaces role assignments and advances the revision.
    ///
    /// # Errors
    ///
    /// Returns an overflow error when the revision cannot advance.
    pub fn replace_roles(
        &mut self,
        roles: impl IntoIterator<Item = RoleId>,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.roles = roles.into_iter().collect();
        self.touch(now)
    }

    /// Disables the account. Repeated calls are state-idempotent.
    ///
    /// # Errors
    ///
    /// Returns an overflow error when the revision cannot advance.
    pub fn disable(&mut self, now: Timestamp) -> Result<bool, DomainError> {
        if self.status == UserStatus::Disabled {
            return Ok(false);
        }
        self.status = UserStatus::Disabled;
        self.touch(now)?;
        Ok(true)
    }

    /// Enables the account. Repeated calls are state-idempotent.
    ///
    /// # Errors
    ///
    /// Returns an overflow error when the revision cannot advance.
    pub fn enable(&mut self, now: Timestamp) -> Result<bool, DomainError> {
        if self.status == UserStatus::Active {
            return Ok(false);
        }
        self.status = UserStatus::Active;
        self.touch(now)?;
        Ok(true)
    }

    /// Applies an administrative `PATCH` (`display_name` and/or `enabled`),
    /// advancing the revision at most once regardless of how many fields
    /// changed. Returns `false` (no revision bump) when every requested
    /// field already matched the current state.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error for a malformed `display_name`, or an
    /// overflow error when the revision cannot advance.
    pub fn apply_admin_patch(
        &mut self,
        display_name: Option<String>,
        enabled: Option<bool>,
        now: Timestamp,
    ) -> Result<bool, DomainError> {
        let mut changed = false;
        if let Some(display_name) = display_name {
            let display_name = validate_display_name(display_name)?;
            if display_name != self.display_name {
                self.display_name = display_name;
                changed = true;
            }
        }
        if let Some(enabled) = enabled {
            let target = if enabled {
                UserStatus::Active
            } else {
                UserStatus::Disabled
            };
            if self.status != target {
                self.status = target;
                changed = true;
            }
        }
        if changed {
            self.touch(now)?;
        }
        Ok(changed)
    }

    /// Marks the required initial password change complete.
    ///
    /// # Errors
    ///
    /// Returns an overflow error when the revision cannot advance.
    pub fn complete_password_change(&mut self, now: Timestamp) -> Result<(), DomainError> {
        self.must_change_password = false;
        self.touch(now)
    }

    /// Requires the user to change the password at next login (used for reset).
    ///
    /// # Errors
    ///
    /// Returns an overflow error when the revision cannot advance.
    pub fn require_password_change(&mut self, now: Timestamp) -> Result<(), DomainError> {
        self.must_change_password = true;
        self.touch(now)
    }

    /// Reconstitutes a persisted user (storage only, no validation).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn reconstitute(
        id: UserId,
        login_id: LoginId,
        display_name: String,
        status: UserStatus,
        must_change_password: bool,
        roles: BTreeSet<RoleId>,
        created_at: Timestamp,
        updated_at: Timestamp,
        revision: Revision,
    ) -> Self {
        Self {
            id,
            login_id,
            display_name,
            status,
            must_change_password,
            roles,
            created_at,
            updated_at,
            revision,
        }
    }

    fn touch(&mut self, now: Timestamp) -> Result<(), DomainError> {
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }
}

fn validate_display_name(value: String) -> Result<String, DomainError> {
    let chars = value.chars().count();
    if !(1..=128).contains(&chars) || value.chars().any(char::is_control) {
        return Err(invalid(
            "display_name must contain 1 through 128 non-control characters",
        ));
    }
    Ok(value)
}

/// Validates a visitor-supplied display name for name-only participation.
///
/// Stricter than [`validate_display_name`]: surrounding whitespace is trimmed
/// and a name that is only whitespace is rejected, so a blank entry cannot
/// become a participant whose name renders as nothing. The name is a label,
/// never an authentication factor or a permission — two visitors may hold the
/// same one and still be distinct subjects.
///
/// # Errors
///
/// Returns an invalid-value error for an empty, whitespace-only, overlong or
/// control-character-containing name.
pub fn validate_supplied_display_name(value: &str) -> Result<String, DomainError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("display_name must not be blank"));
    }
    validate_display_name(trimmed.to_owned())
}

/// Validated Argon2id PHC string that never reveals itself through formatting.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Validates and stores an Argon2id PHC string.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error for any non-Argon2id PHC string.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if !value.starts_with("$argon2id$v=19$") {
            return Err(invalid("password hash must be an Argon2id v=19 PHC string"));
        }
        Ok(Self(value))
    }

    /// Exposes the PHC string only to the credential adapter.
    #[must_use]
    pub fn expose_phc(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash([REDACTED])")
    }
}

impl fmt::Display for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Credential state, logically separate from [`User`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    user_id: UserId,
    password_hash: PasswordHash,
    password_changed_at: Timestamp,
    failed_login_count: u32,
    locked_until: Option<Timestamp>,
}

impl Credential {
    /// Creates credential state for a newly issued temporary password.
    #[must_use]
    pub const fn new(user_id: UserId, password_hash: PasswordHash, now: Timestamp) -> Self {
        Self {
            user_id,
            password_hash,
            password_changed_at: now,
            failed_login_count: 0,
            locked_until: None,
        }
    }

    /// Returns the owning user.
    #[must_use]
    pub const fn user_id(&self) -> UserId {
        self.user_id
    }

    /// Returns the secret hash value object.
    #[must_use]
    pub const fn password_hash(&self) -> &PasswordHash {
        &self.password_hash
    }

    /// Returns when the password was last replaced.
    #[must_use]
    pub const fn password_changed_at(&self) -> Timestamp {
        self.password_changed_at
    }

    /// Returns consecutive failed attempts.
    #[must_use]
    pub const fn failed_login_count(&self) -> u32 {
        self.failed_login_count
    }

    /// Returns the lock deadline.
    #[must_use]
    pub const fn locked_until(&self) -> Option<Timestamp> {
        self.locked_until
    }

    /// Returns whether the credential is locked at `now`.
    #[must_use]
    pub fn is_locked_at(&self, now: Timestamp) -> bool {
        self.locked_until.is_some_and(|deadline| now < deadline)
    }

    /// Login failure threshold (DM-05 / `auth-authorization.md:92`).
    pub const LOGIN_FAILURE_THRESHOLD: u32 = 5;
    /// Lockout duration after reaching the threshold (DM-05).
    pub const LOGIN_LOCKOUT_MILLIS: i64 = 15 * 60 * 1_000;

    /// Records a failed login and locks for fifteen minutes on the fifth failure.
    ///
    /// # Errors
    ///
    /// Returns an error if the timestamp cannot represent the lock deadline.
    pub fn record_failure(&mut self, now: Timestamp) -> Result<(), DomainError> {
        self.failed_login_count = self.failed_login_count.saturating_add(1);
        if self.failed_login_count >= Self::LOGIN_FAILURE_THRESHOLD {
            self.locked_until = Some(now.checked_add_millis(Self::LOGIN_LOCKOUT_MILLIS)?);
        }
        Ok(())
    }

    /// Clears failure state after a successful, unlocked authentication.
    pub fn record_success(&mut self) {
        self.failed_login_count = 0;
        self.locked_until = None;
    }

    /// Replaces the password and clears failure state.
    pub fn replace_password(&mut self, password_hash: PasswordHash, now: Timestamp) {
        self.password_hash = password_hash;
        self.password_changed_at = now;
        self.record_success();
    }

    /// Reconstitutes a persisted credential (storage only).
    #[must_use]
    pub fn reconstitute(
        user_id: UserId,
        password_hash: PasswordHash,
        password_changed_at: Timestamp,
        failed_login_count: u32,
        locked_until: Option<Timestamp>,
    ) -> Self {
        Self {
            user_id,
            password_hash,
            password_changed_at,
            failed_login_count,
            locked_until,
        }
    }
}

/// Namespaced allow-only permission.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Permission(String);

impl Permission {
    /// Creates a permission from lower-case dot-separated segments.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when the name has fewer than two valid segments.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let mut segments = value.split('.');
        let valid = segments.by_ref().all(|segment| {
            !segment.is_empty()
                && segment.chars().enumerate().all(|(index, character)| {
                    character.is_ascii_lowercase() || (index > 0 && character.is_ascii_digit())
                })
        });
        if !valid || value.matches('.').count() < 1 {
            return Err(invalid(
                "permission must contain lower-case dot-separated segments",
            ));
        }
        match value.split('.').next() {
            Some("admin" | "world" | "entity" | "moderation") => {}
            _ => return Err(invalid("permission namespace is not supported")),
        }
        Ok(Self(value))
    }

    /// Returns the stable permission name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_admin_namespace(&self) -> bool {
        self.0.starts_with("admin.")
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Role entity containing allow-only permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    id: RoleId,
    name: String,
    description: Option<String>,
    permissions: BTreeSet<Permission>,
    revision: Revision,
}

impl Role {
    /// Creates a role and de-duplicates its permissions.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error for an invalid name or description.
    pub fn new(
        id: RoleId,
        name: impl Into<String>,
        description: Option<String>,
        permissions: impl IntoIterator<Item = Permission>,
    ) -> Result<Self, DomainError> {
        let name = name.into();
        if !(1..=128).contains(&name.chars().count()) || name.chars().any(char::is_control) {
            return Err(invalid("role name must contain 1 through 128 characters"));
        }
        if description.as_ref().is_some_and(|value| {
            value.chars().count() > 1_024 || value.chars().any(char::is_control)
        }) {
            return Err(invalid("role description must not exceed 1024 characters"));
        }
        let permissions = permissions.into_iter().collect::<BTreeSet<_>>();
        let has_admin = permissions.iter().any(Permission::is_admin_namespace);
        let has_world = permissions
            .iter()
            .any(|permission| !permission.is_admin_namespace());
        if has_admin && has_world {
            return Err(invalid(
                "a role cannot mix admin and world permission namespaces",
            ));
        }
        Ok(Self {
            id,
            name,
            description,
            permissions,
            revision: Revision::from_u64(1),
        })
    }

    /// Returns the role identifier.
    #[must_use]
    pub const fn id(&self) -> RoleId {
        self.id
    }

    /// Returns the role name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the optional description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns the role permissions.
    #[must_use]
    pub const fn permissions(&self) -> &BTreeSet<Permission> {
        &self.permissions
    }

    /// Returns the optimistic-concurrency revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Updates mutable fields, enforcing optimistic concurrency.
    ///
    /// Mirrors `World::update`: only fields that are `Some` are changed, the
    /// namespace-mixing invariant enforced by `Role::new` is re-checked when
    /// `permissions` is replaced, and the revision advances by exactly one.
    ///
    /// # Errors
    ///
    /// Returns revision-mismatch or invalid-value (including the admin/world
    /// namespace-mixing rule).
    pub fn update(
        &mut self,
        expected_revision: Revision,
        name: Option<String>,
        description: Option<Option<String>>,
        permissions: Option<BTreeSet<Permission>>,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        if let Some(name) = name {
            if !(1..=128).contains(&name.chars().count()) || name.chars().any(char::is_control) {
                return Err(invalid("role name must contain 1 through 128 characters"));
            }
            self.name = name;
        }
        if let Some(description) = description {
            if description.as_ref().is_some_and(|value| {
                value.chars().count() > 1_024 || value.chars().any(char::is_control)
            }) {
                return Err(invalid("role description must not exceed 1024 characters"));
            }
            self.description = description;
        }
        if let Some(permissions) = permissions {
            let has_admin = permissions.iter().any(Permission::is_admin_namespace);
            let has_world = permissions
                .iter()
                .any(|permission| !permission.is_admin_namespace());
            if has_admin && has_world {
                return Err(invalid(
                    "a role cannot mix admin and world permission namespaces",
                ));
            }
            self.permissions = permissions;
        }
        self.revision = self.revision.next()?;
        Ok(())
    }

    /// Reconstitutes a persisted role (storage only, no validation).
    #[must_use]
    pub fn reconstitute(
        id: RoleId,
        name: String,
        description: Option<String>,
        permissions: BTreeSet<Permission>,
        revision: Revision,
    ) -> Self {
        Self {
            id,
            name,
            description,
            permissions,
            revision,
        }
    }
}

/// Identity session status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSessionStatus {
    /// Tokens may be validated and refreshed.
    Active,
    /// Every token in the session family is rejected.
    Revoked,
}

/// Identity session aggregate value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSession {
    id: AuthSessionId,
    user_id: UserId,
    status: AuthSessionStatus,
    created_at: Timestamp,
    expires_at: Timestamp,
    revoked_at: Option<Timestamp>,
}

impl AuthSession {
    /// Creates an active session.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error unless expiry is after creation.
    pub fn new(
        id: AuthSessionId,
        user_id: UserId,
        created_at: Timestamp,
        expires_at: Timestamp,
    ) -> Result<Self, DomainError> {
        if expires_at <= created_at {
            return Err(invalid("session expiry must be after creation"));
        }
        Ok(Self {
            id,
            user_id,
            status: AuthSessionStatus::Active,
            created_at,
            expires_at,
            revoked_at: None,
        })
    }

    /// Returns the session identifier.
    #[must_use]
    pub const fn id(&self) -> AuthSessionId {
        self.id
    }

    /// Returns the authenticated user.
    #[must_use]
    pub const fn user_id(&self) -> UserId {
        self.user_id
    }

    /// Returns the current status.
    #[must_use]
    pub const fn status(&self) -> AuthSessionStatus {
        self.status
    }

    /// Returns the creation time.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns the expiry time.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    /// Returns the revocation time.
    #[must_use]
    pub const fn revoked_at(&self) -> Option<Timestamp> {
        self.revoked_at
    }

    /// Returns whether validation is allowed at `now`.
    #[must_use]
    pub fn is_active_at(&self, now: Timestamp) -> bool {
        self.status == AuthSessionStatus::Active && now < self.expires_at
    }

    /// Revokes this session. Repeated calls are idempotent.
    pub fn revoke(&mut self, now: Timestamp) -> bool {
        if self.status == AuthSessionStatus::Revoked {
            return false;
        }
        self.status = AuthSessionStatus::Revoked;
        self.revoked_at = Some(now);
        true
    }

    /// Reconstitutes a persisted session (storage only).
    #[must_use]
    pub fn reconstitute(
        id: AuthSessionId,
        user_id: UserId,
        status: AuthSessionStatus,
        created_at: Timestamp,
        expires_at: Timestamp,
        revoked_at: Option<Timestamp>,
    ) -> Self {
        Self {
            id,
            user_id,
            status,
            created_at,
            expires_at,
            revoked_at,
        }
    }
}

/// Identity fact emitted after a successful state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityEvent {
    /// A user account was created.
    UserCreated {
        /// Created user.
        user_id: UserId,
    },
    /// A user account status changed.
    UserStatusChanged {
        /// Changed user.
        user_id: UserId,
        /// New server-owned status.
        status: UserStatus,
    },
    /// A user's role assignments were replaced.
    UserRolesReplaced {
        /// User whose assignments changed.
        user_id: UserId,
    },
    /// A session was revoked.
    SessionRevoked {
        /// Revoked session.
        session_id: AuthSessionId,
    },
    /// Reuse of a consumed refresh token was detected.
    RefreshTokenReuseDetected {
        /// Session family revoked due to reuse.
        session_id: AuthSessionId,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        AuthSession, Credential, LoginId, PasswordHash, Permission, Role, User, UserStatus,
    };
    use crate::{AuthSessionId, DomainErrorKind, Revision, RoleId, Timestamp, UserId};

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(1_000).expect("test timestamp")
    }

    #[test]
    fn user_and_role_invariants_are_enforced() {
        assert!(LoginId::new("").is_err());
        assert!(
            User::new(
                UserId::generate(),
                LoginId::new("ada").expect("valid"),
                "",
                now()
            )
            .is_err()
        );
        assert!(Permission::new("Admin.Users").is_err());
        let permission = Permission::new("admin.users.read").expect("valid");
        let role = Role::new(
            RoleId::generate(),
            "Reader",
            None,
            [permission.clone(), permission],
        )
        .expect("valid role");
        assert_eq!(role.permissions().len(), 1);
    }

    #[test]
    fn permissions_use_supported_namespaces_and_roles_do_not_mix_admin_access() {
        assert!(Permission::new("custom.read").is_err());
        let admin = Permission::new("admin.users.read").expect("valid admin permission");
        let world = Permission::new("world.join").expect("valid world permission");
        assert!(Role::new(RoleId::generate(), "Mixed", None, [admin, world]).is_err());
    }

    #[test]
    fn role_update_changes_only_provided_fields_and_advances_revision() {
        let read = Permission::new("admin.users.read").expect("valid");
        let mut role =
            Role::new(RoleId::generate(), "Reader", None, [read.clone()]).expect("valid role");
        let rev = role.revision();
        role.update(rev, Some("Renamed".to_owned()), None, None)
            .expect("update");
        assert_eq!(role.name(), "Renamed");
        assert_eq!(role.permissions().len(), 1);
        assert_eq!(role.revision(), Revision::from_u64(2));
    }

    #[test]
    fn role_update_can_clear_description_with_explicit_none() {
        let read = Permission::new("admin.users.read").expect("valid");
        let mut role = Role::new(
            RoleId::generate(),
            "Reader",
            Some("desc".to_owned()),
            [read],
        )
        .expect("valid role");
        let rev = role.revision();
        role.update(rev, None, Some(None), None).expect("update");
        assert_eq!(role.description(), None);
    }

    #[test]
    fn role_update_rejects_mixed_namespaces() {
        let admin = Permission::new("admin.users.read").expect("valid");
        let mut role = Role::new(RoleId::generate(), "Reader", None, [admin]).expect("valid role");
        let rev = role.revision();
        let world = Permission::new("world.join").expect("valid");
        let admin2 = Permission::new("admin.roles.read").expect("valid");
        let error = role
            .update(rev, None, None, Some([admin2, world].into_iter().collect()))
            .expect_err("mixed namespaces must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidValue);
    }

    #[test]
    fn role_update_rejects_revision_mismatch() {
        let admin = Permission::new("admin.users.read").expect("valid");
        let mut role = Role::new(RoleId::generate(), "Reader", None, [admin]).expect("valid role");
        let error = role
            .update(Revision::from_u64(99), Some("X".to_owned()), None, None)
            .expect_err("mismatched revision must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::RevisionMismatch);
    }

    #[test]
    fn login_failures_lock_on_the_fifth_attempt() {
        let hash = PasswordHash::new("$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA")
            .expect("shape is valid");
        let mut credential = Credential::new(UserId::generate(), hash, now());
        for _ in 0..4 {
            credential.record_failure(now()).expect("representable");
            assert!(!credential.is_locked_at(now()));
        }
        credential.record_failure(now()).expect("representable");
        assert!(credential.is_locked_at(now()));
        credential.record_success();
        assert_eq!(credential.failed_login_count(), 0);
    }

    #[test]
    fn status_transitions_are_idempotent() {
        let mut user = User::new(
            UserId::generate(),
            LoginId::new("ada").expect("valid"),
            "Ada",
            now(),
        )
        .expect("valid user");
        assert!(user.disable(now()).expect("revision advances"));
        assert!(!user.disable(now()).expect("idempotent"));
        assert_eq!(user.status(), UserStatus::Disabled);
    }

    #[test]
    fn admin_patch_advances_revision_at_most_once() {
        let mut user = User::new(
            UserId::generate(),
            LoginId::new("ada").expect("valid"),
            "Ada",
            now(),
        )
        .expect("valid user");
        let start_revision = user.revision();
        assert!(
            user.apply_admin_patch(Some("Ada Lovelace".to_owned()), Some(false), now())
                .expect("patch applies")
        );
        assert_eq!(user.display_name(), "Ada Lovelace");
        assert_eq!(user.status(), UserStatus::Disabled);
        assert_eq!(user.revision().as_u64(), start_revision.as_u64() + 1);
        // A no-op patch (same values) must not advance the revision again.
        assert!(
            !user
                .apply_admin_patch(Some("Ada Lovelace".to_owned()), Some(false), now())
                .expect("no-op patch")
        );
        assert_eq!(user.revision().as_u64(), start_revision.as_u64() + 1);
        assert!(
            User::new(
                UserId::generate(),
                LoginId::new("bob").expect("valid"),
                "Bob",
                now()
            )
            .expect("valid user")
            .apply_admin_patch(Some(String::new()), None, now())
            .is_err()
        );
    }

    #[test]
    fn password_hash_formatting_is_redacted() {
        let hash = PasswordHash::new("$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA")
            .expect("shape is valid");
        assert_eq!(hash.to_string(), "[REDACTED]");
        assert!(!format!("{hash:?}").contains("c2FsdA"));
    }

    #[test]
    fn session_expiry_and_revocation_are_enforced() {
        let created = now();
        let expires = created.checked_add_millis(1_000).expect("representable");
        let mut session = AuthSession::new(
            AuthSessionId::generate(),
            UserId::generate(),
            created,
            expires,
        )
        .expect("valid session");
        assert!(session.is_active_at(created));
        assert!(session.revoke(created));
        assert!(!session.revoke(created));
        assert!(!session.is_active_at(created));
    }
}

#[cfg(test)]
mod auth_method_tests {
    use super::{
        AuthMethod, LoginId, RESERVED_LOGIN_ID_PREFIXES, UserKind, is_reserved_login_id,
        validate_supplied_display_name,
    };
    use crate::UserId;

    #[test]
    fn generated_login_ids_are_derived_from_the_user_id_not_the_display_name() {
        // Two subjects that would share a display name must still get distinct
        // login identifiers, or the users.login_id unique constraint rejects
        // the second one.
        let first = UserId::generate();
        let second = UserId::generate();
        let a = LoginId::for_generated_subject(AuthMethod::NameOnly, first).expect("first");
        let b = LoginId::for_generated_subject(AuthMethod::NameOnly, second).expect("second");
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("name:"));
        assert!(a.as_str().ends_with(&first.to_string()));
    }

    #[test]
    fn local_accounts_have_no_generated_login_id() {
        LoginId::for_generated_subject(AuthMethod::Local, UserId::generate())
            .expect_err("local login ids are chosen by an administrator");
    }

    #[test]
    fn every_generated_login_id_uses_a_reserved_prefix() {
        for method in [
            AuthMethod::Guest,
            AuthMethod::NameOnly,
            AuthMethod::External,
        ] {
            let login = LoginId::for_generated_subject(method, UserId::generate()).expect("built");
            assert!(
                login.is_reserved(),
                "{method} must produce a reserved login id"
            );
        }
    }

    #[test]
    fn reserved_prefixes_match_whole_prefixes_only() {
        for prefix in RESERVED_LOGIN_ID_PREFIXES {
            assert!(is_reserved_login_id(&format!("{prefix}anything")));
        }
        // A name that merely starts with the same letters is not reserved.
        assert!(!is_reserved_login_id("guestbook"));
        assert!(!is_reserved_login_id("named"));
        assert!(!is_reserved_login_id("external-user"));
        assert!(!is_reserved_login_id("ada"));
    }

    #[test]
    fn unknown_method_and_kind_names_are_rejected() {
        AuthMethod::parse("sso").expect_err("unknown method must not parse");
        AuthMethod::parse("").expect_err("empty method must not parse");
        UserKind::parse("visitor").expect_err("unknown kind must not parse");
        // Falling back to Account here would hand a temporary subject the
        // lifetime rules of a permanent account.
        UserKind::parse("").expect_err("empty kind must not parse");
    }

    #[test]
    fn method_and_kind_names_round_trip() {
        for method in [
            AuthMethod::Local,
            AuthMethod::Guest,
            AuthMethod::NameOnly,
            AuthMethod::External,
        ] {
            assert_eq!(
                AuthMethod::parse(method.as_str()).expect("round trip"),
                method
            );
        }
        for kind in [
            UserKind::Account,
            UserKind::Guest,
            UserKind::NameOnly,
            UserKind::External,
        ] {
            assert_eq!(UserKind::parse(kind.as_str()).expect("round trip"), kind);
        }
    }

    #[test]
    fn only_guest_and_name_only_are_ephemeral() {
        assert!(AuthMethod::Guest.is_ephemeral());
        assert!(AuthMethod::NameOnly.is_ephemeral());
        assert!(!AuthMethod::Local.is_ephemeral());
        // External is backed by a durable upstream account, so it keeps the
        // pre-ADR-026 session lifetime rules.
        assert!(!AuthMethod::External.is_ephemeral());
        assert!(!UserKind::External.is_ephemeral());
        assert!(!UserKind::Account.is_ephemeral());
    }

    #[test]
    fn supplied_display_names_are_trimmed_and_blanks_rejected() {
        assert_eq!(
            validate_supplied_display_name("  Ada  ").expect("trimmed"),
            "Ada"
        );
        for rejected in ["", "   ", "\t\n", "\u{0}name"] {
            validate_supplied_display_name(rejected)
                .expect_err("blank or control display name must fail");
        }
        let overlong = "x".repeat(129);
        validate_supplied_display_name(&overlong).expect_err("129 characters must fail");
        validate_supplied_display_name(&"x".repeat(128)).expect("128 characters is allowed");
    }

    #[test]
    fn identical_display_names_do_not_imply_identical_subjects() {
        // The name is a label, not an identity: same text, different subject.
        let first = UserId::generate();
        let second = UserId::generate();
        let name = validate_supplied_display_name("Ada").expect("valid");
        assert_eq!(name, validate_supplied_display_name("Ada").expect("valid"));
        assert_ne!(
            LoginId::for_generated_subject(AuthMethod::NameOnly, first).expect("a"),
            LoginId::for_generated_subject(AuthMethod::NameOnly, second).expect("b")
        );
    }
}
