//! W-15 admin user creation API: HTTP router integration tests.
//!
//! Verifies the 5 acceptance conditions from `worker-tasks-2026-08-10.md` §2
//! via `axum` router (not direct handler calls). Uses `FakeIdentityStore`
//! so created users are visible to the login path without a real database.
//! No test is `#[ignore]`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityAdministrationStore, IdentityRepository, RequestId};
use orbisync_domain::{Clock, LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
    PasswordService, token::AccessTokenService,
};
use orbisync_testkit::{FakeIdentityStore, FakeRealtimeTicketStore, FixedClock};
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
        .expect("test token service"),
    )
}

fn test_login_service(
    store: Arc<FakeIdentityStore>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<orbisync_identity::LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> = store.clone() as Arc<dyn LoginTransactionStore>;
    let repo_dyn: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    Arc::new(orbisync_identity::LoginService::new(
        repo_dyn,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn Clock>,
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
    Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("password service"))
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
    clock: Arc<FixedClock>,
    tokens: Arc<AccessTokenService>,
    passwords: Arc<PasswordService>,
) -> (
    HttpState,
    Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
) {
    let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    let admin_port: Arc<dyn IdentityAdministrationStore> =
        store.clone() as Arc<dyn IdentityAdministrationStore>;
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(Arc::clone(&clock) as Arc<dyn Clock>);
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
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(repo)
    .with_password_service(Arc::clone(&passwords))
    .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>)
    .with_identity_admin_service(Arc::clone(&admin))
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

async fn http_create_user(
    state: HttpState,
    bearer: Option<&str>,
    accept_credential: bool,
    login_id: &str,
    display_name: &str,
) -> (StatusCode, serde_json::Value, HeaderMap) {
    let app = router(state);
    let body = serde_json::json!({ "login_id": login_id, "display_name": display_name });
    let mut builder = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if accept_credential {
        builder = builder.header("accept", "application/vnd.orbisync.user-credential+json");
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, headers)
}

async fn http_ticket(state: HttpState, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let mut builder = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ---------------------------------------------------------------------------
// Acceptance #1: bootstrapped admin can create a user (201)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w15_bootstrap_admin_can_create_user_returns_201() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, admin) = make_state(
        Arc::clone(&store),
        Arc::clone(&clock),
        Arc::clone(&tokens),
        Arc::clone(&passwords),
    );

    // Bootstrap first admin via service (simulates CLI bootstrap-admin).
    let (admin_user, admin_password) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request"),
        )
        .await
        .expect("bootstrap must succeed");

    // Admin login via HTTP to obtain access token – proves bootstrap credential works.
    let (status, json) = http_login(state.clone(), "admin", admin_password.expose_secret()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bootstrap admin login must succeed: {json}"
    );
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .expect("access_token")
        .to_owned();
    assert!(!access_token.is_empty());
    // Sanity: bootstrapped admin is enabled and must_change_password true initially,
    // but login still succeeds (policy allows login before password change).
    assert_eq!(admin_user.login_id().as_str(), "admin");

    // Admin creates a new user via POST /v1/users – must be 201.
    let (status, json, _headers) =
        http_create_user(state.clone(), Some(&access_token), false, "bob", "Bob").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "POST /v1/users must be 201, got {json}"
    );
    assert_eq!(json.get("login_id").and_then(|v| v.as_str()), Some("bob"));
    assert_eq!(
        json.get("display_name").and_then(|v| v.as_str()),
        Some("Bob")
    );
    assert_eq!(json.get("enabled").and_then(|v| v.as_bool()), Some(true));
    // Temporary password must NOT be present in default application/json response (D-10).
    assert!(
        json.get("temporary_password").is_none(),
        "default response must not expose temporary_password"
    );
    assert!(json.get("user").is_none());
}

// ---------------------------------------------------------------------------
// Acceptance #2: created user can login with returned temporary password (most important)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w15_created_user_can_login_with_temporary_password() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, admin) = make_state(
        Arc::clone(&store),
        Arc::clone(&clock),
        Arc::clone(&tokens),
        Arc::clone(&passwords),
    );

    let (_, admin_password) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request"),
        )
        .await
        .expect("bootstrap");

    let (status, json) = http_login(state.clone(), "admin", admin_password.expose_secret()).await;
    assert_eq!(status, StatusCode::OK);
    let admin_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Create bob with Accept requesting credential so we receive temporary_password.
    let (status, json, headers) =
        http_create_user(state.clone(), Some(&admin_token), true, "bob", "Bob").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "credential request must be 201: {json}"
    );
    // Content-Type must be the credential media type (D-10).
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/vnd.orbisync.user-credential+json"),
        "content-type must be credential type, got {ct}"
    );

    let temporary = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .expect("temporary_password must be present when Accept requests it")
        .to_owned();
    assert_eq!(
        temporary.chars().count(),
        27,
        "temporary password must be 27 chars (20 bytes base64url)"
    );
    let user_obj = json.get("user").expect("user object");
    assert_eq!(
        user_obj.get("login_id").and_then(|v| v.as_str()),
        Some("bob")
    );

    // Bob logs in with the one-time temporary password – this is the end-to-end proof that
    // the credential stored by create_user matches the hash the login path verifies.
    // If the handler had written the user row but not the credential correctly, this fails.
    let (status, json) = http_login(state.clone(), "bob", &temporary).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bob must be able to login with the returned temporary password, got {json}"
    );
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .expect("bob access_token");
    assert!(!bob_token.is_empty());

    // Verify default Accept does not leak password: create carol without credential accept
    let (status2, json2, _) =
        http_create_user(state.clone(), Some(&admin_token), false, "carol", "Carol").await;
    assert_eq!(status2, StatusCode::CREATED);
    assert!(
        json2.get("temporary_password").is_none(),
        "without Accept, password must not be returned"
    );
    // But carol can still be created – indirect proof that admin path works regardless.
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = now_ts(); // used to avoid unused warning
}

// ---------------------------------------------------------------------------
// Acceptance #3: non-admin is 403
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w15_non_admin_is_forbidden_403() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, admin) = make_state(
        Arc::clone(&store),
        Arc::clone(&clock),
        Arc::clone(&tokens),
        Arc::clone(&passwords),
    );

    let (_, admin_password) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request"),
        )
        .await
        .expect("bootstrap");

    let (status, json) = http_login(state.clone(), "admin", admin_password.expose_secret()).await;
    assert_eq!(status, StatusCode::OK);
    let admin_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Admin creates bob (regular user, no roles).
    let (status, json, _) =
        http_create_user(state.clone(), Some(&admin_token), true, "bob", "Bob").await;
    assert_eq!(status, StatusCode::CREATED);
    let bob_password = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap();

    // Bob logs in.
    let (status, json) = http_login(state.clone(), "bob", bob_password).await;
    assert_eq!(status, StatusCode::OK, "bob login must succeed {json}");
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Bob tries to create charlie – must be 403 ACCESS_DENIED (not 401).
    let (status, json, _) =
        http_create_user(state.clone(), Some(&bob_token), false, "charlie", "Charlie").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-admin must be 403, got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");

    // Breaking RBAC would make this green incorrectly – document mutation: removing
    // `require(actor_roles, "admin.users.create")` in admin.rs would cause this to be 201.
}

// ---------------------------------------------------------------------------
// Acceptance #4: missing auth is 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w15_missing_auth_is_unauthorized_401() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, _admin) = make_state(store, clock, tokens, passwords);

    let (status, json, _) = http_create_user(state, None, false, "dave", "Dave").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing auth must be 401 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "AUTHENTICATION_REQUIRED");
}

// ---------------------------------------------------------------------------
// Acceptance #5: realtime ticket as bearer is 401 (audience isolation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w15_realtime_ticket_as_bearer_is_unauthorized_401() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, admin) = make_state(
        Arc::clone(&store),
        Arc::clone(&clock),
        Arc::clone(&tokens),
        Arc::clone(&passwords),
    );

    let (_, admin_password) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request"),
        )
        .await
        .expect("bootstrap");

    let (status, json) = http_login(state.clone(), "admin", admin_password.expose_secret()).await;
    assert_eq!(status, StatusCode::OK);
    let admin_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Issue a realtime ticket using the access token.
    let (status, json) = http_ticket(state.clone(), Some(&admin_token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ticket issuance must succeed {json}"
    );
    let ticket = json
        .get("realtime_ticket")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Use the realtime ticket as Bearer for POST /v1/users – must be 401 due to audience mismatch.
    let (status, json, _) =
        http_create_user(state.clone(), Some(&ticket), false, "eve", "Eve").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "realtime ticket must not be accepted as Bearer for POST /v1/users (audience isolation), got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "AUTHENTICATION_REQUIRED");

    // Sanity: the same access token still works for user creation.
    let (status, _, _) = http_create_user(state, Some(&admin_token), false, "frank", "Frank").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "access token should still be valid for /v1/users"
    );
}
