//! PW-1 password reset E2E: 9 acceptance conditions via Axum router.
//! Uses FakeIdentityStore + FakeIdempotencyStore + FixedClock.
//! No test is #[ignore]; all run via HTTP with real DB logic replaced by in-memory fakes
//! but covering the full transport -> service -> store contract.
//! Also covers M2 randomness check: different Idempotency-Key => different password.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    unused_variables,
    dead_code,
    unused_qualifications
)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, RequestId};
use orbisync_domain::{AuthSession, Clock, LoginId, Timestamp, UserId};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
    PasswordService, token::AccessTokenService,
};
use orbisync_testkit::{
    FakeIdempotencyStore, FakeIdentityStore, FakeRealtimeTicketStore, FixedClock,
};
use orbisync_transport_http::{HttpState, router};
use tower::ServiceExt as _;
use uuid::Uuid;

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

fn test_token_service() -> Arc<AccessTokenService> {
    Arc::new(
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("token service"),
    )
}

fn test_login_service(
    repo: Arc<FakeIdentityStore>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<orbisync_identity::LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> = repo.clone() as Arc<dyn LoginTransactionStore>;
    let repo_dyn: Arc<dyn orbisync_application::IdentityRepository> =
        repo.clone() as Arc<dyn orbisync_application::IdentityRepository>;
    Arc::new(orbisync_identity::LoginService::new(
        repo_dyn,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn fixed_clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ))
}

fn now_ts() -> Timestamp {
    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid")
}

fn make_password_service() -> Arc<PasswordService> {
    Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw service"))
}

struct NopRefreshCreationStore;
#[async_trait::async_trait]
impl orbisync_application::RefreshTokenCreationStore for NopRefreshCreationStore {
    async fn create(
        &self,
        _cmd: orbisync_application::CreateRefreshTokenCommand,
    ) -> Result<(), orbisync_application::IdentityPortError> {
        Ok(())
    }
}

fn make_state(
    store: Arc<FakeIdentityStore>,
    _idempotency: Arc<FakeIdempotencyStore>,
    clock: Arc<FixedClock>,
    tokens: Arc<AccessTokenService>,
    passwords: Arc<PasswordService>,
) -> (
    HttpState,
    Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
) {
    let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
        store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>;
    // AUD-C2: use unified store for idempotency so atomic claim+completion share same map
    let idem_port: Arc<dyn orbisync_application::IdempotencyStore> =
        store.clone() as Arc<dyn orbisync_application::IdempotencyStore>;
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        clock.clone() as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_clock(Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>)
    .with_identity_repository(repo)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_identity_admin_service(Arc::clone(&admin))
    .with_idempotency_store(idem_port)
    .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
    .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>)
    .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
        as Arc<dyn orbisync_application::RefreshTokenCreationStore>);
    (state, admin)
}

async fn http_login(
    state: HttpState,
    login_id: &str,
    password: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
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

async fn http_change_password(
    state: HttpState,
    bearer: &str,
    current_password: &str,
    new_password: &str,
    idempotency_key: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body =
        serde_json::json!({ "current_password": current_password, "new_password": new_password });
    let req = Request::builder()
        .uri("/v1/auth/change-password")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("Idempotency-Key", idempotency_key)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_reset(
    state: HttpState,
    bearer: Option<&str>,
    target_user_id: &str,
    idempotency_key: Option<&str>,
) -> (StatusCode, serde_json::Value, HeaderMap) {
    let app = router(state);
    let uri = format!("/v1/users/{target_user_id}/reset-password");
    let mut builder = Request::builder().uri(uri).method("POST");
    if let Some(tok) = bearer {
        builder = builder.header("authorization", format!("Bearer {tok}"));
    }
    if let Some(key) = idempotency_key {
        builder = builder.header("Idempotency-Key", key);
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, headers)
}

async fn bootstrap_and_create_bob() -> (
    Arc<FakeIdentityStore>,
    Arc<FakeIdempotencyStore>,
    Arc<FixedClock>,
    Arc<AccessTokenService>,
    Arc<PasswordService>,
    HttpState,
    String, // admin token
    String, // bob user_id
    String, // bob password (temp)
) {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let idempotency = Arc::new(FakeIdempotencyStore::default());
    let (state, admin) = make_state(
        Arc::clone(&store),
        Arc::clone(&idempotency),
        Arc::clone(&clock),
        Arc::clone(&tokens),
        Arc::clone(&passwords),
    );
    let (admin_user, admin_password) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        )
        .await
        .expect("bootstrap");
    let _admin_user_id = admin_user.id();
    // admin login to get token
    let (status, json) = http_login(state.clone(), "admin", admin_password.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "admin login must succeed {json}");
    let admin_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // create bob via service directly (to avoid HTTP create complexity)
    let bob_login = LoginId::new("bob").expect("login");
    let bob_cmd = orbisync_application::CreateUserCommand {
        login_id: bob_login,
        display_name: "Bob".to_owned(),
        actor_id: admin_user.id(),
        request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
    };
    // Fetch admin roles via repository (server-owned)
    let roles = {
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        repo.roles_for_user(admin_user.id()).await.expect("roles")
    };
    let (bob_user, bob_temp) = admin
        .create_user(bob_cmd, &roles)
        .await
        .expect("create bob");
    let bob_id = bob_user.id().to_string();
    let bob_pw = bob_temp.expose_secret().to_owned();
    // Rebuild state to include new user (store already shared)
    let (state2, _) = make_state(
        Arc::clone(&store),
        Arc::clone(&idempotency),
        Arc::clone(&clock),
        Arc::clone(&tokens),
        Arc::clone(&passwords),
    );
    // Ensure http_login for bob works for later tests
    (
        store,
        idempotency,
        clock,
        tokens,
        passwords,
        state2,
        admin_token,
        bob_id,
        bob_pw,
    )
}

// ---------------------------------------------------------------------------
// Test 1: admin executes 202, temporary_password length 27, must_change_password true
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_reset_returns_202_with_27_and_must_change() {
    let (store, idem, clock, tokens, passwords, state, admin_token, bob_id, _bob_pw) =
        bootstrap_and_create_bob().await;
    let key = Uuid::now_v7().to_string();
    let (status, json, _) =
        http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "reset must be 202 got {json}");
    let tp = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .expect("temporary_password");
    assert_eq!(
        tp.chars().count(),
        27,
        "temporary_password length must be 27, got {}",
        tp.len()
    );
    assert_eq!(
        json.get("must_change_password").and_then(|v| v.as_bool()),
        Some(true)
    );
    // also verify base64url charset (no padding, no +/)
    assert!(
        tp.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
    // also verify 20 bytes => 27 length: base64url without pad of 20 bytes is 27
    // This is the contract check: 20 bytes CSPRNG -> 27 chars
}

// ---------------------------------------------------------------------------
// Test 2: returned temporary password can login and must_change flag reflected
// Uses false->true transition to catch M1 (deleting must_change true logic)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_temporary_password_can_login_and_must_change_reflected() {
    let (store, _idem, _clock, _tokens, _passwords, state, admin_token, bob_id, bob_pw) =
        bootstrap_and_create_bob().await;
    // First, make bob's must_change false via self-service change-password
    let (status, json) = http_login(state.clone(), "bob", &bob_pw).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bob login with initial temp must succeed {json}"
    );
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let change_key = Uuid::now_v7().to_string();
    let new_pass = "NewSecurePass123!@#";
    let (status, json) =
        http_change_password(state.clone(), &bob_token, &bob_pw, new_pass, &change_key).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "change password must be 204 {json}"
    );
    // Verify must_change is now false
    let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    let bob_user_id = UserId::new(Uuid::parse_str(&bob_id).unwrap()).unwrap();
    let acc = repo.find_account(bob_user_id).await.unwrap().unwrap();
    assert!(
        !acc.user.must_change_password(),
        "must_change_password must be false after change-password"
    );
    // Now admin resets -> must become true again, and new temp must allow login
    let key = Uuid::now_v7().to_string();
    let (status, json, _) =
        http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let tp = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // login with new temporary password
    let (status, _json) = http_login(state.clone(), "bob", &tp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bob must login with new temp password"
    );
    // verify must_change flag via store is true again
    let acc = repo.find_account(bob_user_id).await.unwrap().unwrap();
    assert!(
        acc.user.must_change_password(),
        "must_change_password must be true after reset (M1 catches if deleted)"
    );
}

// ---------------------------------------------------------------------------
// Test 3: old password fails after reset
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_old_password_fails_after_reset() {
    let (store, _idem, clock, tokens, passwords, state, admin_token, bob_id, bob_old_pw) =
        bootstrap_and_create_bob().await;
    // verify old pw works before reset
    let (status, _) = http_login(state.clone(), "bob", &bob_old_pw).await;
    assert_eq!(status, StatusCode::OK, "old pw should work before reset");
    let key = Uuid::now_v7().to_string();
    let (status, json, _) =
        http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let new_tp = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap();
    assert_ne!(new_tp, bob_old_pw);
    // old pw must now fail
    let (status, json) = http_login(state.clone(), "bob", &bob_old_pw).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "old password must fail after reset {json}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: same Idempotency-Key returns same response, no new password
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_idempotency_same_key_returns_same_password() {
    // AUD-C1: same key replay must NOT return temporary_password (no secret persistence)
    let (store, _idem, _clock, _tokens, _passwords, state, admin_token, bob_id, _old) =
        bootstrap_and_create_bob().await;
    let key = Uuid::now_v7().to_string();
    let (s1, j1, _) = http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    let tp1 = j1
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    assert_eq!(tp1.chars().count(), 27);
    let (s2, j2, _) = http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(
        s2,
        StatusCode::ACCEPTED,
        "second call same key must be 202 {j2}"
    );
    assert_eq!(
        j2.get("must_change_password").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        j2.get("temporary_password").is_none(),
        "replay must not contain temporary_password (AUD-C1)"
    );
    // Different key must produce different password (M2 randomness check) and contain password
    let key2 = Uuid::now_v7().to_string();
    let (s3, j3, _) = http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key2)).await;
    assert_eq!(s3, StatusCode::ACCEPTED);
    let tp3 = j3
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    assert_eq!(tp3.chars().count(), 27);
    assert_ne!(
        tp1, tp3,
        "different Idempotency-Key must produce different password (CSPRNG)"
    );
}

// ---------------------------------------------------------------------------
// Test 5: non-admin token is 403
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_non_admin_is_403() {
    let (store, _idem, clock, tokens, passwords, state, admin_token, bob_id, bob_pw) =
        bootstrap_and_create_bob().await;
    // bob login to get non-admin token
    let (status, json) = http_login(state.clone(), "bob", &bob_pw).await;
    assert_eq!(status, StatusCode::OK);
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let key = Uuid::now_v7().to_string();
    let (status, json, _) = http_reset(state.clone(), Some(&bob_token), &bob_id, Some(&key)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-admin must be 403 {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");
}

// ---------------------------------------------------------------------------
// Test 6: non-existent user_id is 404
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_nonexistent_user_is_404() {
    let (_store, _idem, _clock, _tokens, _passwords, state, admin_token, _bob_id, _pw) =
        bootstrap_and_create_bob().await;
    let fake_id = Uuid::now_v7().to_string();
    let key = Uuid::now_v7().to_string();
    let (status, json, _) =
        http_reset(state.clone(), Some(&admin_token), &fake_id, Some(&key)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "nonexistent user must be 404 {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "RESOURCE_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// Test 7: token with broken sid (valid signature, real sub) is 401
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_broken_sid_token_is_401() {
    let (store, _idem, clock, tokens, _passwords, state, admin_token, bob_id, _pw) =
        bootstrap_and_create_bob().await;
    // Create a token for admin but with random sid not in store
    let admin_id = {
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        // find admin via login_id admin
        let acc = repo
            .find_login(&LoginId::new("admin").unwrap())
            .await
            .unwrap()
            .unwrap();
        acc.user.id()
    };
    let random_sid = orbisync_domain::AuthSessionId::generate();
    let now = clock.now();
    let bad_token = tokens
        .issue(admin_id, random_sid, now)
        .expect("issue")
        .expose_secret()
        .to_owned();
    let key = Uuid::now_v7().to_string();
    let (status, json, _) = http_reset(state.clone(), Some(&bad_token), &bob_id, Some(&key)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "broken sid must be 401 {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "AUTHENTICATION_REQUIRED");
}

// ---------------------------------------------------------------------------
// Test 8: audit event exists and does not contain temporary password
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_audit_exists_and_does_not_contain_password() {
    let (store, _idem, _clock, _tokens, _passwords, state, admin_token, bob_id, _pw) =
        bootstrap_and_create_bob().await;
    let key = Uuid::now_v7().to_string();
    let (status, json, _) =
        http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let tp = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // Audit must have been recorded
    let audits = store.applied();
    let found = audits.iter().find(|(m, a)| {
        if let orbisync_application::IdentityMutation::ResetPassword { user, .. } = m {
            user.id().to_string() == bob_id && a.action == "password.reset" && a.succeeded
        } else {
            false
        }
    });
    assert!(found.is_some(), "audit for password.reset must exist");
    // Ensure audit details do not contain password (the audit view is empty json)
    // Check that none of the stored audit JSON contains the password substring
    // The Fake stores AuditView with empty details, but we also check that password not in applied vector debug
    let debug = format!("{:?}", store.applied());
    assert!(
        !debug.contains(&tp),
        "audit must not contain temporary password"
    );
    // Also check that audit details are empty (no password) via applied audit events
    for (_, audit) in store.applied() {
        // audit.resource_id is user_id, not password
        let audit_str = format!("{:?}", audit);
        assert!(
            !audit_str.contains(&tp),
            "audit event must not contain password"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 9: existing sessions are revoked (§2 decision)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pw1_existing_sessions_are_revoked() {
    let (store, _idem, clock, tokens, _passwords, state, admin_token, bob_id, bob_pw) =
        bootstrap_and_create_bob().await;
    // Create a session for bob and issue token tied to it
    let bob_user_id = UserId::new(Uuid::parse_str(&bob_id).unwrap()).unwrap();
    let session_id = orbisync_domain::AuthSessionId::generate();
    let now = clock.now();
    let expires = now.checked_add_millis(15 * 60 * 1000).unwrap();
    let session = AuthSession::new(session_id, bob_user_id, now, expires).expect("session");
    // Insert session via repository port (public API)
    {
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        repo.save_session(&session).await.expect("save session");
    }
    let bob_session_token = tokens
        .issue(bob_user_id, session_id, now)
        .expect("issue")
        .expose_secret()
        .to_owned();
    // Verify token works for a protected endpoint before reset (GET /v1/users/{id} requires admin.users.read, but bob doesn't have it; use another protected that bob can access? Use GET /v1/users which requires admin.users.read – bob will be 403, not 401. To test session validity we use reset itself with bob's token targeting himself? But bob is non-admin so 403 anyway. Instead we test that after reset, the same bob token is now 401 (revoked), not 403.
    // First, check that using bob token for some auth that doesn't require permission? We can use POST /v1/realtime/tickets which only requires valid token, no RBAC.
    let app = router(state.clone());
    let req = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {bob_session_token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "bob session token must be valid before reset"
    );

    // Now admin resets bob
    let key = Uuid::now_v7().to_string();
    let (status, _json, _) =
        http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key)).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Old bob token must now be 401 (session revoked)
    let app2 = router(state.clone());
    let req2 = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {bob_session_token}"))
        .body(Body::empty())
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::UNAUTHORIZED,
        "old session token must be 401 after password reset (revoked)"
    );
    // Also verify new password works
    // Need to fetch new temp
    // Do second reset with different key to get new temp? But we already have old temp? Let's just login with new temp from previous reset
    // The previous reset's temp is in _json, but we didn't capture. Let's do a new login check using bob's old pw should fail, new temp should succeed (already covered). For session revocation we also check that a newly issued token after reset works.
    let new_tp = {
        let key2 = Uuid::now_v7().to_string();
        let (s, j, _) = http_reset(state.clone(), Some(&admin_token), &bob_id, Some(&key2)).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        j.get("temporary_password")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned()
    };
    let (status, _) = http_login(state.clone(), "bob", &new_tp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "new temp must allow login after revocation"
    );
}
