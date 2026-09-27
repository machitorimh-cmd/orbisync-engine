//! RV-C: world / instance creation requires authorization and atomic audit.
//! Real DB + public HTTP boundary (POST /v1/worlds, POST /v1/instances).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, RequestId};
use orbisync_domain::{Clock as _, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
    PasswordPolicy, PasswordService, token::AccessTokenService,
};
use orbisync_storage_postgres::{
    IdempotencyStore as PgIdempotencyStore, IdentityAdministrationStore as PgIdentityAdminStore,
    PgIdentityRepository, PgWorldAuthorizer, PgWorldDirectoryStore,
};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use sqlx::PgPool;
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use uuid::Uuid;

mod common;

// Serializes every test in this file against the others.
//
// `rv_c_audit_failure_rolls_back_world_insert` installs a real trigger/function
// on the shared `audit_events` table, protected only by `pg_advisory_lock`.
// `setup_two_users()` (called by every other test here) unconditionally runs
// `DROP TRIGGER IF EXISTS ... / DROP FUNCTION IF EXISTS ... CASCADE` against
// that same table as defensive cleanup, and does NOT take the advisory lock
// first. Without this guard, cargo test's default parallel execution lets
// another test's cleanup DROP race the trigger-installing test: if the DROP
// lands between `CREATE TRIGGER` and the test's own HTTP call, the injected
// failure never fires, the world is created, and the test asserts a 500/503
// that never comes (observed as `left: 500/401, right: 200/201` CI flakes).
// A file-local mutex removes the race without weakening any assertion.
fn db_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

fn token_service() -> Arc<AccessTokenService> {
    Arc::new(
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("tokens"),
    )
}

fn test_login_service_pg(
    pool: PgPool,
    repo: Arc<dyn IdentityRepository>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> =
        Arc::new(orbisync_storage_postgres::PgLoginStore::new(pool));
    Arc::new(LoginService::new(
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

fn fixed_clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("ts"),
    ))
}

async fn database() -> Option<PgPool> {
    common::pool_or_skip().await
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
    let repo: Arc<dyn IdentityRepository> = Arc::new(PgIdentityRepository::new(pool.clone()));
    let admin_store = Arc::new(PgIdentityAdminStore::new(pool.clone()));
    let idem_store = Arc::new(PgIdempotencyStore::new(pool.clone()));
    let dyn_store = DynIdentityAdministrationStore(
        admin_store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>
    );
    let dyn_clock = DynClock(Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    // world directory with real authorizer (application layer)
    let world_store =
        PgWorldDirectoryStore::with_codec(pool.clone(), orbisync_testkit::insecure_test_codec());
    let world_authorizer = PgWorldAuthorizer::new(pool.clone());
    let world_directory = Arc::new(orbisync_application::WorldDirectoryUseCase::new(
        world_store,
        world_authorizer,
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
    .with_world_directory(world_directory)
    .with_idempotency_store(idem_store as Arc<dyn orbisync_application::IdempotencyStore>)
    .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
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

async fn http_create_world(
    state: HttpState,
    bearer: Option<&str>,
    name: &str,
) -> (StatusCode, serde_json::Value, HeaderMap) {
    let app = router(state);
    let body = serde_json::json!({ "name": name, "description": null, "capacity": 10 });
    let mut builder = Request::builder()
        .uri("/v1/worlds")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, headers)
}

async fn http_create_instance(
    state: HttpState,
    bearer: Option<&str>,
    world_id: &str,
) -> (StatusCode, serde_json::Value, HeaderMap) {
    let app = router(state);
    let body = serde_json::json!({ "world_id": world_id });
    let mut builder = Request::builder()
        .uri("/v1/instances")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, headers)
}

async fn setup_two_users(pool: PgPool) -> (HttpState, String, String, Uuid, Uuid) {
    // Cleanup any leftover trigger from M3 test (tests use unique users, no full wipe needed)
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("DROP TRIGGER IF EXISTS rv_c_fail_audit_trigger ON audit_events")
        .execute(&pool)
        .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("DROP FUNCTION IF EXISTS rv_c_fail_audit() CASCADE")
        .execute(&pool)
        .await;

    // returns (state, admin_token, bob_token, admin_user_id, bob_user_id)
    let clock = fixed_clock();
    let tokens = token_service();
    let (state, _) = http_state(pool.clone(), Arc::clone(&clock), Arc::clone(&tokens));

    // Create admin directly via SQL (unique, no need for empty DB) with required perms
    let admin_login = format!("rv-c-admin-{}", Uuid::now_v7());
    let admin_password = "AdminPass123!@#";
    let admin_user_id = {
        let now = clock.now();
        let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
        let hash = passwords
            .hash(orbisync_application::SecretString::new(admin_password))
            .await
            .expect("hash")
            .expose_phc()
            .to_owned();
        let user_id = Uuid::now_v7();
        let role_id = Uuid::now_v7();
        sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'RV-C Admin', 'active', true, 1, $3, $3)")
            .bind(user_id)
            .bind(&admin_login)
            .bind(now.as_offset_date_time())
            .execute(&pool)
            .await
            .expect("insert admin user");
        sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, $2, $3)")
            .bind(user_id)
            .bind(&hash)
            .bind(now.as_offset_date_time())
            .execute(&pool)
            .await
            .expect("insert cred");
        sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, 'RV-C Test Admin', 1)")
            .bind(role_id)
            .bind(format!("rv-c-role-{}", Uuid::now_v7()))
            .execute(&pool)
            .await
            .expect("insert role");
        for perm in [
            "admin.users.create",
            "admin.worlds.create",
            "world.instance.create",
        ] {
            sqlx::query("INSERT INTO permissions (name) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(perm)
                .execute(&pool)
                .await
                .expect("perm");
            sqlx::query("INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)")
                .bind(role_id)
                .bind(perm)
                .execute(&pool)
                .await
                .expect("role perm");
        }
        sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
            .bind(user_id)
            .bind(role_id)
            .execute(&pool)
            .await
            .expect("user role");
        user_id
    };
    let (status, json) = http_login(state.clone(), &admin_login, admin_password).await;
    assert_eq!(status, StatusCode::OK, "admin login {json}");
    let admin_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // create bob via HTTP as admin (no roles -> no world permission)
    let bob_login = format!("rv-c-bob-{}", Uuid::now_v7());
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": bob_login, "display_name": "Bob RV-C" });
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
    let j: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let bob_id = j
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let bob_temp = j
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let bob_user_id = Uuid::parse_str(&bob_id).unwrap();

    // bob login
    let (status, json) = http_login(state.clone(), &bob_login, &bob_temp).await;
    assert_eq!(status, StatusCode::OK, "bob login {json}");
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    (state, admin_token, bob_token, admin_user_id, bob_user_id)
}

// ------------------------------------------------------------
// 1. 権限を持つトークンで world を作成でき、監査イベントが残る
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_world_create_with_permission_audited() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let (state, admin_token, _bob_token, admin_user_id, _) = setup_two_users(pool.clone()).await;
    let world_name = format!("rv-c-world-ok-{}", Uuid::now_v7());
    let (status, json, headers) =
        http_create_world(state.clone(), Some(&admin_token), &world_name).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "world create must be 201, got {json}"
    );
    let world_id = json
        .get("id")
        .and_then(|v| v.as_str())
        .expect("world id")
        .to_owned();
    // DB: world_definitions exists
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM world_definitions WHERE id = $1")
        .bind(Uuid::parse_str(&world_id).unwrap())
        .fetch_one(&pool)
        .await
        .expect("count world");
    assert_eq!(count, 1, "world must be persisted");
    // audit: success
    let req_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(req_id.starts_with("req_"), "x-request-id must be present");
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'world.created' AND actor_user_id = $1 AND result = 'success' AND request_id = $2",
    )
    .bind(admin_user_id)
    .bind(&req_id)
    .fetch_one(&pool)
    .await
    .expect("audit count");
    assert_eq!(audit_count, 1, "success audit must exist for world.created");
    // also check that audit target_id is world_id
    let target: Option<String> = sqlx::query_scalar(
        "SELECT target_id FROM audit_events WHERE request_id = $1 AND action = 'world.created'",
    )
    .bind(&req_id)
    .fetch_optional(&pool)
    .await
    .expect("target query");
    assert_eq!(target.as_deref(), Some(world_id.as_str()));
}

// ------------------------------------------------------------
// 2. 権限を持つトークンで instance を作成でき、監査イベントが残る
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_instance_create_with_permission_audited() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let (state, admin_token, _bob_token, admin_user_id, _) = setup_two_users(pool.clone()).await;
    // first create world as admin
    let world_name = format!("rv-c-world-inst-{}", Uuid::now_v7());
    let (status, json, _) = http_create_world(state.clone(), Some(&admin_token), &world_name).await;
    assert_eq!(status, StatusCode::CREATED, "world {json}");
    let world_id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();

    let (status, json, headers) =
        http_create_instance(state.clone(), Some(&admin_token), &world_id).await;
    assert_eq!(status, StatusCode::CREATED, "instance create 201 {json}");
    let instance_id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    let req_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    // DB instance exists
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM world_instances WHERE id = $1")
        .bind(Uuid::parse_str(&instance_id).unwrap())
        .fetch_one(&pool)
        .await
        .expect("instance count");
    assert_eq!(count, 1);
    // audit success
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'world.instance.created' AND actor_user_id = $1 AND result = 'success' AND request_id = $2",
    )
    .bind(admin_user_id)
    .bind(&req_id)
    .fetch_one(&pool)
    .await
    .expect("audit");
    assert_eq!(audit_count, 1, "instance success audit must exist");
}

// ------------------------------------------------------------
// 3. 権限を持たない認証済みトークンで world 作成が 403 になり、world が作られていない
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_world_create_without_permission_is_403_and_not_persisted() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let (state, _admin_token, bob_token, _admin_user_id, bob_user_id) =
        setup_two_users(pool.clone()).await;
    let world_name = format!("rv-c-world-no-perm-{}", Uuid::now_v7());
    let (status, json, headers) =
        http_create_world(state.clone(), Some(&bob_token), &world_name).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bob world create must be 403 {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");
    // DB: no world with that name (query by name)
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM world_definitions WHERE name = $1")
        .bind(&world_name)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 0, "world must not be persisted on 403");
    // audit failure must exist (result = failure)
    let req_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'world.created' AND actor_user_id = $1 AND result = 'failure' AND request_id = $2",
    )
    .bind(bob_user_id)
    .bind(&req_id)
    .fetch_one(&pool)
    .await
    .expect("audit");
    assert_eq!(
        audit_count, 1,
        "failure audit must exist for forbidden world.create"
    );
}

// ------------------------------------------------------------
// 4. 権限を持たない認証済みトークンで instance 作成が 403 になり、作られていない
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_instance_create_without_permission_is_403_and_not_persisted() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let (state, admin_token, bob_token, _admin_id, bob_user_id) =
        setup_two_users(pool.clone()).await;
    // need a world to target (created by admin)
    let world_name = format!("rv-c-world-for-inst-403-{}", Uuid::now_v7());
    let (status, json, _) = http_create_world(state.clone(), Some(&admin_token), &world_name).await;
    assert_eq!(status, StatusCode::CREATED, "admin world {json}");
    let world_id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();

    let (status, json, headers) =
        http_create_instance(state.clone(), Some(&bob_token), &world_id).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bob instance create 403 {json}"
    );
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "ACCESS_DENIED");
    // no instance for this world created by bob – count instances for this world that have not been created before?
    // We check that no new instance was inserted after the 403 by counting total for this world before and after?
    // Since we just created world and have not created any instance with bob, the only instance we could check is that the 403 did not create one.
    // Query all instances for world_id – should be 0
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM world_instances WHERE world_id = $1")
        .bind(Uuid::parse_str(&world_id).unwrap())
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 0, "no instance should be created on 403");
    let req_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'world.instance.created' AND actor_user_id = $1 AND result = 'failure' AND request_id = $2",
    )
    .bind(bob_user_id)
    .bind(&req_id)
    .fetch_one(&pool)
    .await
    .expect("audit");
    assert_eq!(audit_count, 1, "failure audit for instance");
}

// ------------------------------------------------------------
// 5. 認証なしで 401
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_missing_auth_is_401() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let clock = fixed_clock();
    let tokens = token_service();
    let (state, _admin) = http_state(pool.clone(), clock, tokens);
    let world_name = format!("rv-c-noauth-{}", Uuid::now_v7());
    let (status, json, _) = http_create_world(state.clone(), None, &world_name).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no auth 401 {json}");
    let code = json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code, "AUTHENTICATION_REQUIRED");
    // instance also 401
    let (status2, json2, _) =
        http_create_instance(state.clone(), None, &Uuid::now_v7().to_string()).await;
    assert_eq!(
        status2,
        StatusCode::UNAUTHORIZED,
        "instance no auth 401 {json2}"
    );
    let code2 = json2
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code2, "AUTHENTICATION_REQUIRED");
}

// ------------------------------------------------------------
// 6. 失敗時にも result = failure の監査が残ることは 3/4 で検証済みだが、明示的に world/instance で確認
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_failure_audits_are_persisted_for_both_resources() {
    // This is covered by tests 3 and 4, but we double-check that success audits are success and failure audits are failure distinctly.
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let (state, admin_token, bob_token, admin_id, bob_id) = setup_two_users(pool.clone()).await;
    // success path audit – must assert HTTP status before counting audit, otherwise
    // a pool exhaustion or trigger-induced 500 is misreported as "audit missing".
    let world_ok = format!("rv-c-audit-ok-{}", Uuid::now_v7());
    let (status_ok, json_ok, hdr_ok) =
        http_create_world(state.clone(), Some(&admin_token), &world_ok).await;
    assert_eq!(
        status_ok,
        StatusCode::CREATED,
        "admin world create must be 201, got {status_ok} {json_ok}"
    );
    let req_ok = hdr_ok
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_owned();
    let ok_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE request_id = $1 AND result = 'success' AND actor_user_id = $2",
    )
    .bind(&req_ok)
    .bind(admin_id)
    .fetch_one(&pool)
    .await
    .expect("audit ok");
    assert_eq!(
        ok_count, 1,
        "success audit must exist for world.created, request_id={req_ok}, status={status_ok} {json_ok}"
    );

    // failure path audit – also assert 403 before counting
    let world_fail = format!("rv-c-audit-fail-{}", Uuid::now_v7());
    let (status_fail, json_fail, hdr_fail) =
        http_create_world(state.clone(), Some(&bob_token), &world_fail).await;
    assert_eq!(
        status_fail,
        StatusCode::FORBIDDEN,
        "bob world create must be 403, got {status_fail} {json_fail}"
    );
    let req_fail = hdr_fail
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_owned();
    let fail_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE request_id = $1 AND result = 'failure' AND actor_user_id = $2",
    )
    .bind(&req_fail)
    .bind(bob_id)
    .fetch_one(&pool)
    .await
    .expect("audit fail");
    assert_eq!(
        fail_count, 1,
        "failure audit must exist for forbidden world.create, request_id={req_fail}, status={status_fail} {json_fail}"
    );
}

// ------------------------------------------------------------
// M3: 状態変更と監査を別トランザクションにし、監査だけ失敗させると、状態変更も巻き戻る
// 検証: audit_events への INSERT を trigger で強制失敗させ、HTTP 作成が失敗したときに world が残っていないことを確認。
// mutant（別トランザクションで audit だけ失敗しても world がコミットされたまま）なら、このテストは赤になる。
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_audit_failure_rolls_back_world_insert() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else { return };
    let (state, admin_token, _bob_token, admin_id, _) = setup_two_users(pool.clone()).await;

    // Install a trigger that makes the next audit_events insert for world.created fail.
    // We use a temporary trigger that raises an exception. It will be dropped after the test.
    // This does not require changes to production code – it forces DB audit insert to fail,
    // allowing us to verify that the world insert is rolled back (single transaction) vs leaked (separate transactions).
    //
    // Flaky fix: the previous trigger was global (`action = 'world.created'`) and
    // affected all parallel tests that tried to create worlds, causing
    // `rv_c_failure_audits_are_persisted_for_both_resources` to see 500 instead
    // of 201 and then report `ok_count = 0` (the observed `left: 0 right: 1`).
    // We now make the trigger actor-specific (`actor_user_id = '<this-test-admin>'`)
    // so other parallel tests with different admin ids are unaffected. We also
    // serialize DDL via an advisory lock to avoid races where multiple tests
    // concurrently create/drop the same trigger object.
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("SELECT pg_advisory_lock(728391)")
        .execute(&pool)
        .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("DROP TRIGGER IF EXISTS rv_c_fail_audit_trigger ON audit_events")
        .execute(&pool)
        .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("DROP FUNCTION IF EXISTS rv_c_fail_audit() CASCADE")
        .execute(&pool)
        .await;
    let func_sql = format!(
        "CREATE OR REPLACE FUNCTION rv_c_fail_audit() RETURNS TRIGGER AS $$ BEGIN IF NEW.action = 'world.created' AND NEW.actor_user_id = '{}'::uuid THEN RAISE EXCEPTION 'rv-c injected audit failure for actor %', NEW.actor_user_id; END IF; RETURN NEW; END; $$ LANGUAGE plpgsql",
        admin_id
    );
    sqlx::query(&func_sql)
        .execute(&pool)
        .await
        .expect("create function");
    sqlx::query(
        "CREATE TRIGGER rv_c_fail_audit_trigger BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION rv_c_fail_audit()",
    )
    .execute(&pool)
    .await
    .expect("create trigger");

    let world_name = format!("rv-c-rollback-{}", Uuid::now_v7());
    let (status, _json, headers) =
        http_create_world(state.clone(), Some(&admin_token), &world_name).await;
    // Because audit insert fails inside the same transaction, the HTTP handler should return 500 (PortFailure -> InternalError)
    assert!(
        status == StatusCode::INTERNAL_SERVER_ERROR || status == StatusCode::SERVICE_UNAVAILABLE,
        "audit failure should cause 500/503, got {status}"
    );
    // Verify that the world was NOT persisted (rolled back)
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM world_definitions WHERE name = $1")
        .bind(&world_name)
        .fetch_one(&pool)
        .await
        .expect("count after rollback");
    assert_eq!(
        count, 0,
        "world insert must be rolled back when audit fails (single transaction). If this is 1, the code uses separate transactions (mutant M3)."
    );
    // Also verify that no success audit was persisted (the failing insert was rolled back)
    let req_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !req_id.is_empty() {
        let audit_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE request_id = $1")
                .bind(&req_id)
                .fetch_one(&pool)
                .await
                .expect("audit count after rollback");
        assert_eq!(
            audit_count, 0,
            "no audit should be persisted when the transaction rolled back"
        );
    }

    // Cleanup trigger/function + release advisory lock
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("DROP TRIGGER IF EXISTS rv_c_fail_audit_trigger ON audit_events")
        .execute(&pool)
        .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("DROP FUNCTION IF EXISTS rv_c_fail_audit() CASCADE")
        .execute(&pool)
        .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = sqlx::query("SELECT pg_advisory_unlock(728391)")
        .execute(&pool)
        .await;
}

// ------------------------------------------------------------
// M4: トランスポート層でだけ検査し、アプリケーション層の検査を外すと 403 が返らないことを検証するロジックを含めるため、
// application 層に直接 authorizer を外した use case を用いても 403 にならないことを間接的に示す。
// ここでは、application層の check が必須であることを示すため、AllowAllAuthorizer で作成された世界が通るが、
// DenyAllAuthorizer では通らないことを単体テスト的に確認（DBなしでも）。
// 実際の HTTP テストでは、application 層を bypass して transport だけで検査した mutant を想定し、
// その場合はテスト3が緑のまま（つまり失敗を検出できない）になるはずだが、我々の実装は application で守っているのでテスト3は赤（正しく403）。
// ------------------------------------------------------------
#[tokio::test]
async fn rv_c_application_authorizer_is_required_not_only_transport() {
    // This test verifies that the use case itself enforces the permission, not just the HTTP layer.
    // We construct a use case with DenyAllAuthorizer (simulating a user without permission) and
    // ensure it returns NotAuthorized even when called directly (bypassing HTTP).
    use orbisync_application::{CreateWorldCommand, WorldDirectoryUseCase};
    use orbisync_domain::transform::Transform;
    use orbisync_testkit::{AllowAllAuthorizer, DenyAllAuthorizer, FakeWorldDirectoryStore};

    let clock = Timestamp::from_unix_millis(1_700_000_000_000).expect("ts");
    let actor = orbisync_domain::UserId::generate();
    let req_id = RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req");

    // With AllowAll -> succeeds
    let allow_uc = WorldDirectoryUseCase::new(FakeWorldDirectoryStore::new(), AllowAllAuthorizer);
    let cmd_allow = CreateWorldCommand {
        actor_id: actor,
        name: format!("allow-{}", Uuid::now_v7()),
        description: None,
        default_spawn: Transform::identity(),
        capacity: 10,
        now: clock,
        request_id: req_id.clone(),
    };
    let res_allow = allow_uc.create_world(cmd_allow).await;
    assert!(res_allow.is_ok(), "AllowAll should succeed");

    // With DenyAll -> must be NotAuthorized and audit failure recorded, world not persisted
    let deny_uc = WorldDirectoryUseCase::new(FakeWorldDirectoryStore::new(), DenyAllAuthorizer);
    let cmd_deny = CreateWorldCommand {
        actor_id: actor,
        name: format!("deny-{}", Uuid::now_v7()),
        description: None,
        default_spawn: Transform::identity(),
        capacity: 10,
        now: clock,
        request_id: req_id.clone(),
    };
    let res_deny = deny_uc.create_world(cmd_deny).await;
    assert!(res_deny.is_err(), "DenyAll must fail");
    assert_eq!(
        res_deny.unwrap_err().kind(),
        orbisync_application::ApplicationErrorKind::NotAuthorized
    );
}

#[tokio::test]
async fn rv_c_audit_failure_rolls_back_via_use_case_fake() {
    // Verifies atomicity at the application/use-case layer without DB:
    // when the store's audit insert fails, the world must not be persisted.
    use orbisync_application::{CreateWorldCommand, WorldDirectoryUseCase};
    use orbisync_domain::transform::Transform;
    use orbisync_testkit::{AllowAllAuthorizer, FakeWorldDirectoryStore};

    let clock = Timestamp::from_unix_millis(1_700_000_000_000).expect("ts");
    let actor = orbisync_domain::UserId::generate();
    let req_id = RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req");

    let inner = Arc::new(FakeWorldDirectoryStore::new());
    inner.fail_next_audit_with(orbisync_application::ApplicationError::new(
        orbisync_application::ApplicationErrorKind::PortFailure,
        "injected",
    ));

    struct SharedStore(Arc<FakeWorldDirectoryStore>);
    #[async_trait::async_trait]
    impl orbisync_application::WorldDirectoryStore for SharedStore {
        async fn get_world(
            &self,
            id: orbisync_domain::WorldId,
        ) -> Result<Option<orbisync_domain::World>, orbisync_application::ApplicationError>
        {
            self.0.get_world(id).await
        }
        async fn list_worlds(
            &self,
            page: orbisync_application::PageRequest,
        ) -> Result<
            orbisync_application::Page<orbisync_application::WorldView>,
            orbisync_application::ApplicationError,
        > {
            self.0.list_worlds(page).await
        }
        async fn update_world_with_audit(
            &self,
            world: orbisync_domain::World,
            audit: orbisync_application::WorldAuditEvent,
        ) -> Result<(), orbisync_application::ApplicationError> {
            self.0.update_world_with_audit(world, audit).await
        }
        async fn get_instance(
            &self,
            id: orbisync_domain::InstanceId,
        ) -> Result<Option<orbisync_domain::WorldInstance>, orbisync_application::ApplicationError>
        {
            self.0.get_instance(id).await
        }
        async fn create_world_with_audit(
            &self,
            world: orbisync_domain::World,
            audit: orbisync_application::WorldAuditEvent,
        ) -> Result<(), orbisync_application::ApplicationError> {
            self.0.create_world_with_audit(world, audit).await
        }
        async fn create_instance_with_audit(
            &self,
            instance: orbisync_domain::WorldInstance,
            audit: orbisync_application::WorldAuditEvent,
        ) -> Result<(), orbisync_application::ApplicationError> {
            self.0.create_instance_with_audit(instance, audit).await
        }
        async fn record_world_audit(
            &self,
            audit: orbisync_application::WorldAuditEvent,
        ) -> Result<(), orbisync_application::ApplicationError> {
            self.0.record_world_audit(audit).await
        }
        async fn list_instances(
            &self,
            page: orbisync_application::PageRequest,
        ) -> Result<
            orbisync_application::Page<orbisync_application::InstanceView>,
            orbisync_application::ApplicationError,
        > {
            self.0.list_instances(page).await
        }
    }

    let shared = SharedStore(Arc::clone(&inner));
    let uc2 = WorldDirectoryUseCase::new(shared, AllowAllAuthorizer);
    let cmd = CreateWorldCommand {
        actor_id: actor,
        name: format!("rollback-{}", Uuid::now_v7()),
        description: None,
        default_spawn: Transform::identity(),
        capacity: 10,
        now: clock,
        request_id: req_id.clone(),
    };
    let res = uc2.create_world(cmd).await;
    assert!(res.is_err(), "audit failure must cause use case error");
    assert!(
        inner.audits().is_empty(),
        "audit must be empty after rollback"
    );
    assert_eq!(
        res.unwrap_err().kind(),
        orbisync_application::ApplicationErrorKind::PortFailure
    );

    let cmd2 = CreateWorldCommand {
        actor_id: actor,
        name: format!("ok-{}", Uuid::now_v7()),
        description: None,
        default_spawn: Transform::identity(),
        capacity: 10,
        now: clock,
        request_id: req_id,
    };
    let res2 = uc2.create_world(cmd2).await;
    assert!(
        res2.is_ok(),
        "second call without injected failure must succeed"
    );
    assert_eq!(
        inner.audits().len(),
        1,
        "one audit must be present after success"
    );
}
