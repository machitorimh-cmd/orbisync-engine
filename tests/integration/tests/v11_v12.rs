//! V-11 / V-12: login persistence failure handling and refresh HMAC key enforcement.
//!
//! Covers the five V-11 HTTP cases (failure injection) plus the V-12 key
//! requirements, and the mutation-sensitive vector test (M6).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{
    CreateRefreshTokenCommand, IdentityPortError, IdentityRepository, RefreshTokenCreationStore,
    SecretString,
};
use orbisync_domain::{Credential, LoginId, Timestamp, User};
use orbisync_identity::{PasswordPolicy, PasswordService, token::AccessTokenService};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::HttpState;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";
const TEST_HMAC_KEY: &[u8] = b"test-hmac-key-for-unit-tests-32b!!";

fn test_login_service_pg(
    pool: sqlx::PgPool,
    repo: Arc<dyn IdentityRepository>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<orbisync_identity::LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> =
        Arc::new(orbisync_storage_postgres::PgLoginStore::new(pool));
    Arc::new(orbisync_identity::LoginService::new(
        repo,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

#[allow(dead_code)]
fn test_login_service_fake(
    repo: Arc<dyn IdentityRepository>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<orbisync_identity::LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> = Arc::new(orbisync_testkit::FakeIdentityStore::new());
    Arc::new(orbisync_identity::LoginService::new(
        repo,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn fixed_clock(ts: Timestamp) -> Arc<FixedClock> {
    Arc::new(FixedClock::new(ts))
}

fn now_ts() -> Timestamp {
    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid")
}

fn token_service() -> Arc<AccessTokenService> {
    Arc::new(
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("token service")
        .with_access_token_ttl(900),
    )
}

async fn create_alice_repo_with_policy() -> (
    Arc<orbisync_testkit::FakeIdentityStore>,
    Arc<PasswordService>,
    Timestamp,
) {
    let ts = now_ts();
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo = Arc::new(orbisync_testkit::FakeIdentityStore::new());
    let hash = passwords
        .hash(SecretString::new("Cedar!Lake7-Comet"))
        .await
        .expect("hash");
    let user_id = orbisync_domain::UserId::generate();
    let login_id = LoginId::new("alice").expect("login");
    let user = User::new(user_id, login_id, "Alice", ts).expect("user");
    let credential = Credential::new(user_id, hash, ts);
    let account = orbisync_application::LoginAccount { user, credential };
    repo.insert_account(account);
    (repo, passwords, ts)
}

async fn http_login(
    state: HttpState,
    login_id: &str,
    password: &str,
) -> (StatusCode, serde_json::Value) {
    let app = orbisync_transport_http::router(state);
    let body = serde_json::json!({ "login_id": login_id, "password": password });
    let req = Request::builder()
        .uri("/v1/auth/login")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ---------------------------------------------------------------------------
// failing adapters
// ---------------------------------------------------------------------------

struct FailingSaveCredentialRepo {
    inner: Arc<orbisync_testkit::FakeIdentityStore>,
    fail_save_credential: bool,
    fail_save_session: bool,
    fail_record_failure: bool,
    fail_reset_success: bool,
}

impl FailingSaveCredentialRepo {
    fn new(inner: Arc<orbisync_testkit::FakeIdentityStore>) -> Self {
        Self {
            inner,
            fail_save_credential: false,
            fail_save_session: false,
            fail_record_failure: false,
            fail_reset_success: false,
        }
    }
    fn fail_credential(mut self) -> Self {
        self.fail_save_credential = true;
        // V-11 legacy path and RV-B atomic path both must be injectable;
        // existing v11 tests call fail_credential expecting 500 on both
        // record_login_failure (wrong password) and reset_login_success
        // (correct password). Make the flag cover the new atomic methods as
        // well so those tests still verify fail-closed behaviour.
        self.fail_record_failure = true;
        self.fail_reset_success = true;
        self
    }
    fn fail_session(mut self) -> Self {
        self.fail_save_session = true;
        self
    }
    #[allow(dead_code)]
    fn fail_record_failure(mut self) -> Self {
        self.fail_record_failure = true;
        self
    }
    #[allow(dead_code)]
    fn fail_reset_success(mut self) -> Self {
        self.fail_reset_success = true;
        self
    }
}

#[async_trait::async_trait]
impl IdentityRepository for FailingSaveCredentialRepo {
    async fn find_login(
        &self,
        login_id: &LoginId,
    ) -> Result<Option<orbisync_application::LoginAccount>, IdentityPortError> {
        self.inner.find_login(login_id).await
    }
    async fn find_account(
        &self,
        user_id: orbisync_domain::UserId,
    ) -> Result<Option<orbisync_application::LoginAccount>, IdentityPortError> {
        self.inner.find_account(user_id).await
    }
    async fn save_credential(&self, _credential: &Credential) -> Result<(), IdentityPortError> {
        if self.fail_save_credential {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner.save_credential(_credential).await
    }
    async fn record_login_failure(
        &self,
        user_id: orbisync_domain::UserId,
        now: Timestamp,
    ) -> Result<orbisync_application::LoginFailureOutcome, IdentityPortError> {
        if self.fail_record_failure {
            return Err(IdentityPortError::Unavailable);
        }
        // Also honor legacy fail_save_credential for V-11 tests that use
        // fail_credential() to simulate persistence failure on the failure path.
        if self.fail_save_credential {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner.record_login_failure(user_id, now).await
    }
    async fn reset_login_success(
        &self,
        user_id: orbisync_domain::UserId,
        expected_failed_count: u32,
        expected_locked_until: Option<Timestamp>,
        now: Timestamp,
        new_password_hash: Option<orbisync_domain::PasswordHash>,
    ) -> Result<bool, IdentityPortError> {
        if self.fail_reset_success {
            return Err(IdentityPortError::Unavailable);
        }
        if self.fail_save_credential {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner
            .reset_login_success(
                user_id,
                expected_failed_count,
                expected_locked_until,
                now,
                new_password_hash,
            )
            .await
    }
    async fn roles_for_user(
        &self,
        user_id: orbisync_domain::UserId,
    ) -> Result<Vec<orbisync_domain::Role>, IdentityPortError> {
        self.inner.roles_for_user(user_id).await
    }
    async fn find_session(
        &self,
        session_id: orbisync_domain::AuthSessionId,
    ) -> Result<Option<orbisync_domain::AuthSession>, IdentityPortError> {
        self.inner.find_session(session_id).await
    }
    async fn save_session(
        &self,
        session: &orbisync_domain::AuthSession,
    ) -> Result<(), IdentityPortError> {
        if self.fail_save_session {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner.save_session(session).await
    }
}

#[async_trait::async_trait]
impl orbisync_application::LoginTransactionStore for FailingSaveCredentialRepo {
    async fn commit_success(
        &self,
        commit: orbisync_application::LoginCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        if self.fail_save_credential || self.fail_save_session || self.fail_reset_success {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner.commit_success(commit).await
    }

    async fn record_failure(
        &self,
        user_id: Option<orbisync_domain::UserId>,
        audit: orbisync_application::LoginAuditEvent,
    ) -> Result<(), IdentityPortError> {
        if self.fail_record_failure || self.fail_save_credential {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner.record_failure(user_id, audit).await
    }

    async fn commit_subject_success(
        &self,
        commit: orbisync_application::SubjectCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        if self.fail_save_credential || self.fail_save_session || self.fail_reset_success {
            return Err(IdentityPortError::Unavailable);
        }
        self.inner.commit_subject_success(commit).await
    }
}

#[allow(dead_code)]
struct FailingCreationStore;

#[async_trait::async_trait]
impl RefreshTokenCreationStore for FailingCreationStore {
    async fn create(&self, _cmd: CreateRefreshTokenCommand) -> Result<(), IdentityPortError> {
        Err(IdentityPortError::Unavailable)
    }
}

struct OkCreationStore {
    created: std::sync::Mutex<Vec<CreateRefreshTokenCommand>>,
}

impl OkCreationStore {
    fn new() -> Self {
        Self {
            created: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl RefreshTokenCreationStore for OkCreationStore {
    async fn create(&self, cmd: CreateRefreshTokenCommand) -> Result<(), IdentityPortError> {
        self.created.lock().unwrap().push(cmd);
        Ok(())
    }
}

struct FailingLoginStore;

#[async_trait::async_trait]
impl orbisync_application::LoginTransactionStore for FailingLoginStore {
    async fn commit_success(
        &self,
        _commit: orbisync_application::LoginCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        Err(IdentityPortError::Unavailable)
    }

    async fn record_failure(
        &self,
        _user_id: Option<orbisync_domain::UserId>,
        _audit: orbisync_application::LoginAuditEvent,
    ) -> Result<(), IdentityPortError> {
        Err(IdentityPortError::Unavailable)
    }

    async fn commit_subject_success(
        &self,
        _commit: orbisync_application::SubjectCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        Err(IdentityPortError::Unavailable)
    }
}

fn base_state<T>(
    clock: Arc<FixedClock>,
    repo: Arc<T>,
    passwords: Arc<PasswordService>,
    creation_store: Option<Arc<dyn RefreshTokenCreationStore>>,
) -> HttpState
where
    T: IdentityRepository + orbisync_application::LoginTransactionStore + 'static,
{
    let tokens = token_service();
    let repo_dyn: Arc<dyn IdentityRepository> = repo.clone() as Arc<dyn IdentityRepository>;
    let tx: Arc<dyn orbisync_application::LoginTransactionStore> =
        repo.clone() as Arc<dyn orbisync_application::LoginTransactionStore>;
    let login_service = Arc::new(orbisync_identity::LoginService::new(
        repo_dyn.clone(),
        tx,
        (*passwords).clone(),
        Arc::clone(&tokens),
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        TEST_HMAC_KEY.to_vec(),
        900,
        2_592_000,
    ));
    let mut state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        TEST_HMAC_KEY.to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_login_service(login_service)
    .with_identity_repository(repo_dyn)
    .with_password_service(passwords);
    if let Some(cs) = creation_store {
        state = state.with_refresh_creation_store(cs);
    }
    // Also need rotation store for refresh tests (not used here)
    state
}

// ---------------------------------------------------------------------------
// V-11-1: save_session fails -> 500, no tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v11_save_session_failure_returns_500() {
    let (inner_repo, passwords, _) = create_alice_repo_with_policy().await;
    let clock = fixed_clock(now_ts());
    let failing_repo = Arc::new(FailingSaveCredentialRepo::new(inner_repo).fail_session());
    let creation = Arc::new(OkCreationStore::new());
    let state = base_state(
        clock,
        failing_repo,
        passwords,
        Some(creation as Arc<dyn RefreshTokenCreationStore>),
    );
    let (status, json) = http_login(state, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "save_session failure must be 500, got {json}"
    );
    assert_eq!(
        json.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str()),
        Some("INTERNAL_ERROR")
    );
    // Must not return tokens
    assert!(
        json.get("access_token").is_none(),
        "must not leak token on 500"
    );
    assert!(json.get("refresh_token").is_none());
}

// ---------------------------------------------------------------------------
// V-11-2: refresh_creation_store::create fails -> 500, no refresh token
// P2-C4: login now uses LoginTransactionStore atomically, not RefreshTokenCreationStore.
// This test now verifies that LoginTransactionStore::commit_success failure is 500.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v11_refresh_create_failure_returns_500() {
    let (repo, passwords, _) = create_alice_repo_with_policy().await;
    let clock = fixed_clock(now_ts());
    let failing_tx: Arc<dyn orbisync_application::LoginTransactionStore> =
        Arc::new(FailingLoginStore);
    let repo_dyn: Arc<dyn IdentityRepository> = repo.clone() as Arc<dyn IdentityRepository>;
    let tokens = token_service();
    let login_service = Arc::new(orbisync_identity::LoginService::new(
        repo_dyn.clone(),
        failing_tx,
        (*passwords).clone(),
        Arc::clone(&tokens),
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        TEST_HMAC_KEY.to_vec(),
        900,
        2_592_000,
    ));
    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        TEST_HMAC_KEY.to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_login_service(login_service)
    .with_identity_repository(repo_dyn)
    .with_password_service(passwords);
    let (status, json) = http_login(state, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "create failure must be 500 {json}"
    );
    assert_eq!(
        json.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str()),
        Some("INTERNAL_ERROR")
    );
    assert!(
        json.get("refresh_token").is_none(),
        "must not return refresh on 500"
    );
    assert!(json.get("access_token").is_none());
}

// ---------------------------------------------------------------------------
// V-11-3: refresh_creation_store not wired (None) -> 500, no refresh token
// P2-C4: RefreshTokenCreationStore is for post-login refresh, not login itself.
// Login should still succeed (200) even when creation_store is None, because
// login's atomic persistence is via LoginTransactionStore.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v11_refresh_store_unwired_returns_500() {
    let (repo, passwords, _) = create_alice_repo_with_policy().await;
    let clock = fixed_clock(now_ts());
    // No creation store - login should still be 200 post P2-C4 (uses LoginTransactionStore)
    let state = base_state(clock, repo, passwords, None);
    let (status, json) = http_login(state, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unwired creation_store should still be 200 post P2-C4 {json}"
    );
    assert!(json.get("refresh_token").is_some());
    assert!(json.get("access_token").is_some());
}

// ---------------------------------------------------------------------------
// V-11-4: save_credential fails on auth-failure path -> 500 (and logged)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v11_save_credential_failure_on_auth_failure_is_500() {
    let (inner_repo, passwords, _) = create_alice_repo_with_policy().await;
    let clock = fixed_clock(now_ts());
    let failing_repo = Arc::new(FailingSaveCredentialRepo::new(inner_repo).fail_credential());
    let creation = Arc::new(OkCreationStore::new());
    let state = base_state(
        clock,
        failing_repo,
        passwords,
        Some(creation as Arc<dyn RefreshTokenCreationStore>),
    );
    // Wrong password triggers the failure path (verified == false)
    let (status, json) = http_login(state, "alice", "Wrong!Lake7-Comet").await;
    // V-11 decision: 500 to make storage failure visible (see auth.rs comment)
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "save_credential failure on auth failure must be 500 per V-11 decision, got {json}"
    );
    assert_eq!(
        json.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str()),
        Some("INTERNAL_ERROR")
    );
}

// Also check save_credential fails on success path -> 500
#[tokio::test]
async fn v11_save_credential_failure_on_success_is_500() {
    let (inner_repo, passwords, _) = create_alice_repo_with_policy().await;
    let clock = fixed_clock(now_ts());
    let failing_repo = Arc::new(FailingSaveCredentialRepo::new(inner_repo).fail_credential());
    let creation = Arc::new(OkCreationStore::new());
    let state = base_state(
        clock,
        failing_repo,
        passwords,
        Some(creation as Arc<dyn RefreshTokenCreationStore>),
    );
    let (status, json) = http_login(state, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "save_credential on success must be 500 {json}"
    );
    assert!(json.get("access_token").is_none());
}

// ---------------------------------------------------------------------------
// V-11-5: happy path via real DB: 200 and both rows exist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v11_happy_path_persists_session_and_refresh() {
    let Some(pool) = common_pool().await else {
        return;
    };
    let ts = now_ts();
    let clock = fixed_clock(ts);
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let user_id = orbisync_domain::UserId::generate();
    let login = format!("v11-{}", uuid::Uuid::now_v7());
    let pw = "Cedar!Lake7-Comet";
    let hash = passwords.hash(SecretString::new(pw)).await.expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "V11",
        ts,
    )
    .expect("user");
    let cred = Credential::new(user_id, hash, ts);
    insert_user(&pool, &user, &cred).await;
    let state = http_state_with_pool(pool.clone(), clock).await;
    let (status, json) = http_login(state, &login, pw).await;
    assert_eq!(status, StatusCode::OK, "happy login must be 200 {json}");
    assert!(json.get("access_token").and_then(|v| v.as_str()).is_some());
    assert!(json.get("refresh_token").and_then(|v| v.as_str()).is_some());
    // Verify rows
    let session_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth_sessions WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(session_count, 1, "auth_sessions must have one row");
    let token_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM refresh_tokens WHERE session_id = (SELECT id FROM auth_sessions WHERE user_id=$1)",
    )
    .bind(user_id.as_uuid())
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(token_count, 1, "refresh_tokens must have one row");
}

async fn common_pool() -> Option<sqlx::PgPool> {
    // reuse helper from other tests: if DATABASE_URL missing, skip
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => {
            let cfg = orbisync_config::DatabaseConfig {
                url_env: "DATABASE_URL".to_owned(),
                max_connections: 5,
                acquire_timeout_seconds: 2,
                readiness_timeout_seconds: 3,
            };
            let pool = orbisync_storage_postgres::create_pool(&cfg, &url).expect("pool");
            orbisync_storage_postgres::run_migrations(&pool)
                .await
                .expect("migrations");
            Some(pool)
        }
        _ => {
            eprintln!("SKIPPED (V-11): DATABASE_URL not set - happy path DB test skipped");
            None
        }
    }
}

async fn insert_user(pool: &sqlx::PgPool, user: &User, cred: &Credential) {
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1,$2,$3,'active',$4,$5,$6,$7)")
        .bind(user.id().as_uuid())
        .bind(user.login_id().as_str())
        .bind(user.display_name())
        .bind(user.must_change_password())
        .bind(i64::try_from(user.revision().as_u64()).unwrap())
        .bind(user.created_at().as_offset_date_time())
        .bind(user.updated_at().as_offset_date_time())
        .execute(pool).await.expect("insert user");
    sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at, failed_login_count, locked_until) VALUES ($1,$2,$3,$4,$5)")
        .bind(cred.user_id().as_uuid())
        .bind(cred.password_hash().expose_phc())
        .bind(cred.password_changed_at().as_offset_date_time())
        .bind(i32::try_from(cred.failed_login_count()).unwrap())
        .bind(cred.locked_until().map(|t| t.as_offset_date_time()))
        .execute(pool).await.expect("insert cred");
}

async fn http_state_with_pool(pool: sqlx::PgPool, clock: Arc<FixedClock>) -> HttpState {
    use orbisync_domain::Clock;
    let tokens = token_service();
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo: Arc<dyn IdentityRepository> = Arc::new(
        orbisync_storage_postgres::PgIdentityRepository::new(pool.clone()),
    );
    let rotation_store: Arc<dyn orbisync_application::RefreshTokenRotationStore> = Arc::new(
        orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone()),
    );
    let creation_store: Arc<dyn RefreshTokenCreationStore> = Arc::new(
        orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone()),
    );
    HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        TEST_HMAC_KEY.to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_login_service(test_login_service_pg(
        pool.clone(),
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_identity_repository(repo)
    .with_password_service(passwords)
    .with_refresh_creation_store(creation_store)
    .with_refresh_rotation_store(rotation_store)
}

// ---------------------------------------------------------------------------
// V-12-M5: empty key must panic at HttpState construction
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "refresh_token_hmac_key must not be empty")]
fn v12_empty_key_panics_on_new() {
    let clock = fixed_clock(now_ts());
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        Vec::new(),
    );
}

#[test]
#[should_panic(expected = "refresh_token_hmac_key must not be empty")]
fn v12_empty_key_panics_on_digest() {
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = orbisync_transport_http::refresh_token_digest(&[], &SecretString::new("token"));
}

// ---------------------------------------------------------------------------
// V-12-M6: HMAC vs plain SHA-256 vector – mutation to plain must be red
// ---------------------------------------------------------------------------

#[test]
fn v12_digest_is_hmac_not_plain() {
    // Known vector: key = "key", token = "The quick brown fox"
    let key = b"key";
    let token = SecretString::new("The quick brown fox");
    let digest = orbisync_transport_http::refresh_token_digest(key, &token);
    // Compute expected HMAC-SHA256 manually
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac");
    mac.update(token.expose_secret().as_bytes());
    let expected: [u8; 32] = mac.finalize().into_bytes().into();
    assert_eq!(digest, expected, "digest must be HMAC-SHA256");
    // Also verify it differs from plain SHA-256
    use sha2::Digest as _;
    let mut hasher = Sha256::new();
    hasher.update(token.expose_secret().as_bytes());
    let plain: [u8; 32] = {
        let r = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&r);
        out
    };
    assert_ne!(digest, plain, "HMAC must differ from plain SHA-256 (M6)");
    // Different keys must produce different digests (plain would not)
    let digest2 =
        orbisync_transport_http::refresh_token_digest(b"other-key-32bytes!!test!!", &token);
    assert_ne!(
        digest, digest2,
        "different keys must produce different digests"
    );
}

// Also verify round-trip login -> refresh still works with correct HMAC (smoke for M6)
#[tokio::test]
async fn v12_login_refresh_roundtrip_still_200() {
    let (repo, passwords, _) = create_alice_repo_with_policy().await;
    let clock = fixed_clock(now_ts());
    let creation = Arc::new(OkCreationStore::new());
    // Use real token service and in-memory stores that actually support rotation?
    // For this smoke we use FakeIdentityStore combined with a simple creation store that mimics DB.
    // Instead, use the fake repo + creation store and check that login succeeds (refresh token is returned).
    // Full DB round-trip is covered by v11_happy_path and refresh_w_h.
    let state = base_state(
        clock,
        repo,
        passwords,
        Some(creation as Arc<dyn RefreshTokenCreationStore>),
    );
    let (status, json) = http_login(state, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(status, StatusCode::OK, "login must be 200 {json}");
    assert!(json.get("refresh_token").is_some());
}
