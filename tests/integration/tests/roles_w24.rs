//! W-24 role management: HTTP router integration tests.
//!
//! Verifies that `POST /v1/roles` and `PUT /v1/users/{user_id}/roles` are
//! wired through the real Axum router and that RBAC is enforced from the
//! outside (not via direct application calls). Uses `FakeIdentityStore` so
//! created roles and assignments are visible to subsequent `POST /v1/users`
//! authorization checks.
//!
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
    login_id: &str,
    display_name: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "login_id": login_id, "display_name": display_name });
    let mut builder = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_create_role(
    state: HttpState,
    bearer: Option<&str>,
    name: &str,
    permissions: &[&str],
) -> (StatusCode, serde_json::Value, HeaderMap) {
    let app = router(state);
    let body = serde_json::json!({
        "name": name,
        "permissions": permissions
    });
    let mut builder = Request::builder()
        .uri("/v1/roles")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, headers)
}

async fn http_replace_roles(
    state: HttpState,
    bearer: Option<&str>,
    user_id: &str,
    role_ids: &[String],
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "role_ids": role_ids });
    let uri = format!("/v1/users/{user_id}/roles");
    let mut builder = Request::builder()
        .uri(uri)
        .method("PUT")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
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
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w24_admin_can_create_role_201() {
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
    assert_eq!(status, StatusCode::OK, "admin login {json}");
    let admin_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let (status, json, _) = http_create_role(
        state.clone(),
        Some(&admin_token),
        "UserCreator",
        &["admin.users.create"],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "POST /v1/roles must be 201 got {json}"
    );
    assert_eq!(
        json.get("name").and_then(|v| v.as_str()),
        Some("UserCreator")
    );
    let perms = json
        .get("permissions")
        .and_then(|v| v.as_array())
        .expect("permissions");
    assert!(
        perms
            .iter()
            .any(|v| v.as_str() == Some("admin.users.create"))
    );
    assert!(json.get("id").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn w24_non_admin_cannot_create_role_403() {
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

    // Create bob
    let (status, _json) = http_create_user(state.clone(), Some(&admin_token), "bob", "Bob").await;
    assert_eq!(status, StatusCode::CREATED);
    // Get bob's id via login to find user? Instead create via admin and then login bob to get token.
    // We need bob's temporary password for login – create with credential acceptance is not via HTTP for bob,
    // but we have http_create_user without credential accept so we don't get temp password.
    // Instead bootstrap admin created bob with temp password internally, but we didn't capture it.
    // Workaround: create bob via admin service directly to capture password, or use HTTP with credential accept.
    // Use HTTP role create path: we have admin token, create bob with credential accept manually.
    // For this test we need bob token without admin perms.
    // Create charlie via direct admin service to get password, then login.
    // Simpler: create a role for bob assignment, but for 403 test we just need bob's token.
    // Let's create bob via HTTP credential path.
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": "bob2", "display_name": "Bob2" });
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {admin_token}"))
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let bob_pw = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (status, json) = http_login(state.clone(), "bob2", &bob_pw).await;
    assert_eq!(status, StatusCode::OK, "bob2 login {json}");
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let (status, json, _) = http_create_role(
        state.clone(),
        Some(&bob_token),
        "ShouldFail",
        &["admin.users.create"],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-admin must be 403 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");
}

#[tokio::test]
async fn w24_create_role_missing_auth_401() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, _admin) = make_state(store, clock, tokens, passwords);
    let (status, json, _) = http_create_role(state, None, "NoAuth", &["admin.users.create"]).await;
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

#[tokio::test]
async fn w24_create_role_realtime_ticket_is_401() {
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
    let (status, json) = http_ticket(state.clone(), Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK, "ticket {json}");
    let ticket = json
        .get("realtime_ticket")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (status, json, _) = http_create_role(
        state.clone(),
        Some(&ticket),
        "TicketRole",
        &["admin.users.create"],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "realtime ticket must be 401 got {json}"
    );
}

#[tokio::test]
async fn w24_assign_role_and_authz_proves_wiring() {
    // Core W-24 acceptance: create a role that grants admin.users.create, assign it to bob,
    // then prove bob can now POST /v1/users (which was 403 before assignment).
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

    // Create bob via HTTP with credential to get temp password and id.
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": "bob", "display_name": "Bob" });
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {admin_token}"))
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let bob_pw = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let bob_id = json
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let (status, json) = http_login(state.clone(), "bob", &bob_pw).await;
    assert_eq!(status, StatusCode::OK, "bob login {json}");
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Bob cannot create users yet (403).
    let (status, json) =
        http_create_user(state.clone(), Some(&bob_token), "charlie", "Charlie").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bob without role must be 403 got {json}"
    );

    // Admin creates a role that grants admin.users.create.
    let (status, json, _) = http_create_role(
        state.clone(),
        Some(&admin_token),
        "UserCreator",
        &["admin.users.create"],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "role create {json}");
    let role_id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();

    // Admin assigns that role to bob.
    let (status, json) = http_replace_roles(
        state.clone(),
        Some(&admin_token),
        &bob_id,
        std::slice::from_ref(&role_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "assign must be 200 got {json}");
    assert_eq!(
        json.get("user_id").and_then(|v| v.as_str()),
        Some(bob_id.as_str())
    );
    let assigned = json.get("role_ids").and_then(|v| v.as_array()).unwrap();
    assert!(assigned.iter().any(|v| v.as_str() == Some(&role_id)));

    // Now bob can create users (201) – proves assignment is wired and RBAC re-evaluates from DB.
    let (status, json) =
        http_create_user(state.clone(), Some(&bob_token), "charlie", "Charlie").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "bob with UserCreator role must be able to create users, got {json}"
    );
    assert_eq!(
        json.get("login_id").and_then(|v| v.as_str()),
        Some("charlie")
    );

    // Negative: assigning without admin perms is 403, and does not grant.
    // Create dave (no roles) and try to assign a role using bob's token which now has create but not assign.
    // First create dave via bob (now authorized)
    let (_status, _json) = http_create_user(state.clone(), Some(&bob_token), "dave", "Dave").await;
    // bob shouldn't have assign yet, but bob can create dave after previous grant.
    // For the 403 test, use bob's token to try to assign role to dave – should be 403 because bob lacks admin.roles.assign.
    // Need dave id: we need to get dave id via creation response. We already have charlie; create dave separately.
    // Let's create dave via admin to isolate, then attempt assign with bob.
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": "dave2", "display_name": "Dave2" });
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {admin_token}"))
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json2: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let dave_id = json2
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let (status, json) = http_replace_roles(
        state.clone(),
        Some(&bob_token),
        &dave_id,
        std::slice::from_ref(&role_id),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bob without assign perm must be 403 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");
}

#[tokio::test]
async fn w24_assign_roles_missing_auth_401() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = make_password_service();
    let (state, _admin) = make_state(store, clock, tokens, passwords);
    let fake_user = Uuid::now_v7().to_string();
    let fake_role = Uuid::now_v7().to_string();
    let (status, json) = http_replace_roles(state, None, &fake_user, &[fake_role]).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing auth must be 401 got {json}"
    );
}

#[tokio::test]
async fn w24_assign_roles_invalid_user_id_400() {
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
    let (status, json) =
        http_replace_roles(state.clone(), Some(&admin_token), "not-a-uuid", &[]).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid user_id must be 400 got {json}"
    );
}
