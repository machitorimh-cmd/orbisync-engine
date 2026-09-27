//! W-G CR-05 delete_role integration tests via real HTTP + Postgres.
//! Covers 8 scenarios + audit, and validates parse_if_match wiring.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use orbisync_application::{AuditQueryPort, IdentityQueryPort, RequestId};
use orbisync_domain::{LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
    PasswordPolicy, PasswordService, token::AccessTokenService,
};
use orbisync_storage_postgres::{PgIdentityQueryStore, PgIdentityRepository, PgLoginStore};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use uuid::Uuid;

mod common;

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

fn db_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

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

async fn database_pool() -> Option<sqlx::PgPool> {
    common::pool_or_skip().await
}

async fn clean_identity(pool: &sqlx::PgPool) {
    let res = sqlx::query("TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, auth_sessions, refresh_tokens, audit_events, idempotency_records CASCADE")
        .execute(pool)
        .await;
    if let Err(e) = res {
        panic!("clean users truncate failed: {e}");
    }
    let res2 = sqlx::query(
        "TRUNCATE world_definitions, world_instances, instance_checkpoints, outbox_events CASCADE",
    )
    .execute(pool)
    .await;
    if let Err(e) = res2 {
        eprintln!("clean world truncate warning: {e}");
    }
    let cnt: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("count users");
    assert_eq!(cnt, 0, "users must be 0 after clean, got {cnt}");
}

async fn setup_state(
    pool: sqlx::PgPool,
) -> (
    HttpState,
    Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
    sqlx::PgPool,
) {
    let clock = Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ));
    let tokens = test_token_service();
    let passwords =
        Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("password service"));
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()))
        as Arc<dyn orbisync_application::IdentityRepository>;
    let admin_store = Arc::new(orbisync_storage_postgres::IdentityAdministrationStore::new(
        pool.clone(),
    ));
    let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> = admin_store;
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let login_store: Arc<dyn orbisync_application::LoginTransactionStore> =
        Arc::new(PgLoginStore::new(pool.clone()));
    let login_service = Arc::new(LoginService::new(
        repo.clone(),
        login_store,
        (*passwords).clone(),
        tokens.clone(),
        clock.clone() as Arc<dyn orbisync_domain::Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ));
    let mut state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(tokens.clone())
    .with_identity_repository(repo)
    .with_password_service(passwords.clone())
    .with_identity_admin_service(admin.clone())
    .with_login_service(login_service)
    .with_refresh_creation_store(Arc::new(
        orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone()),
    )
        as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
    .with_refresh_rotation_store(Arc::new(
        orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone()),
    )
        as Arc<dyn orbisync_application::RefreshTokenRotationStore>);
    let qstore = Arc::new(PgIdentityQueryStore::with_codec(
        pool.clone(),
        orbisync_testkit::identity::insecure_test_codec(),
    ));
    let iq: Arc<dyn IdentityQueryPort> = qstore.clone();
    let aq: Arc<dyn AuditQueryPort> = qstore;
    state = state.with_identity_query(iq).with_audit_query(aq);
    (state, admin, pool)
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

async fn http_create_role(
    state: HttpState,
    bearer: &str,
    name: &str,
    perms: &[&str],
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "name": name, "permissions": perms });
    let req = Request::builder()
        .uri("/v1/roles")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_delete_role(
    state: HttpState,
    bearer: Option<&str>,
    role_id: &str,
    if_match: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let uri = format!("/v1/roles/{role_id}");
    let mut builder = Request::builder().uri(uri).method("DELETE");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(im) = if_match {
        builder = builder.header("if-match", im);
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

async fn http_get_role(
    state: HttpState,
    bearer: &str,
    role_id: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let req = Request::builder()
        .uri(format!("/v1/roles/{role_id}"))
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

async fn bootstrap_and_login(pool: sqlx::PgPool) -> (HttpState, String, sqlx::PgPool) {
    let (state, admin, pool) = setup_state(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let login = format!("adm-{suffix}");
    let (_, admin_pw) = admin
        .bootstrap_administrator(
            LoginId::new(login.clone()).expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request"),
        )
        .await
        .expect("bootstrap");
    let (status, json) = http_login(state.clone(), &login, admin_pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "admin login {json}");
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    (state, token, pool)
}

async fn create_role_and_get_revision(state: HttpState, token: &str) -> (String, u64) {
    let name = format!("role-{}", Uuid::now_v7());
    let (status, json) = http_create_role(state, token, &name, &["admin.users.read"]).await;
    assert_eq!(status, StatusCode::CREATED, "role create {json}");
    let id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    let rev = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    assert!(rev >= 1);
    (id, rev)
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    sid: String,
    jti: String,
    iss: String,
    aud: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}
fn make_broken_sid_token(sub: String, sid: String) -> String {
    let header = {
        let mut h = Header::new(Algorithm::EdDSA);
        h.kid = Some("test-key-1".to_owned());
        h
    };
    let claims = Claims {
        sub,
        sid,
        jti: Uuid::now_v7().to_string(),
        iss: "orbisync".to_owned(),
        aud: "orbisync-api".to_owned(),
        iat: 1_700_000_000,
        nbf: 1_700_000_000,
        exp: 1_700_000_000 + 900,
    };
    let key = EncodingKey::from_ed_pem(PRIVATE_PEM).expect("key");
    jsonwebtoken::encode(&header, &claims, &key).expect("encode")
}

// Test 1: correct If-Match => 204 and actually deleted
#[tokio::test]
async fn delete_role_correct_if_match_204_and_gone() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token, pool) = bootstrap_and_login(pool).await;
    let (role_id, rev) = create_role_and_get_revision(state.clone(), &token).await;
    let if_match = format!("\"{rev}\"");
    let (status, json) =
        http_delete_role(state.clone(), Some(&token), &role_id, Some(&if_match)).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "delete must be 204 got {json} for If-Match {if_match}"
    );
    // Actually gone: GET => 404
    let (status2, json2) = http_get_role(state.clone(), &token, &role_id).await;
    assert_eq!(
        status2,
        StatusCode::NOT_FOUND,
        "after delete GET must be 404 got {json2}"
    );
    // Audit check: search via SQL
    let cnt: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'role.deleted' AND target_id = $1 AND result = 'success'")
        .bind(&role_id)
        .fetch_one(&pool)
        .await
        .expect("audit count");
    assert!(
        cnt >= 1,
        "audit event for role.deleted success must exist, got {cnt}"
    );
}

// Test 2: If-Match missing => 400
#[tokio::test]
async fn delete_role_missing_if_match_400() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token, _pool) = bootstrap_and_login(pool).await;
    let (role_id, _rev) = create_role_and_get_revision(state.clone(), &token).await;
    let (status, json) = http_delete_role(state.clone(), Some(&token), &role_id, None).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing If-Match must be 400 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "INVALID_REQUEST");
}

// Test 3: If-Match invalid format (bare number) => 400
#[tokio::test]
async fn delete_role_invalid_if_match_400() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token, _pool) = bootstrap_and_login(pool).await;
    let (role_id, _rev) = create_role_and_get_revision(state.clone(), &token).await;
    let (status, json) = http_delete_role(state.clone(), Some(&token), &role_id, Some("19")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "bare 19 must be 400 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "INVALID_REQUEST");
    // also test with missing quotes but with spaces?
    let (status2, json2) =
        http_delete_role(state.clone(), Some(&token), &role_id, Some("\"0\"")).await;
    assert_eq!(
        status2,
        StatusCode::BAD_REQUEST,
        "0 revision must be 400 got {json2}"
    );
}

// Test 4: stale revision => 409
#[tokio::test]
async fn delete_role_stale_revision_409() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token, _pool) = bootstrap_and_login(pool).await;
    let (role_id, rev) = create_role_and_get_revision(state.clone(), &token).await;
    let stale = rev + 99;
    let if_match = format!("\"{stale}\"");
    let (status, json) =
        http_delete_role(state.clone(), Some(&token), &role_id, Some(&if_match)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "stale revision must be 409 got {json} with If-Match {if_match} vs actual {rev}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "RESOURCE_CONFLICT");
    // Also try old revision 1 when rev is 1? That's not mismatch. So test with clearly mismatched.
    // Ensure role still exists (not deleted on conflict)
    let (status2, json2) = http_get_role(state.clone(), &token, &role_id).await;
    assert_eq!(
        status2,
        StatusCode::OK,
        "role should still exist after 409, got {json2}"
    );
}

// Test 5: not found => 404
#[tokio::test]
async fn delete_role_not_found_404() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token, _pool) = bootstrap_and_login(pool).await;
    let fake = Uuid::now_v7().to_string();
    let (status, json) = http_delete_role(state.clone(), Some(&token), &fake, Some("\"1\"")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-existent role must be 404 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "RESOURCE_NOT_FOUND");
}

// Test 6: permission missing => 403
#[tokio::test]
async fn delete_role_forbidden_403() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, admin_token, _pool) = bootstrap_and_login(pool).await;
    // Create a role to delete
    let (role_id, rev) = create_role_and_get_revision(state.clone(), &admin_token).await;
    // Create bob without admin.roles.delete
    let bob_login = format!("bob-{}", Uuid::now_v7());
    // create bob via HTTP with credential accept
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": bob_login, "display_name": "Bob" });
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
    let (status, json) = http_login(state.clone(), &bob_login, &bob_pw).await;
    assert_eq!(status, StatusCode::OK, "bob login {json}");
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // Try delete with bob token (no perms)
    let if_match = format!("\"{rev}\"");
    let (status, json) =
        http_delete_role(state.clone(), Some(&bob_token), &role_id, Some(&if_match)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "missing perm must be 403 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");
    // Ensure role still exists
    let (status2, _) = http_get_role(state.clone(), &admin_token, &role_id).await;
    assert_eq!(status2, StatusCode::OK);
}

// Test 7: broken sid => 401
#[tokio::test]
async fn delete_role_broken_sid_401() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, admin_token, pool) = bootstrap_and_login(pool).await;
    // Get admin user id via repo? Use admin token's sub? We have login, need to fetch user id via DB
    // Instead create a role first to get its id and revision, then craft broken token with real sub but broken sid
    let (role_id, rev) = create_role_and_get_revision(state.clone(), &admin_token).await;
    // Get admin user id from DB
    let admin_id: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("admin id");
    let broken = make_broken_sid_token(admin_id.to_string(), "not-a-uuid".to_owned());
    let if_match = format!("\"{rev}\"");
    let (status, json) =
        http_delete_role(state.clone(), Some(&broken), &role_id, Some(&if_match)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "broken sid must be 401 got {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "AUTHENTICATION_REQUIRED");
}

// Test 8: audit after success (already in test 1, but explicit)
#[tokio::test]
async fn delete_role_audit_exists_after_success() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token, pool) = bootstrap_and_login(pool).await;
    let (role_id, rev) = create_role_and_get_revision(state.clone(), &token).await;
    let if_match = format!("\"{rev}\"");
    let (status, _) =
        http_delete_role(state.clone(), Some(&token), &role_id, Some(&if_match)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Check audit via query port as well
    let qstore = PgIdentityQueryStore::with_codec(
        pool.clone(),
        orbisync_testkit::identity::insecure_test_codec(),
    );
    // Search audit: use filter with action
    let filter = orbisync_application::AuditFilter {
        from: None,
        to: None,
        actor_id: None,
        action: Some("role.deleted".to_owned()),
    };
    let page = qstore
        .search(
            filter,
            orbisync_application::PageRequest {
                limit: 50,
                after: None,
            },
        )
        .await
        .expect("search");
    let found = page
        .items
        .iter()
        .any(|a| a.resource_id.as_deref() == Some(&role_id) && a.result == "success");
    assert!(
        found,
        "audit event for role.deleted success with target {role_id} must exist, got {:?}",
        page.items
    );
    // Also direct SQL
    let cnt: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='role.deleted' AND target_id=$1 AND result='success'")
        .bind(&role_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(cnt >= 1, "direct SQL audit must exist");
}
