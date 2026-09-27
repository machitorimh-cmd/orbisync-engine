//! rest-users-auth: GET /v1/auth/me, PATCH /v1/users/{user_id},
//! POST /v1/users/{user_id}/disable, POST /v1/users/{user_id}/enable,
//! POST /v1/users/import, POST /v1/auth/logout (HTTP + real PostgreSQL).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::RequestId;
use orbisync_domain::{LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
    PasswordPolicy, PasswordService, token::AccessTokenService,
};
use orbisync_storage_postgres::{
    IdempotencyStore, IdentityAdministrationStore, PgIdentityQueryStore, PgIdentityRepository,
    PgLoginStore, PgRealtimeTicketStore,
};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use uuid::Uuid;

mod common;

/// Serializes every test in this file: `bootstrap_administrator` refuses a
/// second call while any account exists, so concurrent tests against the
/// same real database would spuriously fail each other (same pattern as
/// `w29_identity_read.rs`).
fn db_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

async fn clean(pool: &PgPool) {
    sqlx::query("TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, auth_sessions, refresh_tokens, audit_events, idempotency_records, realtime_tickets CASCADE")
        .execute(pool)
        .await
        .expect("TRUNCATE should succeed");
}

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

fn fixed_clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ))
}

async fn pool() -> Option<PgPool> {
    common::pool_or_skip().await
}

fn test_login_service_pg(
    pool: PgPool,
    repo: Arc<dyn orbisync_application::IdentityRepository>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> = Arc::new(PgLoginStore::new(pool));
    Arc::new(LoginService::new(
        repo,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        b"test-refresh-hmac-key-32bytes!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn http_state(
    pool: PgPool,
    clock: Arc<FixedClock>,
    tokens: Arc<AccessTokenService>,
) -> (
    HttpState,
    Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
) {
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo: Arc<dyn orbisync_application::IdentityRepository> =
        Arc::new(PgIdentityRepository::new(pool.clone()));
    let admin_store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
        admin_store.clone();
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let qstore = Arc::new(PgIdentityQueryStore::with_codec(
        pool.clone(),
        orbisync_testkit::identity::insecure_test_codec(),
    ));
    let iq: Arc<dyn orbisync_application::IdentityQueryPort> = qstore;
    let idem_store = Arc::new(IdempotencyStore::new(pool.clone()));
    let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
        Arc::new(PgRealtimeTicketStore::new(pool.clone()));
    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        b"test-refresh-hmac-key-32bytes!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_login_service(test_login_service_pg(
        pool.clone(),
        repo.clone(),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_identity_repository(repo)
    .with_password_service(passwords)
    .with_identity_admin_service(Arc::clone(&admin))
    .with_identity_query(iq)
    .with_idempotency_store(idem_store as Arc<dyn orbisync_application::IdempotencyStore>)
    .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
    .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    .with_realtime_ticket_store(ticket_store)
    .with_refresh_creation_store(
        admin_store.clone() as Arc<dyn orbisync_application::RefreshTokenCreationStore>
    )
    .with_refresh_rotation_store(
        admin_store as Arc<dyn orbisync_application::RefreshTokenRotationStore>,
    );
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

async fn http_refresh(state: HttpState, refresh_token: &str) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "refresh_token": refresh_token });
    let req = Request::builder()
        .uri("/v1/auth/refresh")
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

async fn http_logout(state: HttpState, bearer: &str) -> StatusCode {
    let app = router(state);
    let req = Request::builder()
        .uri("/v1/auth/logout")
        .method("POST")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

async fn http_me(state: HttpState, bearer: &str) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let req = Request::builder()
        .uri("/v1/auth/me")
        .method("GET")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_create_user(
    state: HttpState,
    bearer: &str,
    login: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "login_id": login, "display_name": login });
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_patch_user(
    state: HttpState,
    bearer: &str,
    user_id: &str,
    if_match: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let req = Request::builder()
        .uri(format!("/v1/users/{user_id}"))
        .method("PATCH")
        .header("content-type", "application/merge-patch+json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("if-match", if_match)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_set_enabled(
    state: HttpState,
    bearer: &str,
    user_id: &str,
    enable: bool,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let action = if enable { "enable" } else { "disable" };
    let req = Request::builder()
        .uri(format!("/v1/users/{user_id}/{action}"))
        .method("POST")
        .header("authorization", format!("Bearer {bearer}"))
        .header("Idempotency-Key", Uuid::now_v7().to_string())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_import_users(
    state: HttpState,
    bearer: &str,
    csv: &str,
    idempotency_key: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let req = Request::builder()
        .uri("/v1/users/import")
        .method("POST")
        .header("content-type", "text/csv")
        .header("authorization", format!("Bearer {bearer}"))
        .header("Idempotency-Key", idempotency_key)
        .body(Body::from(csv.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn bootstrap_admin(
    admin: &IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>,
    login: &str,
) -> orbisync_application::SecretString {
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(login).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    pw
}

// ---------------------------------------------------------------------------
// GET /v1/auth/me
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_me_returns_own_profile() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let login = format!("me-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &login).await;
    let (status, json) = http_login(state.clone(), &login, pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let (status, json) = http_me(state.clone(), &token).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json.get("login_id").and_then(|v| v.as_str()),
        Some(login.as_str())
    );
    assert_eq!(json.get("enabled").and_then(|v| v.as_bool()), Some(true));
}

#[tokio::test]
async fn auth_me_without_token_is_401() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, _admin) = http_state(pool.clone(), clock, tokens);
    let app = router(state);
    let req = Request::builder()
        .uri("/v1/auth/me")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// PATCH /v1/users/{user_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_user_updates_display_name_and_bumps_revision_once() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("patch-admin-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (status, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let target_login = format!("patch-target-{}", Uuid::now_v7());
    let (status, created) = http_create_user(state.clone(), &token, &target_login).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let user_id = created.get("id").unwrap().as_str().unwrap().to_owned();
    let revision = created.get("revision").unwrap().as_u64().unwrap();

    let (status, patched) = http_patch_user(
        state.clone(),
        &token,
        &user_id,
        &format!("\"{revision}\""),
        serde_json::json!({ "display_name": "Renamed", "enabled": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(
        patched.get("display_name").and_then(|v| v.as_str()),
        Some("Renamed")
    );
    assert_eq!(
        patched.get("enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        patched.get("revision").and_then(|v| v.as_u64()),
        Some(revision + 1),
        "both fields in one PATCH must advance the revision exactly once"
    );

    // Stale If-Match must be rejected with a conflict, not silently applied.
    let (status, err) = http_patch_user(
        state.clone(),
        &token,
        &user_id,
        &format!("\"{revision}\""),
        serde_json::json!({ "display_name": "StaleWrite" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "stale If-Match must be 409 {err}"
    );
}

#[tokio::test]
async fn patch_user_missing_target_is_404() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("patch-404-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (status, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let missing_id = Uuid::now_v7().to_string();
    let (status, _) = http_patch_user(
        state.clone(),
        &token,
        &missing_id,
        "\"1\"",
        serde_json::json!({ "display_name": "Nobody" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_user_without_permission_is_403() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("patch-403-admin-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (_, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    let admin_token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let plain_login = format!("patch-403-plain-{}", Uuid::now_v7());
    let (status, created) = http_create_user(state.clone(), &admin_token, &plain_login).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let plain_id = created.get("id").unwrap().as_str().unwrap().to_owned();
    let revision = created.get("revision").unwrap().as_u64().unwrap();
    // The freshly created (non-admin) user has no admin.* permissions, so it
    // cannot patch even itself.
    let temp_login = format!("patch-403-plain-{}", Uuid::now_v7());
    let _ = temp_login;

    // Log in as the plain user by resetting its password would require an extra
    // admin call; instead reuse create_user's returned temporary password via the
    // credential media type.
    let app = router(state.clone());
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .header("authorization", format!("Bearer {admin_token}"))
        .body(Body::from(
            serde_json::json!({ "login_id": format!("patch-403-plain2-{}", Uuid::now_v7()), "display_name": "Plain2" })
                .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let created2: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let plain2_login = created2
        .get("user")
        .and_then(|u| u.get("login_id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let plain2_temp = created2
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (status, login_json) = http_login(state.clone(), &plain2_login, &plain2_temp).await;
    assert_eq!(status, StatusCode::OK, "{login_json}");
    let plain2_token = login_json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let (status, err) = http_patch_user(
        state.clone(),
        &plain2_token,
        &plain_id,
        &format!("\"{revision}\""),
        serde_json::json!({ "display_name": "Hacked" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{err}");
}

// ---------------------------------------------------------------------------
// POST /v1/users/{user_id}/disable and /enable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disable_then_enable_round_trip_and_disabled_login_rejected() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("disable-admin-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (_, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    let admin_token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();

    let target_login = format!("disable-target-{}", Uuid::now_v7());
    let app = router(state.clone());
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .header("authorization", format!("Bearer {admin_token}"))
        .body(Body::from(
            serde_json::json!({ "login_id": target_login, "display_name": "Target" }).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let target_id = created
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let target_temp = created
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Sanity: target can log in while enabled.
    let (status, _) = http_login(state.clone(), &target_login, &target_temp).await;
    assert_eq!(status, StatusCode::OK);

    // Disable.
    let (status, disabled) = http_set_enabled(state.clone(), &admin_token, &target_id, false).await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    assert_eq!(
        disabled.get("enabled").and_then(|v| v.as_bool()),
        Some(false)
    );

    // Idempotent repeat: same 200, no error.
    let (status, disabled_again) =
        http_set_enabled(state.clone(), &admin_token, &target_id, false).await;
    assert_eq!(status, StatusCode::OK, "{disabled_again}");

    // Disabled user cannot log in.
    let (status, _) = http_login(state.clone(), &target_login, &target_temp).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "disabled user must not log in"
    );

    // Re-enable.
    let (status, enabled) = http_set_enabled(state.clone(), &admin_token, &target_id, true).await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    assert_eq!(enabled.get("enabled").and_then(|v| v.as_bool()), Some(true));

    // Now login works again.
    let (status, _) = http_login(state.clone(), &target_login, &target_temp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "re-enabled user must be able to log in"
    );
}

#[tokio::test]
async fn disable_missing_target_is_404() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("disable-404-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (_, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    let admin_token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let missing_id = Uuid::now_v7().to_string();
    let (status, _) = http_set_enabled(state.clone(), &admin_token, &missing_id, false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// POST /v1/users/import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_users_reports_partial_success() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("import-admin-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (_, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    let admin_token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();

    let unique = Uuid::now_v7();
    let existing_login = format!("import-existing-{unique}");
    let (status, _) = http_create_user(state.clone(), &admin_token, &existing_login).await;
    assert_eq!(status, StatusCode::CREATED);

    let new_login = format!("import-new-{unique}");
    let csv = format!(
        "login_id,display_name\n{existing_login},Existing\n{new_login},Brand New\n{new_login},Duplicate In Batch\n"
    );
    let key = Uuid::now_v7().to_string();
    let (status, result) = http_import_users(state.clone(), &admin_token, &csv, &key).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{result}");
    assert_eq!(result.get("total").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(result.get("succeeded").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(result.get("failed").and_then(|v| v.as_u64()), Some(2));
    let rows = result.get("results").and_then(|v| v.as_array()).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].get("status").and_then(|v| v.as_str()),
        Some("failed")
    );
    assert_eq!(
        rows[0].get("error_code").and_then(|v| v.as_str()),
        Some("RESOURCE_CONFLICT"),
        "existing login_id must be reported as RESOURCE_CONFLICT: {:?}",
        rows[0]
    );
    assert_eq!(
        rows[1].get("status").and_then(|v| v.as_str()),
        Some("created")
    );
    assert!(
        rows[1]
            .get("temporary_password")
            .and_then(|v| v.as_str())
            .is_some(),
        "created row must include a temporary_password on the live response"
    );
    assert_eq!(
        rows[2].get("status").and_then(|v| v.as_str()),
        Some("failed")
    );
    assert_eq!(
        rows[2].get("error_code").and_then(|v| v.as_str()),
        Some("RESOURCE_CONFLICT"),
        "duplicate within the same batch must also be RESOURCE_CONFLICT: {:?}",
        rows[2]
    );

    // Replay with the same Idempotency-Key must not mint a second temporary
    // password for the already-created row (AUD-C1).
    let (status, replay) = http_import_users(state.clone(), &admin_token, &csv, &key).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replay}");
    let replay_rows = replay.get("results").and_then(|v| v.as_array()).unwrap();
    assert!(
        replay_rows
            .iter()
            .all(|r| r.get("temporary_password").is_none()),
        "replay must never contain a temporary_password: {replay_rows:?}"
    );
}

#[tokio::test]
async fn import_users_without_permission_is_403() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let admin_login = format!("import-403-admin-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &admin_login).await;
    let (_, json) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    let admin_token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();

    let app = router(state.clone());
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .header("authorization", format!("Bearer {admin_token}"))
        .body(Body::from(
            serde_json::json!({ "login_id": format!("import-403-plain-{}", Uuid::now_v7()), "display_name": "Plain" })
                .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let plain_login = created
        .get("user")
        .and_then(|u| u.get("login_id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let plain_temp = created
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (status, login_json) = http_login(state.clone(), &plain_login, &plain_temp).await;
    assert_eq!(status, StatusCode::OK, "{login_json}");
    let plain_token = login_json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let csv = "login_id,display_name\nirrelevant,Irrelevant\n";
    let key = Uuid::now_v7().to_string();
    let (status, err) = http_import_users(state.clone(), &plain_token, csv, &key).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{err}");
}

// ---------------------------------------------------------------------------
// POST /v1/auth/logout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_revokes_session_and_old_refresh_token_is_rejected() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let login = format!("logout-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &login).await;
    let (status, json) = http_login(state.clone(), &login, pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let access_token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let refresh_token = json
        .get("refresh_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();

    // Sanity: the refresh token works before logout.
    let (status, refreshed) = http_refresh(state.clone(), &refresh_token).await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    let rotated_refresh_token = refreshed
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Logout revokes the session tied to the access token.
    let status = http_logout(state.clone(), &access_token).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The regression this task exists to prevent: after logout, the refresh
    // token belonging to the now-revoked session must be rejected, because
    // `rotate_refresh_token_with_source_ip` joins `auth_sessions` and requires
    // `status = 'active'`.
    let (status, rejected) = http_refresh(state.clone(), &rotated_refresh_token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "refresh token from a logged-out session must be rejected: {rejected}"
    );

    // `IdentityAdministrationService::logout` itself is state-idempotent
    // (revoking an already-revoked session is a no-op returning `Ok(())`),
    // but that path is unreachable through this endpoint: `authenticate()`
    // rejects the access token once its own session is no longer active, so
    // a second logout with the same token is 401, not a replay 204.
    let status = http_logout(state.clone(), &access_token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an access token bound to an already-revoked session cannot authenticate"
    );
}

#[tokio::test]
async fn logout_without_token_is_401() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, _admin) = http_state(pool.clone(), clock, tokens);
    let app = router(state);
    let req = Request::builder()
        .uri("/v1/auth/logout")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// ADR-026 T19: an administrator must not be able to create an account under a
/// prefix the server reserves for the subjects it generates.
///
/// `LoginId::new` accepts any non-control text, so without an explicit refusal
/// `guest:<uuid>` would be a perfectly legal account name and could collide
/// with, or pass for, a generated guest.
#[tokio::test]
async fn creating_a_user_under_a_reserved_login_prefix_is_refused() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (state, admin) = http_state(pool.clone(), clock, tokens);
    let login = format!("admin-{}", Uuid::now_v7());
    let pw = bootstrap_admin(&admin, &login).await;
    let (status, json) = http_login(state.clone(), &login, pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let token = json
        .get("access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();

    for reserved in [
        format!("guest:{}", Uuid::now_v7()),
        format!("name:{}", Uuid::now_v7()),
        format!("ext:{}", Uuid::now_v7()),
        "guest:anything".to_owned(),
    ] {
        let (status, json) = http_create_user(state.clone(), &token, &reserved).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "`{reserved}` must be refused, got {json}"
        );
        assert_eq!(
            json.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST"),
            "{json}"
        );
    }

    // A login id that merely begins with the same letters is not reserved and
    // must still be usable, so the guard cannot be a blanket substring ban.
    let (status, json) = http_create_user(state.clone(), &token, "guestbook").await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
}
