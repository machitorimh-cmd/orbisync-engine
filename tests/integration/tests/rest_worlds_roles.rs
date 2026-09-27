//! World and role REST endpoint integration tests via real HTTP + Postgres.
//!
//! Covers the 5 endpoints wired for this task:
//! - GET /v1/worlds/{world_id}
//! - GET /v1/worlds
//! - PATCH /v1/worlds/{world_id}
//! - POST /v1/worlds/{world_id}/archive
//! - PATCH /v1/roles/{role_id}

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, RequestId};
use orbisync_domain::{LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
    PasswordPolicy, PasswordService, token::AccessTokenService,
};
use orbisync_storage_postgres::{
    PgIdentityQueryStore, PgIdentityRepository, PgLoginStore, PgWorldAuthorizer,
    PgWorldDirectoryStore,
};
use orbisync_testkit::FixedClock;
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

async fn database_pool() -> Option<PgPool> {
    common::pool_or_skip().await
}

async fn clean_identity(pool: &PgPool) {
    sqlx::query("TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, auth_sessions, refresh_tokens, audit_events, idempotency_records CASCADE")
        .execute(pool)
        .await
        .expect("clean identity truncate");
    sqlx::query(
        "TRUNCATE world_definitions, world_instances, instance_checkpoints, outbox_events CASCADE",
    )
    .execute(pool)
    .await
    .expect("clean world truncate");
}

async fn setup_state(
    pool: PgPool,
) -> (
    HttpState,
    Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
) {
    setup_state_at(
        pool,
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    )
    .await
}

async fn setup_state_at(
    pool: PgPool,
    now: Timestamp,
) -> (
    HttpState,
    Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
) {
    let clock = Arc::new(FixedClock::new(now));
    let tokens = test_token_service();
    let passwords =
        Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("password service"));
    let repo: Arc<dyn IdentityRepository> = Arc::new(PgIdentityRepository::new(pool.clone()));
    let admin_store = Arc::new(orbisync_storage_postgres::IdentityAdministrationStore::new(
        pool.clone(),
    ));
    let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
        admin_store.clone();
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

    let world_store =
        PgWorldDirectoryStore::with_codec(pool.clone(), orbisync_testkit::insecure_test_codec());
    let world_authorizer = PgWorldAuthorizer::new(pool.clone());
    let world_directory: Arc<dyn orbisync_transport_http::worlds::WorldDirectory> = Arc::new(
        orbisync_application::WorldDirectoryUseCase::new(world_store, world_authorizer),
    );

    let qstore = Arc::new(PgIdentityQueryStore::with_codec(
        pool.clone(),
        orbisync_testkit::insecure_test_codec(),
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
    .with_login_service(login_service)
    .with_identity_repository(repo)
    .with_password_service(passwords)
    .with_identity_admin_service(admin.clone())
    .with_identity_query(qstore as Arc<dyn orbisync_application::IdentityQueryPort>)
    .with_world_directory(world_directory)
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

async fn bootstrap_and_login(pool: PgPool) -> (HttpState, String) {
    let (state, admin) = setup_state(pool).await;
    bootstrap_state(state, admin).await
}

async fn bootstrap_state(
    state: HttpState,
    admin: Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
) -> (HttpState, String) {
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
    (state, token)
}

/// Creates a second, permission-less user via the admin's HTTP endpoint and
/// logs them in, returning their bearer token.
async fn create_unprivileged_user_and_login(state: HttpState, admin_token: &str) -> String {
    let login = format!("bob-{}", Uuid::now_v7());
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": login, "display_name": "Bob" });
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
    let temp = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (status, login_json) = http_login(state, &login, &temp).await;
    assert_eq!(status, StatusCode::OK, "bob login {login_json}");
    login_json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned()
}

async fn http_json(
    state: HttpState,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    if_match: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let mut builder = Request::builder().uri(uri).method(method);
    if let Some(t) = bearer {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    if let Some(im) = if_match {
        builder = builder.header("if-match", im);
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
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

async fn create_world(state: HttpState, token: &str, name: &str) -> (String, u64) {
    let (status, json) = http_json(
        state,
        "POST",
        "/v1/worlds",
        Some(token),
        None,
        Some(serde_json::json!({ "name": name, "capacity": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create world {json}");
    let id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    let rev = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    (id, rev)
}

async fn create_role(state: HttpState, token: &str, name: &str) -> (String, u64) {
    let (status, json) = http_json(
        state,
        "POST",
        "/v1/roles",
        Some(token),
        None,
        Some(serde_json::json!({ "name": name, "permissions": ["admin.users.read"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create role {json}");
    let id = json.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    let rev = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    (id, rev)
}

// Real router + PostgreSQL regression for post-startup role escalation.
#[tokio::test]
async fn temporary_subject_role_edits_and_assignments_cannot_lift_permission_ceiling() {
    use orbisync_application::{ApplicationErrorKind, WorldAuthorizer};
    use orbisync_domain::{AuthMethod, RoleId, UserId};
    use orbisync_identity::{EphemeralMethodPolicy, EphemeralSubjectService, SessionIssuer};

    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let now = Timestamp::from_offset_date_time(time::OffsetDateTime::now_utc());
    let (state, admin) = setup_state_at(pool.clone(), now).await;
    let (state, admin_token) = bootstrap_state(state, admin).await;
    let allowed = orbisync_application::world::EPHEMERAL_SUBJECT_PERMISSION_ALLOWLIST;
    let (status, role) = http_json(
        state.clone(),
        "POST",
        "/v1/roles",
        Some(&admin_token),
        None,
        Some(serde_json::json!({"name": "Unrelated role name", "permissions": allowed})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let role_id = role["id"].as_str().unwrap();
    let role_uuid = Uuid::parse_str(role_id).unwrap();
    let mut revision = role["revision"].as_u64().unwrap();
    let policy = |method| EphemeralMethodPolicy {
        method,
        grant_roles: vec![RoleId::new(role_uuid).unwrap()],
        session_ttl_seconds: 3600,
        allowed_worlds: vec![Uuid::now_v7()],
        display_name_prefix: "Visitor".to_owned(),
    };
    let issuer = Arc::new(SessionIssuer::new(
        Arc::new(PgLoginStore::new(pool.clone())),
        state.token_service().unwrap(),
        state.clock(),
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        3600,
    ));
    let state = state.with_ephemeral_subject_service(Arc::new(EphemeralSubjectService::new(
        issuer,
        Some(policy(AuthMethod::Guest)),
        Some(policy(AuthMethod::NameOnly)),
    )));
    let repo = PgIdentityRepository::new(pool.clone());
    let world = PgWorldAuthorizer::new(pool.clone());
    let forbidden = [
        "admin.users.read",
        "admin.roles.read",
        "world.instance.stop",
        "world.future.unknown",
    ];
    let mut subjects = Vec::new();

    // Issue both before and after the live PATCH. The existing configured
    // policy retains the same role id, just as the running server does.
    for phase in 0..3 {
        if phase > 0 {
            let permissions: Vec<&str> = if phase == 1 {
                vec!["admin.users.read"] // the reported reproduction
            } else {
                allowed
                    .iter()
                    .copied()
                    .chain(forbidden[2..].iter().copied())
                    .collect()
            };
            let (status, body) = http_json(
                state.clone(),
                "PATCH",
                &format!("/v1/roles/{role_id}"),
                Some(&admin_token),
                Some(&format!("\"{revision}\"")),
                Some(serde_json::json!({"permissions": permissions})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "role edit rejected: {body}");
            revision = body["revision"].as_u64().unwrap();
        }
        for (kind, uri, body) in [
            ("guest", "/v1/auth/guest", serde_json::json!({})),
            (
                "name_only",
                "/v1/auth/name",
                serde_json::json!({"display_name": "Admin"}),
            ),
        ] {
            let (status, issued) =
                http_json(state.clone(), "POST", uri, None, None, Some(body)).await;
            assert_eq!(status, StatusCode::OK, "subject issuance failed");
            let token = issued["access_token"].as_str().unwrap().to_owned();
            let id: Uuid =
                sqlx::query_scalar("SELECT id FROM users WHERE kind = $1 ORDER BY id DESC LIMIT 1")
                    .bind(kind)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            subjects.push((UserId::new(id).unwrap(), token));
        }
        for (user, token) in &subjects {
            let (status, _) =
                http_json(state.clone(), "GET", "/v1/users", Some(token), None, None).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "phase {phase}: guest gained admin API"
            );
            let roles = repo.roles_for_user(*user).await.unwrap();
            for permission in forbidden {
                assert!(
                    roles
                        .iter()
                        .all(|role| role.permissions().iter().all(|p| p.as_str() != permission))
                );
                assert_eq!(
                    world.require(*user, permission).await.unwrap_err().kind(),
                    ApplicationErrorKind::NotAuthorized
                );
            }
            // These are the production WS handshake and hook re-resolution
            // authorizer calls. Reuse the same instance across all mutations.
            for permission in allowed {
                assert_eq!(world.require(*user, permission).await.is_ok(), phase != 1);
            }
        }
        let (status, _) = http_json(
            state.clone(),
            "GET",
            "/v1/users",
            Some(&admin_token),
            None,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "local administrator must stay usable"
        );
    }

    // Additional assignments cannot bypass the ceiling either. Permanent
    // account/external identities retain every permission of that same role.
    let (status, extra_role) = http_json(
        state.clone(),
        "POST",
        "/v1/roles",
        Some(&admin_token),
        None,
        Some(serde_json::json!({"name": "Extra admin role", "permissions": &forbidden[..2]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let extra_id = Uuid::parse_str(extra_role["id"].as_str().unwrap()).unwrap();
    for (user, token) in &subjects {
        sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
            .bind(user.as_uuid())
            .bind(extra_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(world.require(*user, "admin.users.read").await.is_err());
        let (status, _) =
            http_json(state.clone(), "GET", "/v1/users", Some(token), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        world.require(*user, "entity.update.any").await.unwrap();
    }
    for kind in ["account", "external"] {
        let id = UserId::generate();
        sqlx::query("INSERT INTO users (id, login_id, display_name, status, kind, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'Permanent', 'active', $3, FALSE, 1, now(), now())")
            .bind(id.as_uuid()).bind(format!("permanent-{id}")).bind(kind).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
            .bind(id.as_uuid())
            .bind(role_uuid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
            .bind(id.as_uuid())
            .bind(extra_id)
            .execute(&pool)
            .await
            .unwrap();
        for permission in allowed.iter().copied().chain(forbidden) {
            world.require(id, permission).await.unwrap();
        }
    }

    // No lookup failure, missing ledger, deadline, or revocation may grant
    // even an otherwise allowed permission.
    for (index, (user, _)) in subjects.iter().enumerate() {
        let sql = match index % 3 {
            0 => "DELETE FROM ephemeral_subjects WHERE user_id = $1",
            1 => {
                "UPDATE ephemeral_subjects SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE user_id = $1"
            }
            _ => "UPDATE users SET status = 'disabled' WHERE id = $1",
        };
        sqlx::query(sql)
            .bind(user.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert!(repo.roles_for_user(*user).await.unwrap().is_empty());
        assert_eq!(
            world
                .require(*user, "entity.spawn")
                .await
                .unwrap_err()
                .kind(),
            ApplicationErrorKind::NotAuthorized
        );
    }
    assert!(
        repo.roles_for_user(UserId::generate())
            .await
            .unwrap()
            .is_empty()
    );
    pool.close().await;
    assert!(repo.roles_for_user(subjects[0].0).await.is_err());
    assert!(world.require(subjects[0].0, "entity.spawn").await.is_err());
}

// ---------------------------------------------------------------------------
// GET /v1/worlds/{world_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_world_returns_created_world_and_404_for_unknown_and_401_without_bearer() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token) = bootstrap_and_login(pool).await;
    let (world_id, rev) = create_world(state.clone(), &token, "Lobby").await;

    let (status, json) = http_json(
        state.clone(),
        "GET",
        &format!("/v1/worlds/{world_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get world {json}");
    assert_eq!(
        json.get("id").and_then(|v| v.as_str()),
        Some(world_id.as_str())
    );
    assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("active"));
    assert_eq!(json.get("revision").and_then(|v| v.as_u64()), Some(rev));

    let unknown = Uuid::now_v7();
    let (status_404, json_404) = http_json(
        state.clone(),
        "GET",
        &format!("/v1/worlds/{unknown}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status_404, StatusCode::NOT_FOUND, "{json_404}");

    let (status_401, json_401) = http_json(
        state,
        "GET",
        &format!("/v1/worlds/{world_id}"),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status_401, StatusCode::UNAUTHORIZED, "{json_401}");
}

#[tokio::test]
async fn get_world_is_forbidden_for_a_subject_without_admin_worlds_read() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, admin_token) = bootstrap_and_login(pool).await;
    let (world_id, _) = create_world(state.clone(), &admin_token, "Restricted").await;
    let bob_token = create_unprivileged_user_and_login(state.clone(), &admin_token).await;

    let (status, json) = http_json(
        state,
        "GET",
        &format!("/v1/worlds/{world_id}"),
        Some(&bob_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
}

// ---------------------------------------------------------------------------
// GET /v1/worlds (list + pagination boundary)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_worlds_paginates_and_returns_next_cursor_at_the_boundary() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token) = bootstrap_and_login(pool).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let (id, _) = create_world(state.clone(), &token, &format!("World {i}")).await;
        ids.push(id);
    }
    ids.sort();

    let (status, page1) = http_json(
        state.clone(),
        "GET",
        "/v1/worlds?limit=2",
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    let items1 = page1.get("items").and_then(|v| v.as_array()).unwrap();
    assert_eq!(items1.len(), 2, "first page must have exactly limit items");
    let cursor = page1
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .expect("next_cursor must be present at the boundary")
        .to_owned();

    let (status2, page2) = http_json(
        state,
        "GET",
        &format!("/v1/worlds?limit=2&cursor={cursor}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status2, StatusCode::OK, "{page2}");
    let items2 = page2.get("items").and_then(|v| v.as_array()).unwrap();
    assert_eq!(items2.len(), 1, "second page must have the remaining item");
    assert!(
        page2.get("next_cursor").is_none() || page2.get("next_cursor").unwrap().is_null(),
        "last page must not carry a next_cursor: {page2}"
    );
}

// ---------------------------------------------------------------------------
// PATCH /v1/worlds/{world_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_world_applies_merge_patch_and_advances_revision() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token) = bootstrap_and_login(pool).await;
    let (world_id, rev) = create_world(state.clone(), &token, "Before").await;

    let (status, json) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/worlds/{world_id}"),
        Some(&token),
        Some(&format!("\"{rev}\"")),
        Some(serde_json::json!({ "name": "After", "capacity": 42 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update world {json}");
    assert_eq!(json.get("name").and_then(|v| v.as_str()), Some("After"));
    let new_rev = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    assert_eq!(new_rev, rev + 1);

    // Missing If-Match -> 400.
    let (status_no_match, _) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/worlds/{world_id}"),
        Some(&token),
        None,
        Some(serde_json::json!({ "name": "NoMatch" })),
    )
    .await;
    assert_eq!(status_no_match, StatusCode::BAD_REQUEST);

    // Stale revision -> 409.
    let (status_stale, json_stale) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/worlds/{world_id}"),
        Some(&token),
        Some(&format!("\"{rev}\"")),
        Some(serde_json::json!({ "name": "Stale" })),
    )
    .await;
    assert_eq!(status_stale, StatusCode::CONFLICT, "{json_stale}");

    // Unknown world -> 404.
    let unknown = Uuid::now_v7();
    let (status_404, json_404) = http_json(
        state,
        "PATCH",
        &format!("/v1/worlds/{unknown}"),
        Some(&token),
        Some("\"1\""),
        Some(serde_json::json!({ "name": "Nope" })),
    )
    .await;
    assert_eq!(status_404, StatusCode::NOT_FOUND, "{json_404}");
}

// ---------------------------------------------------------------------------
// POST /v1/worlds/{world_id}/archive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archive_world_is_idempotent_and_blocks_further_updates_to_status_only() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token) = bootstrap_and_login(pool).await;
    let (world_id, rev) = create_world(state.clone(), &token, "ToArchive").await;
    let idem_key = Uuid::now_v7().to_string();

    let archive_uri = format!("/v1/worlds/{world_id}/archive");
    let app = router(state.clone());
    let req = Request::builder()
        .uri(&archive_uri)
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", &idem_key)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "archive world {json}");
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("archived")
    );
    let archived_rev = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    assert_eq!(archived_rev, rev + 1);

    // Archiving again is a no-op: 200, same revision, no further state change.
    let app2 = router(state.clone());
    let req2 = Request::builder()
        .uri(&archive_uri)
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", &idem_key)
        .body(Body::empty())
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    let status2 = resp2.status();
    let bytes2 = resp2.into_body().collect().await.unwrap().to_bytes();
    let json2: serde_json::Value = serde_json::from_slice(&bytes2).unwrap();
    assert_eq!(status2, StatusCode::OK, "re-archive {json2}");
    assert_eq!(
        json2.get("revision").and_then(|v| v.as_u64()),
        Some(archived_rev),
        "idempotent archive must not bump revision again"
    );

    // Missing Idempotency-Key -> 400.
    let app3 = router(state.clone());
    let req3 = Request::builder()
        .uri(format!("/v1/worlds/{world_id}/archive"))
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp3 = app3.oneshot(req3).await.unwrap();
    assert_eq!(resp3.status(), StatusCode::BAD_REQUEST);

    // Unknown world -> 404.
    let unknown = Uuid::now_v7();
    let app4 = router(state);
    let req4 = Request::builder()
        .uri(format!("/v1/worlds/{unknown}/archive"))
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", Uuid::now_v7().to_string())
        .body(Body::empty())
        .unwrap();
    let resp4 = app4.oneshot(req4).await.unwrap();
    assert_eq!(resp4.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// PATCH /v1/roles/{role_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_role_applies_merge_patch_including_null_description_clear() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, token) = bootstrap_and_login(pool).await;
    let (role_id, rev) = create_role(state.clone(), &token, "Reader").await;

    let (status, json) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/roles/{role_id}"),
        Some(&token),
        Some(&format!("\"{rev}\"")),
        Some(serde_json::json!({
            "name": "Renamed Reader",
            "description": "temp description",
            "permissions": ["admin.users.read", "admin.roles.read"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update role {json}");
    assert_eq!(
        json.get("name").and_then(|v| v.as_str()),
        Some("Renamed Reader")
    );
    assert_eq!(
        json.get("description").and_then(|v| v.as_str()),
        Some("temp description")
    );
    let rev2 = json.get("revision").and_then(|v| v.as_u64()).unwrap();
    assert_eq!(rev2, rev + 1);
    let perms: Vec<&str> = json
        .get("permissions")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(perms, vec!["admin.roles.read", "admin.users.read"]);

    // Explicit null clears description; name/permissions untouched (absent keys).
    let (status_clear, json_clear) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/roles/{role_id}"),
        Some(&token),
        Some(&format!("\"{rev2}\"")),
        Some(serde_json::json!({ "description": null })),
    )
    .await;
    assert_eq!(status_clear, StatusCode::OK, "{json_clear}");
    assert!(json_clear.get("description").unwrap().is_null());
    assert_eq!(
        json_clear.get("name").and_then(|v| v.as_str()),
        Some("Renamed Reader"),
        "name must be unchanged when the key is absent from the patch"
    );
    let rev3 = json_clear.get("revision").and_then(|v| v.as_u64()).unwrap();
    assert_eq!(rev3, rev2 + 1);

    // Stale revision -> 409.
    let (status_stale, json_stale) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/roles/{role_id}"),
        Some(&token),
        Some(&format!("\"{rev}\"")),
        Some(serde_json::json!({ "name": "Stale" })),
    )
    .await;
    assert_eq!(status_stale, StatusCode::CONFLICT, "{json_stale}");

    // Unknown role -> 404.
    let unknown = Uuid::now_v7();
    let (status_404, json_404) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/roles/{unknown}"),
        Some(&token),
        Some("\"1\""),
        Some(serde_json::json!({ "name": "Nope" })),
    )
    .await;
    assert_eq!(status_404, StatusCode::NOT_FOUND, "{json_404}");

    // Missing If-Match -> 400.
    let (status_no_match, _) = http_json(
        state.clone(),
        "PATCH",
        &format!("/v1/roles/{role_id}"),
        Some(&token),
        None,
        Some(serde_json::json!({ "name": "NoMatch" })),
    )
    .await;
    assert_eq!(status_no_match, StatusCode::BAD_REQUEST);

    // Mixed-namespace permission replacement is rejected by the domain invariant.
    let (status_mixed, json_mixed) = http_json(
        state,
        "PATCH",
        &format!("/v1/roles/{role_id}"),
        Some(&token),
        Some(&format!("\"{rev3}\"")),
        Some(serde_json::json!({ "permissions": ["admin.roles.read", "world.join"] })),
    )
    .await;
    assert_eq!(status_mixed, StatusCode::BAD_REQUEST, "{json_mixed}");
}

#[tokio::test]
async fn update_role_is_forbidden_for_a_subject_without_admin_roles_update() {
    let _guard = db_guard().lock().await;
    let Some(pool) = database_pool().await else {
        return;
    };
    clean_identity(&pool).await;
    let (state, admin_token) = bootstrap_and_login(pool).await;
    let (role_id, rev) = create_role(state.clone(), &admin_token, "Reader2").await;
    let bob_token = create_unprivileged_user_and_login(state.clone(), &admin_token).await;

    let (status, json) = http_json(
        state,
        "PATCH",
        &format!("/v1/roles/{role_id}"),
        Some(&bob_token),
        Some(&format!("\"{rev}\"")),
        Some(serde_json::json!({ "name": "Hijacked" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
}
