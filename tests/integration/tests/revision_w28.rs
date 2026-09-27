//! W-28 revision honesty: real PostgreSQL + real HTTP.
//! Verifies CR-03: POST /v1/roles and POST /v1/users return revision >=1,
//! persisted value matches response, and parse_if_match accepts it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use tokio::sync::Mutex;

// W-28: 3 tests share a single real PostgreSQL instance via DATABASE_URL.
// BootstrapAdministrator requires `SELECT count(*) FROM users == 0` (InvalidRevision guard).
// Without serialization, concurrent TRUNCATE + bootstrap race on count(*) and
// two tests see count=1 -> InvalidRevision -> PortFailure. This is a test
// isolation issue (A), not production code, so serialize via a process-wide mutex.
// Assertions are not relaxed; all 3 tests still verify revision >=1, DB==response, parse_if_match.
fn db_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::IdentityQueryPort;
use orbisync_domain::{LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
    PasswordService, token::AccessTokenService,
};
use orbisync_storage_postgres::{PgIdentityQueryStore, PgIdentityRepository};

mod common;
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, parse_if_match, router};
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

async fn database_pool() -> Option<sqlx::PgPool> {
    common::pool_or_skip().await
}

async fn clean_identity(pool: &sqlx::PgPool) {
    // Bootstrap requires empty users table (count==0). Truncate all identity-related tables.
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
        // these tables may be empty, ignore
        eprintln!("clean world truncate warning: {e}");
    }
    // Verify clean succeeded 窶・help debug flaky bootstrap
    let cnt: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("count users");
    if cnt != 0 {
        eprintln!("clean_identity: users count after truncate = {cnt}, will panic");
        // try again once
        // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = sqlx::query("TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, auth_sessions, refresh_tokens, audit_events, idempotency_records CASCADE").execute(pool).await;
        let cnt2: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .expect("count2");
        eprintln!("after retry count={cnt2}");
        assert_eq!(cnt2, 0, "users must be 0 after clean, got {cnt2}");
    }
}

fn test_login_service_pg(
    pool: sqlx::PgPool,
    repo: Arc<dyn orbisync_application::IdentityRepository>,
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
    let state = HttpState::new(
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
    .with_login_service(test_login_service_pg(
        pool.clone(),
        repo.clone(),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_identity_repository(repo)
    .with_password_service(passwords.clone())
    .with_identity_admin_service(admin.clone())
    .with_refresh_creation_store(Arc::new(
        orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone()),
    )
        as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
    .with_refresh_rotation_store(Arc::new(
        orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone()),
    )
        as Arc<dyn orbisync_application::RefreshTokenRotationStore>);
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

async fn http_create_user(
    state: HttpState,
    bearer: &str,
    login_id: &str,
    display_name: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "login_id": login_id, "display_name": display_name });
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
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn w28_role_revision_is_honest_and_parseable() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, admin, pool2) = setup_state(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let login = format!("adm-{suffix}");
    let (_, admin_pw) = admin
        .bootstrap_administrator(
            LoginId::new(login.clone()).expect("login"),
            "Admin".to_owned(),
            orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                .expect("request"),
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

    let role_name = format!("R-{}", Uuid::now_v7());
    let (status, json) =
        http_create_role(state.clone(), &token, &role_name, &["admin.users.read"]).await;
    assert_eq!(status, StatusCode::CREATED, "role create {json}");

    // 1 & 4: revision >=1 and schema fields present
    let rev = json
        .get("revision")
        .and_then(|v| v.as_u64())
        .expect("revision u64");
    assert!(
        rev >= 1,
        "Role revision must be >=1 per openapi minimum:1, got {rev}"
    );
    // schema required fields
    assert!(
        json.get("id").and_then(|v| v.as_str()).is_some(),
        "Role id required"
    );
    assert!(json.get("name").and_then(|v| v.as_str()).is_some());
    assert!(json.get("permissions").and_then(|v| v.as_array()).is_some());

    // 3: parse_if_match must accept server-issued revision
    let header_value = format!("\"{rev}\"");
    let parsed = parse_if_match(&header_value).expect("parse_if_match must accept server revision");
    assert_eq!(parsed, rev, "parsed revision must equal delivered");

    // 1: DB matches response
    let role_id_str = json.get("id").and_then(|v| v.as_str()).unwrap();
    let role_id = Uuid::parse_str(role_id_str).unwrap();
    let db_rev: i64 = sqlx::query_scalar("SELECT revision FROM roles WHERE id = $1")
        .bind(role_id)
        .fetch_one(&pool2)
        .await
        .expect("role row exists");
    assert_eq!(
        u64::try_from(db_rev).unwrap(),
        rev,
        "DB revision must equal response revision"
    );

    // Also verify via query store RoleView
    let qstore = PgIdentityQueryStore::with_codec(
        pool2.clone(),
        orbisync_testkit::identity::insecure_test_codec(),
    );
    let view = qstore
        .role(orbisync_domain::RoleId::new(role_id).unwrap())
        .await
        .unwrap()
        .expect("role view");
    assert_eq!(view.revision, rev, "RoleView revision must equal response");
    // parse_if_match via view
    let hv2 = format!("\"{}\"", view.revision);
    parse_if_match(&hv2).expect("RoleView revision must be parseable");
}

#[tokio::test]
async fn w28_user_revision_is_honest_and_parseable() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let cnt_before: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("count before");
    eprintln!("w28_user before bootstrap count={cnt_before}");
    let (state, admin, pool2) = setup_state(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let login = format!("adm2-{suffix}");
    let bootstrap_res = admin
        .bootstrap_administrator(
            LoginId::new(login.clone()).expect("login"),
            "Admin".to_owned(),
            orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                .expect("request"),
        )
        .await;
    eprintln!("bootstrap_res={:?}", bootstrap_res);
    let (_, admin_pw) = bootstrap_res.expect("bootstrap");

    let (status, json) = http_login(state.clone(), &login, admin_pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK, "admin login {json}");
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let user_login = format!("u-{}", Uuid::now_v7());
    let (status, json) = http_create_user(state.clone(), &token, &user_login, "UserOne").await;
    assert_eq!(status, StatusCode::CREATED, "user create {json}");

    let rev = json
        .get("revision")
        .and_then(|v| v.as_u64())
        .expect("revision u64");
    assert!(rev >= 1, "User revision must be >=1, got {rev}");
    assert!(json.get("id").and_then(|v| v.as_str()).is_some());
    assert!(json.get("login_id").and_then(|v| v.as_str()).is_some());
    assert!(json.get("enabled").and_then(|v| v.as_bool()).is_some());

    let header_value = format!("\"{rev}\"");
    let parsed = parse_if_match(&header_value).expect("parse_if_match must accept user revision");
    assert_eq!(parsed, rev);

    let user_id_str = json.get("id").and_then(|v| v.as_str()).unwrap();
    let user_id = Uuid::parse_str(user_id_str).unwrap();
    let db_rev: i64 = sqlx::query_scalar("SELECT revision FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&pool2)
        .await
        .expect("user row");
    assert_eq!(
        u64::try_from(db_rev).unwrap(),
        rev,
        "DB user revision must equal response"
    );

    // query store check
    let qstore = PgIdentityQueryStore::with_codec(
        pool2.clone(),
        orbisync_testkit::identity::insecure_test_codec(),
    );
    let view = qstore
        .user(orbisync_domain::UserId::new(user_id).unwrap())
        .await
        .unwrap()
        .expect("user view");
    assert_eq!(view.revision, rev);
    let hv2 = format!("\"{}\"", view.revision);
    parse_if_match(&hv2).expect("UserView revision parseable");
}

#[tokio::test]
async fn w28_bootstrap_role_backfill_is_1() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, admin, pool2) = setup_state(pool).await;
    let login = format!("adm3-{}", Uuid::now_v7());
    let (_, admin_pw) = admin
        .bootstrap_administrator(
            LoginId::new(login.clone()).expect("login"),
            "Admin".to_owned(),
            orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                .expect("request"),
        )
        .await
        .expect("bootstrap");
    let (status, json) = http_login(state.clone(), &login, admin_pw.expose_secret()).await;
    assert_eq!(status, StatusCode::OK);
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // bootstrap created a role implicitly; query it via DB
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM roles WHERE revision < 1")
        .fetch_one(&pool2)
        .await
        .unwrap();
    assert_eq!(count, 0, "no roles should have revision <1 after backfill");
    // also ensure that newly created roles via HTTP are 1
    let (status, json) = http_create_role(
        state.clone(),
        &token,
        &format!("R2-{}", Uuid::now_v7()),
        &["admin.roles.read"],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let rev = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    assert_eq!(rev, 1, "initial role revision must be 1");
}
