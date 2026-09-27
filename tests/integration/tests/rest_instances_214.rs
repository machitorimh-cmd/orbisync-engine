//! REST instance endpoints: real PostgreSQL + real HTTP boundary.
//!
//! Covers the six endpoints wired for the "rest-instances" task:
//! `GET/POST /v1/instances`, `GET /v1/instances/{instance_id}`,
//! `POST /v1/instances/{instance_id}/start`, `POST /v1/instances/{instance_id}/stop`,
//! `POST /v1/instances/{instance_id}/kick/{user_id}`,
//! `GET /v1/instances/{instance_id}/members`.
//!
//! `start`/`stop` and `get`/`list` are backed by `PgWorldDirectoryStore` (real
//! DB). `members`/`kick` are backed by live runtime presence
//! (`session_store.rs` D-24), which this lightweight HTTP-only harness does
//! not spin up; they are exercised here against
//! `orbisync_testkit::FakeInstanceMembershipStore` while instance existence
//! checks still go through the real `PgWorldDirectoryStore`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::IdentityRepository;
use orbisync_domain::{Clock as _, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
    PasswordPolicy, PasswordService, token::AccessTokenService,
};
use orbisync_storage_postgres::{
    IdempotencyStore as PgIdempotencyStore, IdentityAdministrationStore as PgIdentityAdminStore,
    PgIdentityRepository, PgWorldAuthorizer, PgWorldDirectoryStore,
};
use orbisync_testkit::{FakeInstanceMembershipStore, FixedClock};
use orbisync_transport_http::{HttpState, router};
use sqlx::PgPool;
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use uuid::Uuid;

mod common;

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

/// Builds `HttpState` with a real `PgWorldDirectoryStore`/`PgWorldAuthorizer`
/// for `world_directory` and a fake runtime-membership port for
/// `instance_membership` (see module doc).
fn http_state(
    pool: PgPool,
    clock: Arc<FixedClock>,
    tokens: Arc<AccessTokenService>,
    membership: Arc<FakeInstanceMembershipStore>,
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

    let world_store = PgWorldDirectoryStore::with_codec(
        pool.clone(),
        orbisync_application::pagination::CursorCodec::new(
            b"rest-instances-214-test-key-32b".to_vec(),
        )
        .expect("test codec"),
    );
    let world_authorizer = PgWorldAuthorizer::new(pool.clone());
    let world_directory: Arc<dyn orbisync_transport_http::worlds::WorldDirectory> = Arc::new(
        orbisync_application::WorldDirectoryUseCase::new(world_store, world_authorizer),
    );

    let membership_store = PgWorldDirectoryStore::with_codec(
        pool.clone(),
        orbisync_application::pagination::CursorCodec::new(
            b"rest-instances-214-members-key32".to_vec(),
        )
        .expect("test codec"),
    );
    let membership_authorizer = PgWorldAuthorizer::new(pool.clone());
    let instance_membership: Arc<
        dyn orbisync_transport_http::instance_membership::InstanceMembership,
    > = Arc::new(orbisync_application::InstanceMembershipUseCase::new(
        membership_store,
        membership,
        membership_authorizer,
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
    .with_instance_membership(instance_membership)
    .with_idempotency_store(idem_store as Arc<dyn orbisync_application::IdempotencyStore>)
    .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-rest-i214".to_vec())
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

async fn http_json(
    state: HttpState,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let mut builder = Request::builder().uri(uri).method(method);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(t) = bearer {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let payload = body.map_or_else(String::new, |b| b.to_string());
    let req = builder.body(Body::from(payload)).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn clean(pool: &PgPool) {
    sqlx::query("TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, auth_sessions, refresh_tokens, audit_events, idempotency_records, world_definitions, world_instances, instance_checkpoints, outbox_events CASCADE")
        .execute(pool)
        .await
        .expect("TRUNCATE should succeed");
}

/// Inserts a user directly via SQL with an explicit permission set, bypassing
/// the fixed bootstrap-administrator grant set so tests can exercise
/// narrower and empty permission sets.
async fn insert_user_with_permissions(
    pool: &PgPool,
    clock: &FixedClock,
    login: &str,
    password: &str,
    permissions: &[&str],
) -> Uuid {
    let now = clock.now();
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let hash = passwords
        .hash(orbisync_application::SecretString::new(password))
        .await
        .expect("hash")
        .expose_phc()
        .to_owned();
    let user_id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $2, 'active', true, 1, $3, $3)")
        .bind(user_id)
        .bind(login)
        .bind(now.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert user");
    sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(&hash)
        .bind(now.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert cred");
    if !permissions.is_empty() {
        let role_id = Uuid::now_v7();
        sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, 'rest-instances-214 test role', 1)")
            .bind(role_id)
            .bind(format!("role-{}", Uuid::now_v7()))
            .execute(pool)
            .await
            .expect("insert role");
        for perm in permissions {
            sqlx::query("INSERT INTO permissions (name) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(*perm)
                .execute(pool)
                .await
                .expect("perm");
            sqlx::query("INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)")
                .bind(role_id)
                .bind(*perm)
                .execute(pool)
                .await
                .expect("role perm");
        }
        sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
            .bind(user_id)
            .bind(role_id)
            .execute(pool)
            .await
            .expect("user role");
    }
    user_id
}

async fn login_token(state: HttpState, login: &str, password: &str) -> String {
    let (status, json) = http_login(state, login, password).await;
    assert_eq!(status, StatusCode::OK, "login {login} {json}");
    json.get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned()
}

async fn create_world_and_instance(pool: &PgPool, clock: Timestamp, capacity: u32) -> (Uuid, Uuid) {
    let world_id = Uuid::now_v7();
    sqlx::query("INSERT INTO world_definitions (id, name, description, status, capacity, default_spawn, metadata, revision, created_at, updated_at) VALUES ($1,$2,NULL,'active',$3,$4,$5,1,$6,$6)")
        .bind(world_id)
        .bind(format!("world-{world_id}"))
        .bind(i32::try_from(capacity).unwrap())
        .bind(serde_json::json!({
            "position": {"x": 0.0, "y": 0.0, "z": 0.0},
            "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
            "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
        }))
        .bind(serde_json::json!({}))
        .bind(clock.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert world");
    let instance_id = Uuid::now_v7();
    sqlx::query("INSERT INTO world_instances (id, world_id, lifecycle, capacity, created_at, started_at, revision) VALUES ($1,$2,'created',$3,$4,NULL,1)")
        .bind(instance_id)
        .bind(world_id)
        .bind(i32::try_from(capacity).unwrap())
        .bind(clock.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert instance");
    (world_id, instance_id)
}

const ALL_INSTANCE_PERMS: &[&str] = &[
    "world.instance.read",
    "world.instance.start",
    "world.instance.stop",
    "moderation.kick",
];

// ------------------------------------------------------------
// 1. Normal path: get / list / start / stop, real DB state transitions.
// ------------------------------------------------------------
#[tokio::test]
async fn instances_get_list_start_stop_happy_path() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else {
        return;
    };
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = token_service();
    let membership = Arc::new(FakeInstanceMembershipStore::new());
    let (state, _admin) = http_state(pool.clone(), Arc::clone(&clock), tokens, membership);

    let login = format!("i214-admin-{}", Uuid::now_v7());
    let password = "AdminPass123!@#";
    insert_user_with_permissions(&pool, &clock, &login, password, ALL_INSTANCE_PERMS).await;
    let token = login_token(state.clone(), &login, password).await;

    let (_world_id, instance_id) = create_world_and_instance(&pool, clock.now(), 10).await;

    let (status, json) = http_json(
        state.clone(),
        "GET",
        &format!("/v1/instances/{instance_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get instance {json}");
    assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("created"));

    let (status, json) = http_json(state.clone(), "GET", "/v1/instances", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "list instances {json}");
    let items = json.get("items").and_then(|v| v.as_array()).expect("items");
    assert!(
        items
            .iter()
            .any(|item| item.get("id").and_then(|v| v.as_str()) == Some(&instance_id.to_string())),
        "created instance must appear in the list: {json}"
    );

    let (status, json) = http_json(
        state.clone(),
        "POST",
        &format!("/v1/instances/{instance_id}/start"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "start instance {json}");
    assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("running"));

    let (status, json) = http_json(
        state.clone(),
        "POST",
        &format!("/v1/instances/{instance_id}/stop"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "stop instance {json}");
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("stopping")
    );

    let (status, json) = http_json(
        state.clone(),
        "GET",
        &format!("/v1/instances/{instance_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get after stop {json}");
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("stopping"),
        "GET must reflect the persisted stopping state {json}"
    );
}

// ------------------------------------------------------------
// 2. Unauthorized actor (no permissions) is rejected for every instance
//    endpoint, including members/kick.
// ------------------------------------------------------------
#[tokio::test]
async fn instances_endpoints_reject_unauthorized_actor() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else {
        return;
    };
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = token_service();
    let membership = Arc::new(FakeInstanceMembershipStore::new());
    let (state, _admin) = http_state(pool.clone(), Arc::clone(&clock), tokens, membership);

    let (_world_id, instance_id) = create_world_and_instance(&pool, clock.now(), 10).await;

    let bob_login = format!("i214-bob-{}", Uuid::now_v7());
    let bob_password = "BobPass123!@#";
    insert_user_with_permissions(&pool, &clock, &bob_login, bob_password, &[]).await;
    let bob_token = login_token(state.clone(), &bob_login, bob_password).await;

    let other_user_id = Uuid::now_v7();
    let checks: Vec<(&str, &str)> = vec![
        ("GET", "instances"),
        ("GET", "instance"),
        ("POST", "start"),
        ("POST", "stop"),
        ("GET", "members"),
        ("POST", "kick"),
    ];
    for (method, kind) in checks {
        let uri = match kind {
            "instances" => "/v1/instances".to_owned(),
            "instance" => format!("/v1/instances/{instance_id}"),
            "start" => format!("/v1/instances/{instance_id}/start"),
            "stop" => format!("/v1/instances/{instance_id}/stop"),
            "members" => format!("/v1/instances/{instance_id}/members"),
            "kick" => format!("/v1/instances/{instance_id}/kick/{other_user_id}"),
            _ => unreachable!(),
        };
        let (status, json) = http_json(state.clone(), method, &uri, Some(&bob_token), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "unauthorized actor must be 403 for {method} {uri}, got {json}"
        );
        assert_eq!(
            json.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("ACCESS_DENIED")
        );
    }
}

// ------------------------------------------------------------
// 3. 404 for unknown instance_id / unknown (non-member) user_id.
// ------------------------------------------------------------
#[tokio::test]
async fn instances_404_for_unknown_instance_and_member() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else {
        return;
    };
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = token_service();
    let membership = Arc::new(FakeInstanceMembershipStore::new());
    let (state, _admin) = http_state(
        pool.clone(),
        Arc::clone(&clock),
        tokens,
        Arc::clone(&membership),
    );

    let login = format!("i214-admin404-{}", Uuid::now_v7());
    let password = "AdminPass123!@#";
    insert_user_with_permissions(&pool, &clock, &login, password, ALL_INSTANCE_PERMS).await;
    let token = login_token(state.clone(), &login, password).await;

    let unknown_instance_id = Uuid::now_v7();
    for (method, uri) in [
        ("GET", format!("/v1/instances/{unknown_instance_id}")),
        ("POST", format!("/v1/instances/{unknown_instance_id}/start")),
        ("POST", format!("/v1/instances/{unknown_instance_id}/stop")),
        (
            "GET",
            format!("/v1/instances/{unknown_instance_id}/members"),
        ),
        (
            "POST",
            format!(
                "/v1/instances/{unknown_instance_id}/kick/{}",
                Uuid::now_v7()
            ),
        ),
    ] {
        let (status, json) = http_json(state.clone(), method, &uri, Some(&token), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "unknown instance must 404 for {method} {uri}, got {json}"
        );
    }

    // A real instance exists, but the target user is not a live member.
    let (_world_id, instance_id) = create_world_and_instance(&pool, clock.now(), 10).await;
    let not_a_member = Uuid::now_v7();
    let (status, json) = http_json(
        state.clone(),
        "POST",
        &format!("/v1/instances/{instance_id}/kick/{not_a_member}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "kicking a non-member must 404, got {json}"
    );
}

// ------------------------------------------------------------
// 4. GET /v1/instances pagination boundary (real DB, keyset cursor).
// ------------------------------------------------------------
#[tokio::test]
async fn instances_list_pagination_boundary() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else {
        return;
    };
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = token_service();
    let membership = Arc::new(FakeInstanceMembershipStore::new());
    let (state, _admin) = http_state(pool.clone(), Arc::clone(&clock), tokens, membership);

    let login = format!("i214-adminpg-{}", Uuid::now_v7());
    let password = "AdminPass123!@#";
    insert_user_with_permissions(&pool, &clock, &login, password, ALL_INSTANCE_PERMS).await;
    let token = login_token(state.clone(), &login, password).await;

    let mut created = Vec::new();
    for _ in 0..3 {
        let (_world_id, instance_id) = create_world_and_instance(&pool, clock.now(), 5).await;
        created.push(instance_id.to_string());
    }
    created.sort();

    let (status, json) = http_json(
        state.clone(),
        "GET",
        "/v1/instances?limit=2",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "page 1 {json}");
    let items = json.get("items").and_then(|v| v.as_array()).expect("items");
    assert_eq!(items.len(), 2, "page 1 must have 2 items, got {json}");
    let next_cursor = json
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    assert!(next_cursor.is_some(), "third instance remains: {json}");

    let (status, json) = http_json(
        state.clone(),
        "GET",
        &format!("/v1/instances?limit=2&cursor={}", next_cursor.unwrap()),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "page 2 {json}");
    let items2 = json.get("items").and_then(|v| v.as_array()).expect("items");
    assert!(
        json.get("next_cursor").is_none()
            || json.get("next_cursor") == Some(&serde_json::Value::Null),
        "last page must not carry a cursor: {json}"
    );

    let mut seen: Vec<String> = items
        .iter()
        .chain(items2.iter())
        .map(|it| it.get("id").and_then(|v| v.as_str()).unwrap().to_owned())
        .collect();
    seen.sort();
    assert_eq!(
        seen.len(),
        3,
        "traversal must visit exactly 3 created instances"
    );
    for id in &created {
        assert!(
            seen.contains(id),
            "cursor traversal must include {id}: {seen:?}"
        );
    }
}

// ------------------------------------------------------------
// 5. members list pagination + kick removes a live member (fake runtime port).
// ------------------------------------------------------------
#[tokio::test]
async fn instance_members_list_paginates_and_kick_removes_member() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database().await else {
        return;
    };
    clean(&pool).await;
    let clock = fixed_clock();
    let tokens = token_service();
    let membership = Arc::new(FakeInstanceMembershipStore::new());
    let (state, _admin) = http_state(
        pool.clone(),
        Arc::clone(&clock),
        tokens,
        Arc::clone(&membership),
    );

    let login = format!("i214-adminmem-{}", Uuid::now_v7());
    let password = "AdminPass123!@#";
    insert_user_with_permissions(&pool, &clock, &login, password, ALL_INSTANCE_PERMS).await;
    let token = login_token(state.clone(), &login, password).await;

    let (_world_id, instance_id) = create_world_and_instance(&pool, clock.now(), 10).await;
    let mut member_ids: Vec<orbisync_domain::UserId> = (0..3)
        .map(|_| orbisync_domain::UserId::generate())
        .collect();
    member_ids.sort_by_key(orbisync_domain::UserId::to_string);
    let instance_domain_id = orbisync_domain::InstanceId::new(instance_id).expect("valid");
    membership.seed(instance_domain_id, member_ids.clone());

    let (status, json) = http_json(
        state.clone(),
        "GET",
        &format!("/v1/instances/{instance_id}/members?limit=2"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "members page 1 {json}");
    let items = json.get("items").and_then(|v| v.as_array()).expect("items");
    assert_eq!(items.len(), 2, "members page 1 must have 2 items");
    assert!(
        json.get("next_cursor").and_then(|v| v.as_str()).is_some(),
        "third member remains: {json}"
    );

    let target = member_ids[0];
    let (status, _json) = http_json(
        state.clone(),
        "POST",
        &format!("/v1/instances/{instance_id}/kick/{target}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "kick must succeed");

    let remaining = pollster::block_on(async {
        use orbisync_application::InstanceMembershipStore as _;
        membership.list_members(instance_domain_id).await.unwrap()
    });
    assert!(
        !remaining.contains(&target),
        "kicked member must be removed from the live membership port"
    );

    // Kicking the same user again is now 404 (no longer a live member).
    let (status, json) = http_json(
        state.clone(),
        "POST",
        &format!("/v1/instances/{instance_id}/kick/{target}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "kicking an already-removed member must 404, got {json}"
    );
}
