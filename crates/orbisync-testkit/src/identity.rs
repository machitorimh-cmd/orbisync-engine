//! In-memory identity ports for deterministic use-case tests.

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

use orbisync_application::{
    AuditFilter, AuditQueryPort, AuditView, CreateRealtimeTicketCommand, IdempotencyClaim,
    IdempotencyClaimCommand, IdempotencyCompletion, IdempotencyStore, IdentityAdministrationStore,
    IdentityAuditEvent, IdentityMutation, IdentityPortError, IdentityQueryPort, IdentityRepository,
    LoginAccount, LoginAuditEvent, LoginCommit, LoginRefreshRecord, LoginTransactionStore, Page,
    PageRequest, QueryCursor, RoleView, UserView, pagination::CursorCodec,
};
use orbisync_domain::{
    AuthSession, AuthSessionId, Credential, LoginId, PasswordHash, Role, RoleId, Timestamp, User,
    UserId, UserStatus,
};

/// In-memory implementation of [`IdentityRepository`].
#[derive(Debug, Default)]
pub struct FakeIdentityRepository {
    accounts: Mutex<HashMap<String, LoginAccount>>,
    sessions: Mutex<HashMap<AuthSessionId, AuthSession>>,
    roles: Mutex<HashMap<UserId, Vec<Role>>>,
}

impl FakeIdentityRepository {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a login account.
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    pub fn insert_account(&self, account: LoginAccount) {
        self.accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"))
            .insert(account.user.login_id().as_str().to_owned(), account);
    }

    /// Assigns the server-owned roles returned for a user.
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    pub fn set_roles(&self, user_id: UserId, roles: Vec<Role>) {
        self.roles
            .lock()
            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
            .insert(user_id, roles);
    }
}

#[async_trait::async_trait]
impl IdentityRepository for FakeIdentityRepository {
    async fn find_login(
        &self,
        login_id: &LoginId,
    ) -> Result<Option<LoginAccount>, IdentityPortError> {
        Ok(self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"))
            .get(login_id.as_str())
            .cloned())
    }

    async fn find_account(
        &self,
        user_id: UserId,
    ) -> Result<Option<LoginAccount>, IdentityPortError> {
        Ok(self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"))
            .values()
            .find(|a| a.user.id() == user_id)
            .cloned())
    }

    async fn save_credential(&self, credential: &Credential) -> Result<(), IdentityPortError> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        if let Some(account) = accounts
            .values_mut()
            .find(|account| account.user.id() == credential.user_id())
        {
            account.credential = credential.clone();
        }
        Ok(())
    }

    async fn record_login_failure(
        &self,
        user_id: UserId,
        now: Timestamp,
    ) -> Result<orbisync_application::LoginFailureOutcome, IdentityPortError> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        let account = accounts
            .values_mut()
            .find(|a| a.user.id() == user_id)
            .ok_or(IdentityPortError::NotFound)?;
        let current = account.credential.failed_login_count();
        let next = current.saturating_add(1);
        let new_locked = if next >= Credential::LOGIN_FAILURE_THRESHOLD {
            Some(
                now.checked_add_millis(Credential::LOGIN_LOCKOUT_MILLIS)
                    .map_err(|_| IdentityPortError::DataCorruption)?,
            )
        } else {
            account.credential.locked_until()
        };
        account.credential = Credential::reconstitute(
            user_id,
            account.credential.password_hash().clone(),
            account.credential.password_changed_at(),
            next,
            new_locked,
        );
        Ok(orbisync_application::LoginFailureOutcome {
            failed_login_count: next,
            locked_until: new_locked,
        })
    }

    async fn reset_login_success(
        &self,
        user_id: UserId,
        expected_failed_count: u32,
        expected_locked_until: Option<Timestamp>,
        _now: Timestamp,
        new_password_hash: Option<PasswordHash>,
    ) -> Result<bool, IdentityPortError> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        let account = accounts
            .values_mut()
            .find(|a| a.user.id() == user_id)
            .ok_or(IdentityPortError::NotFound)?;
        if account.credential.failed_login_count() != expected_failed_count
            || account.credential.locked_until() != expected_locked_until
        {
            return Ok(false);
        }
        let pwd_changed = account.credential.password_changed_at();
        let has_new = new_password_hash.is_some();
        let new_hash =
            new_password_hash.unwrap_or_else(|| account.credential.password_hash().clone());
        let new_changed = if has_new { _now } else { pwd_changed };
        account.credential = Credential::reconstitute(user_id, new_hash, new_changed, 0, None);
        Ok(true)
    }

    async fn roles_for_user(&self, user_id: UserId) -> Result<Vec<Role>, IdentityPortError> {
        Ok(self
            .roles
            .lock()
            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
            .get(&user_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn find_session(
        &self,
        session_id: AuthSessionId,
    ) -> Result<Option<AuthSession>, IdentityPortError> {
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"))
            .get(&session_id)
            .cloned())
    }

    async fn save_session(&self, session: &AuthSession) -> Result<(), IdentityPortError> {
        self.sessions
            .lock()
            .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"))
            .insert(session.id(), session.clone());
        Ok(())
    }
}

/// Captures enumerated identity mutations and their mandatory audit records.
#[derive(Debug, Default)]
pub struct FakeIdentityAdministrationStore {
    applied: Mutex<Vec<(IdentityMutation, IdentityAuditEvent)>>,
}

impl FakeIdentityAdministrationStore {
    /// Returns captured atomic operations.
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    #[must_use]
    pub fn applied(&self) -> Vec<(IdentityMutation, IdentityAuditEvent)> {
        self.applied
            .lock()
            .unwrap_or_else(|error| panic!("fake administration lock poisoned: {error}"))
            .clone()
    }
}

#[async_trait::async_trait]
impl IdentityAdministrationStore for FakeIdentityAdministrationStore {
    async fn apply(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
    ) -> Result<(), IdentityPortError> {
        self.applied
            .lock()
            .unwrap_or_else(|error| panic!("fake administration lock poisoned: {error}"))
            .push((mutation, audit));
        Ok(())
    }

    async fn apply_with_idempotency(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        _idempotency_key: String,
        _idempotency_owner: String,
        _completion: IdempotencyCompletion,
    ) -> Result<(), IdentityPortError> {
        // Simple fake: treat as atomic but always succeed (no idempotency state).
        // If a test needs to simulate failure, use FakeIdentityStore with its
        // idempotency map and fail flag.
        self.applied
            .lock()
            .unwrap_or_else(|error| panic!("fake administration lock poisoned: {error}"))
            .push((mutation, audit));
        Ok(())
    }
}

/// Combined in-memory store that implements both [`IdentityRepository`] and
/// [`IdentityAdministrationStore`] with shared state, so that users created
/// via `IdentityAdministrationService` are immediately visible to the login
/// path (`FakeIdentityRepository` + `FakeIdentityAdministrationStore` would
/// A subject issued through `commit_subject_success`, recorded so tests can
/// assert what the server decided rather than what a request asked for.
#[derive(Debug, Clone)]
pub struct IssuedSubject {
    /// The stored user row.
    pub user: User,
    /// How the subject was created.
    pub kind: orbisync_domain::UserKind,
    /// Roles the server granted.
    pub roles: Vec<RoleId>,
    /// Absolute deadline for a temporary subject.
    pub expires_at: Option<Timestamp>,
    /// World boundary snapshotted at issue time.
    pub allowed_worlds: Vec<Uuid>,
    /// Issuer and subject for an external identity.
    pub issuer_subject: Option<(String, String)>,
}

/// otherwise be separate hash maps).
pub struct FakeIdentityStore {
    accounts: Mutex<HashMap<String, LoginAccount>>,
    sessions: Mutex<HashMap<AuthSessionId, AuthSession>>,
    roles: Mutex<HashMap<UserId, Vec<Role>>>,
    role_defs: Mutex<HashMap<RoleId, Role>>,
    applied: Mutex<Vec<(IdentityMutation, IdentityAuditEvent)>>,
    audits: Mutex<Vec<AuditView>>,
    /// Subjects issued through `commit_subject_success`, keyed by user id.
    subjects: Mutex<HashMap<UserId, IssuedSubject>>,
    codec: CursorCodec,
    // Idempotency state for atomic tests (AUD-C2 / P2-C2): single map shared with
    // the IdempotencyStore impl so that apply_with_idempotency can be atomic.
    // Stores (command, completion, owner, lease_until).
    #[allow(clippy::type_complexity)]
    idempotency_results: Mutex<
        HashMap<
            String,
            (
                IdempotencyClaimCommand,
                Option<IdempotencyCompletion>,
                String,
                Timestamp,
            ),
        >,
    >,
    fail_next_atomic_completion: Mutex<bool>,
    // Login transaction state (P2-C4).
    login_audits: Mutex<Vec<LoginAuditEvent>>,
    refresh_tokens_fake: Mutex<HashMap<String, LoginRefreshRecord>>,
    fail_next_login_commit: Mutex<bool>,
    fail_next_login_audit: Mutex<bool>,
    fail_next_login_session: Mutex<bool>,
    fail_next_login_refresh: Mutex<bool>,
    fail_next_login_failure_increment: Mutex<bool>,
}

impl core::fmt::Debug for FakeIdentityStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeIdentityStore")
            .field("accounts", &self.accounts)
            .field("sessions", &self.sessions)
            .field("roles", &self.roles)
            .field("role_defs", &self.role_defs)
            .field("applied", &self.applied)
            .field("audits", &self.audits)
            .finish()
    }
}

/// Returns a deterministic insecure codec for tests. Must not be used in production.
///
/// This centralizes the insecure fixed key in the testkit crate so the
/// production binary (`orbisync-application`, `orbisync-server`) contains
/// no hard-coded pagination key material. The key is only compiled into
/// test binaries.
#[allow(clippy::expect_used)]
#[must_use]
pub fn insecure_test_codec() -> CursorCodec {
    CursorCodec::new(b"insecure-fixed-pagination-key-for-tests-only-32b!!".to_vec())
        .expect("fixed test key is non-empty")
}

impl Default for FakeIdentityStore {
    fn default() -> Self {
        Self {
            accounts: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            roles: Mutex::new(HashMap::new()),
            role_defs: Mutex::new(HashMap::new()),
            applied: Mutex::new(Vec::new()),
            audits: Mutex::new(Vec::new()),
            subjects: Mutex::new(HashMap::new()),
            codec: insecure_test_codec(),
            idempotency_results: Mutex::new(HashMap::new()),
            fail_next_atomic_completion: Mutex::new(false),
            login_audits: Mutex::new(Vec::new()),
            refresh_tokens_fake: Mutex::new(HashMap::new()),
            fail_next_login_commit: Mutex::new(false),
            fail_next_login_audit: Mutex::new(false),
            fail_next_login_session: Mutex::new(false),
            fail_next_login_refresh: Mutex::new(false),
            fail_next_login_failure_increment: Mutex::new(false),
        }
    }
}

impl FakeIdentityStore {
    /// Creates an empty combined store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a role definition so subjects can be granted it.
    pub fn insert_role_definition(&self, role: Role) {
        self.role_defs
            .lock()
            .unwrap_or_else(|e| panic!("role defs poisoned: {e}"))
            .insert(role.id(), role);
    }

    /// Returns how many subjects were issued through `commit_subject_success`.
    #[must_use]
    pub fn issued_subject_count(&self) -> usize {
        self.subjects
            .lock()
            .unwrap_or_else(|e| panic!("subjects poisoned: {e}"))
            .len()
    }

    /// Returns how many distinct user ids were issued.
    ///
    /// Distinct from [`Self::issued_subject_count`] only if the store ever
    /// reused an id, which is exactly what a "fresh guest" test needs to rule
    /// out.
    #[must_use]
    pub fn distinct_issued_user_count(&self) -> usize {
        self.subjects
            .lock()
            .unwrap_or_else(|e| panic!("subjects poisoned: {e}"))
            .keys()
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// Returns every issued subject, for assertions about what the server
    /// decided.
    #[must_use]
    pub fn issued_subjects(&self) -> Vec<IssuedSubject> {
        self.subjects
            .lock()
            .unwrap_or_else(|e| panic!("subjects poisoned: {e}"))
            .values()
            .cloned()
            .collect()
    }

    /// Returns the roles granted to the most recently issued subject.
    #[must_use]
    pub fn last_issued_roles(&self) -> Option<Vec<RoleId>> {
        let subjects = self
            .subjects
            .lock()
            .unwrap_or_else(|e| panic!("subjects poisoned: {e}"));
        subjects
            .values()
            .max_by_key(|subject| subject.user.created_at())
            .map(|subject| subject.roles.clone())
    }

    /// Creates a store with an explicit codec (tests that need deterministic cursors).
    #[must_use]
    pub fn with_codec(codec: CursorCodec) -> Self {
        Self {
            codec,
            ..Self::default()
        }
    }

    /// Returns captured atomic operations.
    #[must_use]
    pub fn applied(&self) -> Vec<(IdentityMutation, IdentityAuditEvent)> {
        self.applied
            .lock()
            .unwrap_or_else(|error| panic!("fake administration lock poisoned: {error}"))
            .clone()
    }

    /// Directly inserts an account (for test setup bypassing the admin service).
    pub fn insert_account(&self, account: LoginAccount) {
        self.accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"))
            .insert(account.user.login_id().as_str().to_owned(), account);
    }

    /// Assigns roles for a user (server-owned view).
    pub fn set_roles(&self, user_id: UserId, roles: Vec<Role>) {
        self.roles
            .lock()
            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
            .insert(user_id, roles);
    }

    #[allow(missing_docs)]
    /// Forces the next atomic completion to fail (test helper).
    pub fn fail_next_atomic_completion(&self) {
        *self
            .fail_next_atomic_completion
            .lock()
            .unwrap_or_else(|error| panic!("fake atomic flag poisoned: {error}")) = true;
    }

    #[allow(missing_docs)]
    /// Returns idempotency map for inspection (test helper).
    pub fn idempotency_results(
        &self,
    ) -> HashMap<
        String,
        (
            IdempotencyClaimCommand,
            Option<IdempotencyCompletion>,
            String,
            Timestamp,
        ),
    > {
        self.idempotency_results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"))
            .clone()
    }

    #[allow(missing_docs)]
    /// Lease duration secs for tests.
    const IDEM_LEASE_SECS: u64 = 30;
    /// Returns login audits for inspection (P2-C4).
    #[allow(missing_docs)]
    pub fn login_audits(&self) -> Vec<LoginAuditEvent> {
        self.login_audits
            .lock()
            .unwrap_or_else(|e| panic!("login audit poisoned: {e}"))
            .clone()
    }

    /// Returns stored refresh tokens for login (P2-C4).
    #[allow(missing_docs)]
    pub fn login_refresh_tokens(&self) -> HashMap<String, LoginRefreshRecord> {
        self.refresh_tokens_fake
            .lock()
            .unwrap_or_else(|e| panic!("refresh poisoned: {e}"))
            .clone()
    }

    /// Forces next login commit to fail (atomicity test helper).
    #[allow(missing_docs)]
    pub fn fail_next_login_commit(&self) {
        *self
            .fail_next_login_commit
            .lock()
            .unwrap_or_else(|e| panic!("flag poisoned: {e}")) = true;
    }

    /// Forces next login audit write to fail (fail-closed helper).
    #[allow(missing_docs)]
    pub fn fail_next_login_audit(&self) {
        *self
            .fail_next_login_audit
            .lock()
            .unwrap_or_else(|e| panic!("flag poisoned: {e}")) = true;
    }

    /// Forces next login session insert to fail (partial commit helper).
    #[allow(missing_docs)]
    pub fn fail_next_login_session(&self) {
        *self
            .fail_next_login_session
            .lock()
            .unwrap_or_else(|e| panic!("flag poisoned: {e}")) = true;
    }

    /// Forces next login refresh insert to fail (partial commit helper).
    #[allow(missing_docs)]
    pub fn fail_next_login_refresh(&self) {
        *self
            .fail_next_login_refresh
            .lock()
            .unwrap_or_else(|e| panic!("flag poisoned: {e}")) = true;
    }

    /// Forces next failure increment to simulate DB error (P2-C4).
    #[allow(missing_docs)]
    pub fn fail_next_login_failure_increment(&self) {
        *self
            .fail_next_login_failure_increment
            .lock()
            .unwrap_or_else(|e| panic!("flag poisoned: {e}")) = true;
    }

    /// Returns sessions map for inspection (P2-C4).
    #[allow(missing_docs)]
    pub fn sessions_snapshot(&self) -> HashMap<AuthSessionId, AuthSession> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| panic!("session poisoned: {e}"))
            .clone()
    }
}

#[async_trait::async_trait]
impl IdentityRepository for FakeIdentityStore {
    async fn find_login(
        &self,
        login_id: &LoginId,
    ) -> Result<Option<LoginAccount>, IdentityPortError> {
        Ok(self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"))
            .get(login_id.as_str())
            .cloned())
    }

    async fn find_account(
        &self,
        user_id: UserId,
    ) -> Result<Option<LoginAccount>, IdentityPortError> {
        Ok(self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"))
            .values()
            .find(|a| a.user.id() == user_id)
            .cloned())
    }

    async fn save_credential(&self, credential: &Credential) -> Result<(), IdentityPortError> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        if let Some(account) = accounts
            .values_mut()
            .find(|account| account.user.id() == credential.user_id())
        {
            account.credential = credential.clone();
        }
        Ok(())
    }

    async fn record_login_failure(
        &self,
        user_id: UserId,
        now: Timestamp,
    ) -> Result<orbisync_application::LoginFailureOutcome, IdentityPortError> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        let account = accounts
            .values_mut()
            .find(|a| a.user.id() == user_id)
            .ok_or(IdentityPortError::NotFound)?;
        let current = account.credential.failed_login_count();
        let next = current.saturating_add(1);
        let new_locked = if next >= Credential::LOGIN_FAILURE_THRESHOLD {
            Some(
                now.checked_add_millis(Credential::LOGIN_LOCKOUT_MILLIS)
                    .map_err(|_| IdentityPortError::DataCorruption)?,
            )
        } else {
            account.credential.locked_until()
        };
        account.credential = Credential::reconstitute(
            user_id,
            account.credential.password_hash().clone(),
            account.credential.password_changed_at(),
            next,
            new_locked,
        );
        Ok(orbisync_application::LoginFailureOutcome {
            failed_login_count: next,
            locked_until: new_locked,
        })
    }

    async fn reset_login_success(
        &self,
        user_id: UserId,
        expected_failed_count: u32,
        expected_locked_until: Option<Timestamp>,
        _now: Timestamp,
        new_password_hash: Option<PasswordHash>,
    ) -> Result<bool, IdentityPortError> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        let account = accounts
            .values_mut()
            .find(|a| a.user.id() == user_id)
            .ok_or(IdentityPortError::NotFound)?;
        if account.credential.failed_login_count() != expected_failed_count
            || account.credential.locked_until() != expected_locked_until
        {
            return Ok(false);
        }
        let new_hash =
            new_password_hash.unwrap_or_else(|| account.credential.password_hash().clone());
        let new_changed =
            if account.credential.password_hash().expose_phc() != new_hash.expose_phc() {
                _now
            } else {
                account.credential.password_changed_at()
            };
        account.credential = Credential::reconstitute(user_id, new_hash, new_changed, 0, None);
        Ok(true)
    }

    async fn roles_for_user(&self, user_id: UserId) -> Result<Vec<Role>, IdentityPortError> {
        Ok(self
            .roles
            .lock()
            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
            .get(&user_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn find_session(
        &self,
        session_id: AuthSessionId,
    ) -> Result<Option<AuthSession>, IdentityPortError> {
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"))
            .get(&session_id)
            .cloned())
    }

    async fn save_session(&self, session: &AuthSession) -> Result<(), IdentityPortError> {
        self.sessions
            .lock()
            .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"))
            .insert(session.id(), session.clone());
        Ok(())
    }
}

#[async_trait::async_trait]
impl IdentityAdministrationStore for FakeIdentityStore {
    async fn apply(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
    ) -> Result<(), IdentityPortError> {
        {
            let mut accounts = self
                .accounts
                .lock()
                .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
            match &mutation {
                IdentityMutation::BootstrapAdministrator {
                    user,
                    credential,
                    roles,
                } => {
                    if !accounts.is_empty() {
                        return Err(IdentityPortError::Unavailable);
                    }
                    let login = user.login_id().as_str().to_owned();
                    if accounts.contains_key(&login) {
                        return Err(IdentityPortError::Unavailable);
                    }
                    let account = LoginAccount {
                        user: user.clone(),
                        credential: credential.clone(),
                    };
                    accounts.insert(login, account);
                    drop(accounts);
                    {
                        let mut role_defs = self
                            .role_defs
                            .lock()
                            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
                        for role in roles {
                            role_defs.insert(role.id(), role.clone());
                        }
                    }
                    self.roles
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
                        .insert(user.id(), roles.clone());
                }
                IdentityMutation::CreateUser { user, credential } => {
                    let login = user.login_id().as_str().to_owned();
                    if accounts.contains_key(&login) {
                        return Err(IdentityPortError::Unavailable);
                    }
                    let account = LoginAccount {
                        user: user.clone(),
                        credential: credential.clone(),
                    };
                    accounts.insert(login, account);
                }
                IdentityMutation::ReplaceUserRoles { user_id, role_ids } => {
                    drop(accounts);
                    let defs = self
                        .role_defs
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
                    let roles: Vec<Role> = role_ids
                        .iter()
                        .filter_map(|id| defs.get(id).cloned())
                        .collect();
                    self.roles
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
                        .insert(*user_id, roles);
                }
                IdentityMutation::StoreRole { role } => {
                    drop(accounts);
                    self.role_defs
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"))
                        .insert(role.id(), role.clone());
                }
                IdentityMutation::UpdateRole { role } => {
                    drop(accounts);
                    let mut defs = self
                        .role_defs
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
                    let Some(current) = defs.get(&role.id()) else {
                        return Err(IdentityPortError::NotFound);
                    };
                    let expected = role.revision().as_u64().saturating_sub(1);
                    if current.revision().as_u64() != expected {
                        return Err(IdentityPortError::Conflict);
                    }
                    defs.insert(role.id(), role.clone());
                }
                IdentityMutation::StoreCredential { credential } => {
                    if let Some(account) = accounts
                        .values_mut()
                        .find(|account| account.user.id() == credential.user_id())
                    {
                        account.credential = credential.clone();
                    }
                }
                IdentityMutation::PasswordChangeRejected { .. } => {
                    // audit-only, no state change
                }
                IdentityMutation::StorePasswordChange { user, credential } => {
                    if let Some(account) = accounts
                        .values_mut()
                        .find(|account| account.user.id() == user.id())
                    {
                        account.user = user.clone();
                        account.credential = credential.clone();
                    }
                    // Emulate PG revocation: revoke all active sessions for this user.
                    let updated_at = user.updated_at();
                    let target = user.id();
                    drop(accounts);
                    let mut sessions = self
                        .sessions
                        .lock()
                        .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"));
                    for session in sessions.values_mut() {
                        if session.user_id() == target
                            && session.status() == orbisync_domain::AuthSessionStatus::Active
                        {
                            session.revoke(updated_at);
                        }
                    }
                }
                IdentityMutation::ResetPassword { user, credential } => {
                    if let Some(account) = accounts
                        .values_mut()
                        .find(|account| account.user.id() == user.id())
                    {
                        account.user = user.clone();
                        account.credential = credential.clone();
                    }
                    // PW-1 §2: 既存セッションの失効 – 管理者リセットで全セッションを失効させる。
                    let updated_at = user.updated_at();
                    let target = user.id();
                    drop(accounts);
                    let mut sessions = self
                        .sessions
                        .lock()
                        .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"));
                    for session in sessions.values_mut() {
                        if session.user_id() == target
                            && session.status() == orbisync_domain::AuthSessionStatus::Active
                        {
                            session.revoke(updated_at);
                        }
                    }
                }
                IdentityMutation::StoreUserStatus { user } => {
                    if let Some(account) = accounts
                        .values_mut()
                        .find(|account| account.user.id() == user.id())
                    {
                        account.user = user.clone();
                    }
                }
                IdentityMutation::UpdateUserProfile {
                    user,
                    expected_revision,
                } => {
                    let Some(account) = accounts
                        .values_mut()
                        .find(|account| account.user.id() == user.id())
                    else {
                        return Err(IdentityPortError::NotFound);
                    };
                    if account.user.revision().as_u64() != *expected_revision {
                        return Err(IdentityPortError::Conflict);
                    }
                    account.user = user.clone();
                    if user.status() == UserStatus::Disabled {
                        let updated_at = user.updated_at();
                        let target = user.id();
                        drop(accounts);
                        let mut sessions = self
                            .sessions
                            .lock()
                            .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"));
                        for session in sessions.values_mut() {
                            if session.user_id() == target
                                && session.status() == orbisync_domain::AuthSessionStatus::Active
                            {
                                session.revoke(updated_at);
                            }
                        }
                    }
                }
                IdentityMutation::StoreSession { session } => {
                    drop(accounts);
                    self.sessions
                        .lock()
                        .unwrap_or_else(|error| panic!("fake session lock poisoned: {error}"))
                        .insert(session.id(), session.clone());
                }
                IdentityMutation::DeleteRole {
                    role_id,
                    expected_revision,
                } => {
                    drop(accounts);
                    let mut defs = self
                        .role_defs
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
                    let Some(role) = defs.get(role_id) else {
                        return Err(IdentityPortError::NotFound);
                    };
                    if role.revision().as_u64() != *expected_revision {
                        return Err(IdentityPortError::Conflict);
                    }
                    defs.remove(role_id);
                    drop(defs);
                    // Cascade: remove assignments
                    let mut roles = self
                        .roles
                        .lock()
                        .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
                    for (_user_id, assigned) in roles.iter_mut() {
                        assigned.retain(|r| r.id() != *role_id);
                    }
                    // Roles empty vectors remain; assignment is effectively removed.
                }
                _ => {
                    // Other mutations are captured but do not affect login state for W-15.
                }
            }
        }
        // Also store audit view for search (CR-04)
        let view = {
            let actor_type = if audit.actor_id.is_some() {
                "user".to_owned()
            } else {
                "system".to_owned()
            };
            let resource_type = audit
                .action
                .split_once('.')
                .map(|(r, _)| r.to_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            AuditView {
                id: Uuid::now_v7().to_string(),
                occurred_at: audit.occurred_at,
                actor_type,
                actor_id: audit.actor_id,
                action: audit.action.to_owned(),
                resource_type,
                resource_id: audit.resource_id.clone(),
                result: if audit.succeeded {
                    "success".to_owned()
                } else {
                    "failure".to_owned()
                },
                error_code: None,
                request_id: audit.request_id.to_string(),
                details: serde_json::json!({}),
            }
        };
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .push(view);
        self.applied
            .lock()
            .unwrap_or_else(|error| panic!("fake administration lock poisoned: {error}"))
            .push((mutation, audit));
        Ok(())
    }

    async fn apply_with_idempotency(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        idempotency_key: String,
        idempotency_owner: String,
        completion: IdempotencyCompletion,
    ) -> Result<(), IdentityPortError> {
        // Check fail flag first – simulate DB failure on idempotency complete.
        let should_fail = {
            let mut flag = self
                .fail_next_atomic_completion
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(IdentityPortError::Unavailable);
        }
        // Atomically apply mutation + idempotency. We first handle idempotency
        // completion: it must affect exactly one in_progress record with matching owner.
        {
            let mut map = self
                .idempotency_results
                .lock()
                .unwrap_or_else(|e| panic!("idempotency poisoned: {e}"));
            let entry = map.get_mut(&idempotency_key);
            let Some((_cmd, comp_opt, owner, _lease)) = entry else {
                return Err(IdentityPortError::Unavailable);
            };
            if *owner != idempotency_owner {
                return Err(IdentityPortError::Unavailable);
            }
            if comp_opt.is_some() {
                return Err(IdentityPortError::Unavailable);
            }
            *comp_opt = Some(completion);
        }
        // Now apply the mutation using same logic as apply(), but if it fails,
        // rollback the idempotency completion.
        let result = self.apply(mutation.clone(), audit.clone()).await;
        if result.is_err() {
            let mut map = self
                .idempotency_results
                .lock()
                .unwrap_or_else(|e| panic!("idempotency poisoned: {e}"));
            if let Some((_, comp_opt, _, _)) = map.get_mut(&idempotency_key) {
                *comp_opt = None;
            }
        }
        result
    }
}

#[async_trait::async_trait]
impl IdempotencyStore for FakeIdentityStore {
    async fn claim(
        &self,
        command: IdempotencyClaimCommand,
    ) -> Result<IdempotencyClaim, IdentityPortError> {
        let mut records = self
            .idempotency_results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"));
        if let Some((existing, completion, owner, lease_until)) = records.get(&command.key) {
            if existing.actor_user_id != command.actor_user_id
                || existing.operation != command.operation
                || existing.request_hash != command.request_hash
            {
                return Ok(IdempotencyClaim::Reused);
            }
            if let Some(comp) = completion.clone() {
                return Ok(IdempotencyClaim::Completed(comp));
            }
            // In-progress
            if *lease_until <= command.now {
                // Lease expired – steal
                let new_owner = Uuid::now_v7().to_string();
                let new_lease = command
                    .now
                    .checked_add_millis((Self::IDEM_LEASE_SECS * 1000) as i64)
                    .unwrap_or(command.now);
                // Update entry
                if let Some(entry) = records.get_mut(&command.key) {
                    entry.2 = new_owner.clone();
                    entry.3 = new_lease;
                }
                return Ok(IdempotencyClaim::Acquired {
                    owner: new_owner,
                    lease_until: new_lease,
                });
            }
            let diff_ms = lease_until.to_unix_millis().unwrap_or(0)
                - command.now.to_unix_millis().unwrap_or(0);
            let secs = if diff_ms <= 0 {
                1
            } else {
                u64::try_from(diff_ms / 1000).unwrap_or(1)
            };
            let secs = if secs == 0 { 1 } else { secs };
            let _ = owner;
            return Ok(IdempotencyClaim::InProgress {
                retry_after_secs: secs,
            });
        }
        let owner = Uuid::now_v7().to_string();
        let lease_until = command
            .now
            .checked_add_millis((Self::IDEM_LEASE_SECS * 1000) as i64)
            .unwrap_or(command.now);
        records.insert(
            command.key.clone(),
            (command, None, owner.clone(), lease_until),
        );
        Ok(IdempotencyClaim::Acquired { owner, lease_until })
    }

    async fn complete(
        &self,
        key: String,
        owner: String,
        completion: IdempotencyCompletion,
    ) -> Result<(), IdentityPortError> {
        let mut records = self
            .idempotency_results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"));
        let entry = records
            .get_mut(&key)
            .ok_or(IdentityPortError::Unavailable)?;
        if entry.2 != owner {
            return Err(IdentityPortError::Unavailable);
        }
        if entry.1.is_some() {
            return Err(IdentityPortError::Unavailable);
        }
        entry.1 = Some(completion);
        Ok(())
    }

    async fn abandon(&self, key: String, owner: String) -> Result<(), IdentityPortError> {
        let mut records = self
            .idempotency_results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"));
        let entry = records.get(&key).ok_or(IdentityPortError::Unavailable)?;
        if entry.2 != owner {
            return Err(IdentityPortError::Unavailable);
        }
        if entry.1.is_some() {
            return Err(IdentityPortError::Unavailable);
        }
        records.remove(&key);
        Ok(())
    }
}

#[async_trait::async_trait]
impl LoginTransactionStore for FakeIdentityStore {
    async fn commit_success(&self, commit: LoginCommit<'_>) -> Result<(), IdentityPortError> {
        // Fail-closed checks before any mutation (atomicity).
        let should_fail = {
            let mut flag = self
                .fail_next_login_commit
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(IdentityPortError::Unavailable);
        }
        let fail_audit = {
            let mut flag = self
                .fail_next_login_audit
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if fail_audit {
            return Err(IdentityPortError::Unavailable);
        }
        let fail_session = {
            let mut flag = self
                .fail_next_login_session
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if fail_session {
            return Err(IdentityPortError::Unavailable);
        }
        let fail_refresh = {
            let mut flag = self
                .fail_next_login_refresh
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if fail_refresh {
            return Err(IdentityPortError::Unavailable);
        }

        // Validate audit does not contain raw login id (enumeration resistance).
        // The fake audit type already lacks login_id, so we just check that
        // action is correct and no secret-like fields.
        // Secret redaction is checked by separate tests that inspect logs.

        // Apply credential reset with optimistic predicate.
        {
            let mut accounts = self
                .accounts
                .lock()
                .unwrap_or_else(|e| panic!("fake poisoned: {e}"));
            let account = accounts
                .values_mut()
                .find(|a| a.user.id() == commit.user_id)
                .ok_or(IdentityPortError::Unavailable)?;
            if account.credential.failed_login_count() != commit.expected_failed_count
                || account.credential.locked_until() != commit.expected_locked_until
            {
                // Predicate miss – keep concurrent failure, still proceed to
                // session/refresh/audit but do not reset. We emulate by not
                // resetting credential.
            } else {
                let new_hash = commit
                    .new_password_hash
                    .cloned()
                    .unwrap_or_else(|| account.credential.password_hash().clone());
                let new_changed = if commit.new_password_hash.is_some() {
                    commit.audit.occurred_at
                } else {
                    account.credential.password_changed_at()
                };
                account.credential =
                    Credential::reconstitute(commit.user_id, new_hash, new_changed, 0, None);
            }
        }

        // Insert session
        {
            let session = AuthSession::new(
                commit.session.id,
                commit.session.user_id,
                commit.session.created_at,
                commit.session.expires_at,
            )
            .map_err(|_| IdentityPortError::DataCorruption)?;
            self.sessions
                .lock()
                .unwrap_or_else(|e| panic!("session poisoned: {e}"))
                .insert(session.id(), session);
        }
        // Insert refresh token
        {
            self.refresh_tokens_fake
                .lock()
                .unwrap_or_else(|e| panic!("refresh poisoned: {e}"))
                .insert(commit.refresh.token_id.clone(), commit.refresh.clone());
        }
        // Insert audit (enumeration-resistant: no raw login_id, actor only on success)
        {
            // Verify no secret leak: audit action must be auth.login and must not contain password/token bytes.
            // This is implicitly true because LoginAuditEvent lacks those fields.
            self.login_audits
                .lock()
                .unwrap_or_else(|e| panic!("audit poisoned: {e}"))
                .push(commit.audit.clone());
            // Also push to generic audit view for query tests.
            let view = AuditView {
                id: Uuid::now_v7().to_string(),
                occurred_at: commit.audit.occurred_at,
                actor_type: if commit.audit.actor_id.is_some() {
                    "user".to_owned()
                } else {
                    "system".to_owned()
                },
                actor_id: commit.audit.actor_id,
                action: commit.audit.action.to_owned(),
                resource_type: "auth".to_owned(),
                resource_id: None,
                result: if commit.audit.succeeded {
                    "success".to_owned()
                } else {
                    "failure".to_owned()
                },
                error_code: None,
                request_id: commit.audit.request_id.to_string(),
                details: serde_json::json!({}),
            };
            self.audits
                .lock()
                .unwrap_or_else(|e| panic!("audit poisoned: {e}"))
                .push(view);
        }
        Ok(())
    }

    async fn commit_subject_success(
        &self,
        commit: orbisync_application::SubjectCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        // Honour the same injected-failure flag the credential path uses, so a
        // test can prove this commit is atomic too.
        let should_fail = {
            let mut flag = self
                .fail_next_login_commit
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(IdentityPortError::Unavailable);
        }

        if let Some(new_user) = commit.new_user {
            let accounts = self
                .accounts
                .lock()
                .unwrap_or_else(|e| panic!("fake poisoned: {e}"));
            if accounts.contains_key(new_user.login_id.as_str()) {
                // Mirrors the users.login_id unique constraint.
                return Err(IdentityPortError::Conflict);
            }
            let user = User::reconstitute(
                commit.user_id,
                new_user.login_id.clone(),
                new_user.display_name.clone(),
                UserStatus::Active,
                false,
                commit.grant_roles.iter().copied().collect(),
                new_user.created_at,
                new_user.created_at,
                orbisync_domain::Revision::from_u64(1),
            );
            // Deliberately no credential entry: these subjects hold no password,
            // so `find_login` must not be able to resolve them.
            self.subjects
                .lock()
                .unwrap_or_else(|e| panic!("subjects poisoned: {e}"))
                .insert(
                    commit.user_id,
                    IssuedSubject {
                        user: user.clone(),
                        kind: new_user.kind,
                        roles: commit.grant_roles.to_vec(),
                        expires_at: commit.ephemeral.as_ref().map(|value| value.expires_at),
                        allowed_worlds: commit
                            .ephemeral
                            .as_ref()
                            .map(|value| value.allowed_worlds.clone())
                            .unwrap_or_default(),
                        issuer_subject: commit
                            .external
                            .as_ref()
                            .map(|value| (value.issuer.clone(), value.subject.clone())),
                    },
                );
            // Deliberately NOT inserted into `accounts`, which is what
            // `find_login` and `find_account` read. In Postgres those queries
            // INNER JOIN `user_credentials`, so a subject with no credential
            // row is invisible to them; recording it here would make the fake
            // resolve a subject the real store cannot.
            drop(accounts);

            let mut roles = self
                .roles
                .lock()
                .unwrap_or_else(|e| panic!("roles poisoned: {e}"));
            let defs = self
                .role_defs
                .lock()
                .unwrap_or_else(|e| panic!("role defs poisoned: {e}"));
            let granted: Vec<Role> = commit
                .grant_roles
                .iter()
                .filter_map(|id| defs.get(id).cloned())
                .collect();
            roles.insert(commit.user_id, granted);
        }

        let session = AuthSession::new(
            commit.session.id,
            commit.session.user_id,
            commit.session.created_at,
            commit.session.expires_at,
        )
        .map_err(|_| IdentityPortError::DataCorruption)?;
        self.sessions
            .lock()
            .unwrap_or_else(|e| panic!("session poisoned: {e}"))
            .insert(session.id(), session);

        self.refresh_tokens_fake
            .lock()
            .unwrap_or_else(|e| panic!("refresh poisoned: {e}"))
            .insert(commit.refresh.token_id.clone(), commit.refresh.clone());

        self.login_audits
            .lock()
            .unwrap_or_else(|e| panic!("audit poisoned: {e}"))
            .push(commit.audit.clone());
        let view = AuditView {
            id: Uuid::now_v7().to_string(),
            occurred_at: commit.audit.occurred_at,
            actor_type: "user".to_owned(),
            actor_id: commit.audit.actor_id,
            action: commit.audit.action.to_owned(),
            resource_type: "auth".to_owned(),
            resource_id: None,
            result: "success".to_owned(),
            error_code: None,
            request_id: commit.audit.request_id.to_string(),
            details: serde_json::json!({}),
        };
        self.audits
            .lock()
            .unwrap_or_else(|e| panic!("audit poisoned: {e}"))
            .push(view);
        Ok(())
    }

    async fn record_failure(
        &self,
        user_id: Option<UserId>,
        audit: LoginAuditEvent,
    ) -> Result<(), IdentityPortError> {
        let fail_audit = {
            let mut flag = self
                .fail_next_login_audit
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if fail_audit {
            return Err(IdentityPortError::Unavailable);
        }
        let fail_inc = {
            let mut flag = self
                .fail_next_login_failure_increment
                .lock()
                .unwrap_or_else(|e| panic!("flag poisoned: {e}"));
            if *flag {
                *flag = false;
                true
            } else {
                false
            }
        };
        if fail_inc {
            return Err(IdentityPortError::Unavailable);
        }

        // Increment failure count if account exists.
        if let Some(uid) = user_id {
            let now = audit.occurred_at;
            let mut accounts = self
                .accounts
                .lock()
                .unwrap_or_else(|e| panic!("fake poisoned: {e}"));
            let account = accounts
                .values_mut()
                .find(|a| a.user.id() == uid)
                .ok_or(IdentityPortError::Unavailable)?;
            let current = account.credential.failed_login_count();
            let next = current.saturating_add(1);
            let new_locked = if next >= Credential::LOGIN_FAILURE_THRESHOLD {
                Some(
                    now.checked_add_millis(Credential::LOGIN_LOCKOUT_MILLIS)
                        .map_err(|_| IdentityPortError::DataCorruption)?,
                )
            } else {
                account.credential.locked_until()
            };
            account.credential = Credential::reconstitute(
                uid,
                account.credential.password_hash().clone(),
                account.credential.password_changed_at(),
                next,
                new_locked,
            );
        }
        // Record audit – must be enumeration-resistant (no raw login_id).
        {
            self.login_audits
                .lock()
                .unwrap_or_else(|e| panic!("audit poisoned: {e}"))
                .push(audit.clone());
            let view = AuditView {
                id: Uuid::now_v7().to_string(),
                occurred_at: audit.occurred_at,
                actor_type: if audit.actor_id.is_some() {
                    "user".to_owned()
                } else {
                    "system".to_owned()
                },
                actor_id: audit.actor_id,
                action: audit.action.to_owned(),
                resource_type: "auth".to_owned(),
                resource_id: None,
                result: if audit.succeeded {
                    "success".to_owned()
                } else {
                    "failure".to_owned()
                },
                error_code: None,
                request_id: audit.request_id.to_string(),
                details: serde_json::json!({}),
            };
            self.audits
                .lock()
                .unwrap_or_else(|e| panic!("audit poisoned: {e}"))
                .push(view);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl IdentityQueryPort for FakeIdentityStore {
    async fn user(&self, id: UserId) -> Result<Option<UserView>, IdentityPortError> {
        let accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        for account in accounts.values() {
            if account.user.id() == id {
                return Ok(Some(UserView {
                    id: account.user.id(),
                    login_id: account.user.login_id().as_str().to_owned(),
                    display_name: account.user.display_name().to_owned(),
                    status: account.user.status(),
                    revision: account.user.revision().as_u64(),
                }));
            }
        }
        Ok(None)
    }

    async fn users(&self, page: PageRequest) -> Result<Page<UserView>, IdentityPortError> {
        // CR-07: handle opaque cursor (HMAC) and clamp 1..200
        let after_id = if let Some(after) = &page.after {
            if after.0.contains('.') {
                let id_str = self
                    .codec
                    .decode_users(&after.0)
                    .map_err(|_| IdentityPortError::InvalidRequest)?;
                Some(id_str)
            } else {
                Uuid::parse_str(&after.0).map_err(|_| IdentityPortError::InvalidRequest)?;
                Some(after.0.clone())
            }
        } else {
            None
        };
        let limit = usize::try_from(page.limit.clamp(1, 200)).unwrap_or(0);
        let accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|error| panic!("fake identity lock poisoned: {error}"));
        let mut views: Vec<UserView> = accounts
            .values()
            .map(|account| UserView {
                id: account.user.id(),
                login_id: account.user.login_id().as_str().to_owned(),
                display_name: account.user.display_name().to_owned(),
                status: account.user.status(),
                revision: account.user.revision().as_u64(),
            })
            .collect();
        views.sort_by_key(|view| view.id.to_string());
        let start = if let Some(id_str) = after_id {
            views
                .iter()
                .position(|v| v.id.to_string() == id_str)
                .ok_or(IdentityPortError::InvalidRequest)?
                + 1
        } else {
            0
        };
        let end = (start + limit).min(views.len());
        let items = views[start..end].to_vec();
        let next = if end < views.len() {
            let last_id = items.last().map(|l| l.id.to_string()).unwrap_or_default();
            let cursor = self
                .codec
                .encode_users(&last_id)
                .map_err(|_| IdentityPortError::DataCorruption)?;
            Some(QueryCursor(cursor))
        } else {
            None
        };
        Ok(Page { items, next })
    }

    async fn role(&self, id: RoleId) -> Result<Option<RoleView>, IdentityPortError> {
        let defs = self
            .role_defs
            .lock()
            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
        Ok(defs.get(&id).map(|role| RoleView {
            id: role.id(),
            name: role.name().to_owned(),
            description: role.description().map(str::to_owned),
            permissions: role
                .permissions()
                .iter()
                .map(|permission| permission.as_str().to_owned())
                .collect(),
            revision: role.revision().as_u64(),
        }))
    }

    async fn roles(&self, page: PageRequest) -> Result<Page<RoleView>, IdentityPortError> {
        let after_id = if let Some(after) = &page.after {
            if after.0.contains('.') {
                let id_str = self
                    .codec
                    .decode_roles(&after.0)
                    .map_err(|_| IdentityPortError::InvalidRequest)?;
                Some(id_str)
            } else {
                Uuid::parse_str(&after.0).map_err(|_| IdentityPortError::InvalidRequest)?;
                Some(after.0.clone())
            }
        } else {
            None
        };
        let limit = usize::try_from(page.limit.clamp(1, 200)).unwrap_or(0);
        let defs = self
            .role_defs
            .lock()
            .unwrap_or_else(|error| panic!("fake role lock poisoned: {error}"));
        let mut views: Vec<RoleView> = defs
            .values()
            .map(|role| RoleView {
                id: role.id(),
                name: role.name().to_owned(),
                description: role.description().map(str::to_owned),
                permissions: role
                    .permissions()
                    .iter()
                    .map(|permission| permission.as_str().to_owned())
                    .collect(),
                revision: role.revision().as_u64(),
            })
            .collect();
        views.sort_by_key(|view| view.id.to_string());
        let start = if let Some(id_str) = after_id {
            views
                .iter()
                .position(|v| v.id.to_string() == id_str)
                .ok_or(IdentityPortError::InvalidRequest)?
                + 1
        } else {
            0
        };
        let end = (start + limit).min(views.len());
        let items = views[start..end].to_vec();
        let next = if end < views.len() {
            let last_id = items.last().map(|l| l.id.to_string()).unwrap_or_default();
            let cursor = self
                .codec
                .encode_roles(&last_id)
                .map_err(|_| IdentityPortError::DataCorruption)?;
            Some(QueryCursor(cursor))
        } else {
            None
        };
        Ok(Page { items, next })
    }
}

#[async_trait::async_trait]
impl AuditQueryPort for FakeIdentityStore {
    async fn search(
        &self,
        filter: AuditFilter,
        page: PageRequest,
    ) -> Result<Page<AuditView>, IdentityPortError> {
        // CR-07 / V-04: apply filter, clamp limit 1..200, handle opaque cursor with filter binding.
        let limit = usize::try_from(page.limit.clamp(1, 200)).unwrap_or(0);
        let audits = self
            .audits
            .lock()
            .unwrap_or_else(|e| panic!("fake audit lock poisoned: {e}"))
            .clone();
        // Filter
        let mut filtered: Vec<AuditView> = audits
            .into_iter()
            .filter(|v| {
                if let Some(from) = filter.from
                    && v.occurred_at < from
                {
                    return false;
                }
                if let Some(to) = filter.to
                    && v.occurred_at > to
                {
                    return false;
                }
                if let Some(actor) = filter.actor_id
                    && v.actor_id != Some(actor)
                {
                    return false;
                }
                if let Some(action) = &filter.action
                    && &v.action != action
                {
                    return false;
                }
                true
            })
            .collect();
        // Sort descending occurred_at, id
        filtered.sort_by(|a, b| {
            b.occurred_at
                .cmp(&a.occurred_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        // Handle cursor
        let start = if let Some(after) = page.after {
            if after.0.contains('.') {
                // Opaque cursor: decode and verify filter hash
                let from_millis = filter.from.map(|t| {
                    t.as_offset_date_time().unix_timestamp() * 1000
                        + i64::from(t.as_offset_date_time().millisecond())
                });
                let to_millis = filter.to.map(|t| {
                    t.as_offset_date_time().unix_timestamp() * 1000
                        + i64::from(t.as_offset_date_time().millisecond())
                });
                let actor = filter.actor_id.map(|id| id.to_string());
                let fh = self.codec.audit_filter_hash(
                    from_millis,
                    to_millis,
                    actor.as_deref(),
                    filter.action.as_deref(),
                );
                let (ts_millis, id_str) = self
                    .codec
                    .decode_audit(&after.0, &fh)
                    .map_err(|_| IdentityPortError::InvalidRequest)?;
                let ts = Timestamp::from_unix_millis(ts_millis)
                    .map_err(|_| IdentityPortError::InvalidRequest)?;
                let pos = filtered
                    .iter()
                    .position(|v| v.occurred_at == ts && v.id == id_str);
                match pos {
                    Some(idx) => idx + 1,
                    None => return Err(IdentityPortError::InvalidRequest),
                }
            } else {
                // Legacy plain UUID (for users/roles tests) – treat as InvalidRequest for audit to force opaque
                return Err(IdentityPortError::InvalidRequest);
            }
        } else {
            0
        };
        let end = (start + limit).min(filtered.len());
        let items = filtered[start..end].to_vec();
        let next = if end < filtered.len() {
            match items.last() {
                Some(last) => {
                    let ts_millis = last.occurred_at.as_offset_date_time().unix_timestamp() * 1000
                        + i64::from(last.occurred_at.as_offset_date_time().millisecond());
                    let from_millis = filter.from.map(|t| {
                        t.as_offset_date_time().unix_timestamp() * 1000
                            + i64::from(t.as_offset_date_time().millisecond())
                    });
                    let to_millis = filter.to.map(|t| {
                        t.as_offset_date_time().unix_timestamp() * 1000
                            + i64::from(t.as_offset_date_time().millisecond())
                    });
                    let actor = filter.actor_id.map(|id| id.to_string());
                    let fh = self.codec.audit_filter_hash(
                        from_millis,
                        to_millis,
                        actor.as_deref(),
                        filter.action.as_deref(),
                    );
                    let cursor = self
                        .codec
                        .encode_audit(ts_millis, &last.id, &fh)
                        .map_err(|_| IdentityPortError::DataCorruption)?;
                    Some(QueryCursor(cursor))
                }
                None => None,
            }
        } else {
            None
        };
        Ok(Page { items, next })
    }

    async fn event(
        &self,
        id: orbisync_application::query::AuditEventId,
    ) -> Result<Option<AuditView>, IdentityPortError> {
        let audits = self
            .audits
            .lock()
            .unwrap_or_else(|e| panic!("fake audit lock poisoned: {e}"))
            .clone();
        Ok(audits.into_iter().find(|v| v.id == id.to_string()))
    }
}

/// In-memory realtime ticket store for HTTP ticket issuance tests (C3).
#[derive(Debug, Default)]
pub struct FakeRealtimeTicketStore {
    tickets: Mutex<HashMap<[u8; 32], CreateRealtimeTicketCommand>>,
}

#[async_trait::async_trait]
impl orbisync_application::RealtimeTicketStore for FakeRealtimeTicketStore {
    async fn create(&self, command: CreateRealtimeTicketCommand) -> Result<(), IdentityPortError> {
        self.tickets
            .lock()
            .unwrap_or_else(|e| panic!("fake realtime ticket lock poisoned: {e}"))
            .insert(command.token_digest, command);
        Ok(())
    }

    async fn consume(
        &self,
        token_digest: [u8; 32],
        now: Timestamp,
    ) -> Result<orbisync_application::RealtimeTicketConsumption, IdentityPortError> {
        let mut guard = self
            .tickets
            .lock()
            .unwrap_or_else(|e| panic!("fake realtime ticket lock poisoned: {e}"));
        let Some(cmd) = guard.get(&token_digest).cloned() else {
            return Ok(orbisync_application::RealtimeTicketConsumption::Rejected);
        };
        if cmd.expires_at < now {
            guard.remove(&token_digest);
            return Ok(orbisync_application::RealtimeTicketConsumption::Rejected);
        }
        guard.remove(&token_digest);
        Ok(orbisync_application::RealtimeTicketConsumption::Consumed {
            user_id: cmd.user_id,
            session_id: cmd.session_id,
        })
    }
}

/// In-memory idempotency result store.
#[derive(Debug, Default)]
pub struct FakeIdempotencyStore {
    #[allow(clippy::type_complexity)]
    results: Mutex<
        HashMap<
            String,
            (
                IdempotencyClaimCommand,
                Option<IdempotencyCompletion>,
                String,
                Timestamp,
            ),
        >,
    >,
}

#[async_trait::async_trait]
impl IdempotencyStore for FakeIdempotencyStore {
    async fn claim(
        &self,
        command: IdempotencyClaimCommand,
    ) -> Result<IdempotencyClaim, IdentityPortError> {
        let mut records = self
            .results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"));
        if let Some((existing, completion, owner, lease_until)) = records.get(&command.key) {
            if existing.actor_user_id != command.actor_user_id
                || existing.operation != command.operation
                || existing.request_hash != command.request_hash
            {
                return Ok(IdempotencyClaim::Reused);
            }
            if let Some(comp) = completion.clone() {
                return Ok(IdempotencyClaim::Completed(comp));
            }
            if *lease_until <= command.now {
                let new_owner = Uuid::now_v7().to_string();
                let new_lease = command
                    .now
                    .checked_add_millis(30_000)
                    .unwrap_or(command.now);
                if let Some(entry) = records.get_mut(&command.key) {
                    entry.2 = new_owner.clone();
                    entry.3 = new_lease;
                }
                return Ok(IdempotencyClaim::Acquired {
                    owner: new_owner,
                    lease_until: new_lease,
                });
            }
            let diff_ms = lease_until.to_unix_millis().unwrap_or(0)
                - command.now.to_unix_millis().unwrap_or(0);
            let secs = if diff_ms <= 0 {
                1
            } else {
                u64::try_from(diff_ms / 1000).unwrap_or(1)
            };
            let secs = if secs == 0 { 1 } else { secs };
            let _ = owner;
            return Ok(IdempotencyClaim::InProgress {
                retry_after_secs: secs,
            });
        }
        let owner = Uuid::now_v7().to_string();
        let lease_until = command
            .now
            .checked_add_millis(30_000)
            .unwrap_or(command.now);
        records.insert(
            command.key.clone(),
            (command, None, owner.clone(), lease_until),
        );
        Ok(IdempotencyClaim::Acquired { owner, lease_until })
    }

    async fn complete(
        &self,
        key: String,
        owner: String,
        completion: IdempotencyCompletion,
    ) -> Result<(), IdentityPortError> {
        let mut records = self
            .results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"));
        let entry = records
            .get_mut(&key)
            .ok_or(IdentityPortError::Unavailable)?;
        if entry.2 != owner {
            return Err(IdentityPortError::Unavailable);
        }
        if entry.1.is_some() {
            return Err(IdentityPortError::Unavailable);
        }
        entry.1 = Some(completion);
        Ok(())
    }

    async fn abandon(&self, key: String, owner: String) -> Result<(), IdentityPortError> {
        let mut records = self
            .results
            .lock()
            .unwrap_or_else(|error| panic!("fake idempotency lock poisoned: {error}"));
        let entry = records.get(&key).ok_or(IdentityPortError::Unavailable)?;
        if entry.2 != owner {
            return Err(IdentityPortError::Unavailable);
        }
        if entry.1.is_some() {
            return Err(IdentityPortError::Unavailable);
        }
        records.remove(&key);
        Ok(())
    }
}
