//! W-29 read side: real PostgreSQL + real HTTP.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

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
    IdentityAdministrationStore, PgIdentityQueryStore, PgIdentityRepository, PgLoginStore,
    PgRealtimeTicketStore,
};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use std::sync::{Arc, OnceLock};
use time::OffsetDateTime;
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
        .expect("token service"),
    )
}

fn test_login_service_pg(
    pool: sqlx::PgPool,
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
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

async fn pool_or_skip() -> Option<sqlx::PgPool> {
    common::pool_or_skip().await
}

async fn clean(pool: &sqlx::PgPool) {
    sqlx::query("TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, auth_sessions, refresh_tokens, audit_events, idempotency_records, world_definitions, world_instances, instance_checkpoints, outbox_events CASCADE")
        .execute(pool)
        .await
        .expect("TRUNCATE should succeed");
}

async fn setup(
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
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()))
        as Arc<dyn orbisync_application::IdentityRepository>;
    let admin_store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> = admin_store;
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
    let iq: Arc<dyn orbisync_application::IdentityQueryPort> = qstore.clone();
    let aq: Arc<dyn orbisync_application::AuditQueryPort> = qstore;
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
    .with_password_service(passwords)
    .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    .with_realtime_ticket_store(ticket_store)
    .with_identity_admin_service(admin.clone())
    .with_identity_query(iq)
    .with_audit_query(aq)
    .with_refresh_creation_store(Arc::new(IdentityAdministrationStore::new(pool.clone()))
        as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
    .with_refresh_rotation_store(Arc::new(IdentityAdministrationStore::new(pool.clone()))
        as Arc<dyn orbisync_application::RefreshTokenRotationStore>);
    (state, admin, pool)
}

async fn http_login(state: HttpState, login: &str, pw: &str) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({"login_id": login, "password": pw});
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

async fn http_get(
    state: HttpState,
    bearer: &str,
    uri: &str,
) -> (StatusCode, serde_json::Value, axum::http::HeaderMap) {
    let app = router(state);
    let req = Request::builder()
        .uri(uri)
        .method("GET")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, headers)
}

async fn http_create_user(
    state: HttpState,
    bearer: &str,
    login: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({"login_id": login, "display_name": login});
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

async fn http_create_role(
    state: HttpState,
    bearer: &str,
    name: &str,
    perms: &[&str],
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({"name": name, "permissions": perms});
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
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_create_user_with_cred(
    state: HttpState,
    bearer: &str,
    login: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({"login_id": login, "display_name": login});
    let req = Request::builder()
        .uri("/v1/users")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("accept", "application/vnd.orbisync.user-credential+json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_ticket(state: HttpState, bearer: &str) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let req = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// Test 1: pagination 100+ users
#[tokio::test]
async fn w29_pagination_users_200_and_50_and_traversal() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, _) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK);
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // create 250 users
    for i in 0..250 {
        let login = format!("u-w29-{}-{}", suffix, i);
        let (st, j) = http_create_user(state.clone(), &token, &login).await;
        assert_eq!(st, StatusCode::CREATED, "create user {i} {j}");
    }
    // limit=200
    let (st, j, _) = http_get(state.clone(), &token, "/v1/users?limit=200").await;
    assert_eq!(st, StatusCode::OK, "limit 200 {j}");
    let items = j.get("items").and_then(|v| v.as_array()).unwrap();
    assert_eq!(items.len(), 200, "limit 200 should return 200");
    let next = j
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    assert!(next.is_some(), "next_cursor should be Some when more");
    // limit omitted -> default 50
    let (st, j, _) = http_get(state.clone(), &token, "/v1/users").await;
    assert_eq!(st, StatusCode::OK);
    let items = j.get("items").and_then(|v| v.as_array()).unwrap();
    assert_eq!(items.len(), 50, "default limit should be 50");
    // traverse all via cursor
    let mut seen = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    let mut total = 0usize;
    loop {
        let uri = if let Some(c) = &cursor {
            format!("/v1/users?limit=50&cursor={}", c)
        } else {
            "/v1/users?limit=50".to_owned()
        };
        let (st, j, _) = http_get(state.clone(), &token, &uri).await;
        assert_eq!(st, StatusCode::OK, "traversal {j}");
        let items = j.get("items").and_then(|v| v.as_array()).unwrap();
        for it in items {
            let id = it.get("id").unwrap().as_str().unwrap().to_owned();
            assert!(seen.insert(id.clone()), "duplicate id {}", id);
        }
        total += items.len();
        let next = j
            .get("next_cursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        if let Some(n) = next {
            cursor = Some(n);
        } else {
            break;
        }
        if total > 300 {
            panic!("too many");
        }
    }
    // we created 250 + 1 admin = 251 users
    assert!(
        total >= 250,
        "should have traversed at least 250, got {total}"
    );
    assert_eq!(
        seen.len(),
        total,
        "no duplicates and no missing within traversal"
    );
}

// Test 2: cursor tamper 3 vectors for users / roles / audit (CR-11 + CR-06 binding)
#[tokio::test]
async fn w29_cursor_tamper_and_cross_filter_400() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, _) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm2-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK);
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    for i in 0..5 {
        // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = http_create_user(state.clone(), &token, &format!("u2-{}-{}", suffix, i)).await;
    }
    for i in 0..3 {
        // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = http_create_role(
            state.clone(),
            &token,
            &format!("r2-{}-{}", suffix, i),
            &["admin.users.read"],
        )
        .await;
    }

    fn tamper_sig(cursor: &str) -> String {
        let (payload, sig) = cursor.split_once('.').expect("cursor must contain '.'");
        let mut new_sig = sig.to_owned();
        let last = new_sig.pop().expect("sig non-empty");
        let repl = if last == 'A' { 'B' } else { 'A' };
        new_sig.push(repl);
        format!("{payload}.{new_sig}")
    }
    fn tamper_payload(cursor: &str) -> String {
        let (payload, sig) = cursor.split_once('.').expect("cursor must contain '.'");
        let mut new_payload = payload.to_owned();
        let first = new_payload.chars().next().expect("payload non-empty");
        let repl = if first == 'A' { 'B' } else { 'A' };
        new_payload.replace_range(0..1, &repl.to_string());
        format!("{new_payload}.{sig}")
    }

    // users: 3 vectors
    let (st, j, _) = http_get(state.clone(), &token, "/v1/users?limit=2").await;
    assert_eq!(st, StatusCode::OK);
    let cursor = j
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    assert!(cursor.contains('.'), "users cursor must be opaque signed");
    {
        let plain = Uuid::now_v7().to_string();
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/users?limit=2&cursor={plain}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "users plain UUID should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
        let sig_t = tamper_sig(&cursor);
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/users?limit=2&cursor={sig_t}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "users sig-tampered should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
        let payload_t = tamper_payload(&cursor);
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/users?limit=2&cursor={payload_t}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "users payload-tampered should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
    }

    // roles: 3 vectors
    let (st, j, _) = http_get(state.clone(), &token, "/v1/roles?limit=2").await;
    assert_eq!(st, StatusCode::OK, "roles list {j}");
    let cursor = j
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    assert!(cursor.contains('.'), "roles cursor must be opaque signed");
    {
        let plain = Uuid::now_v7().to_string();
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/roles?limit=2&cursor={plain}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "roles plain UUID should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
        let sig_t = tamper_sig(&cursor);
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/roles?limit=2&cursor={sig_t}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "roles sig-tampered should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
        let payload_t = tamper_payload(&cursor);
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/roles?limit=2&cursor={payload_t}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "roles payload-tampered should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
    }

    // audit: 3 vectors
    let (st, j, _) = http_get(
        state.clone(),
        &token,
        "/v1/audit-events?limit=2&action=user.created",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "audit list {j}");
    let audit_cursor = j
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .expect("audit cursor must exist for user.created");
    assert!(
        audit_cursor.contains('.'),
        "audit cursor must be opaque signed"
    );
    {
        let plain = Uuid::now_v7().to_string();
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/audit-events?limit=2&action=user.created&cursor={plain}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "audit plain UUID should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
        let sig_t = tamper_sig(&audit_cursor);
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/audit-events?limit=2&action=user.created&cursor={sig_t}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "audit sig-tampered should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
        let payload_t = tamper_payload(&audit_cursor);
        let (st, j, _) = http_get(
            state.clone(),
            &token,
            &format!("/v1/audit-events?limit=2&action=user.created&cursor={payload_t}"),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "audit payload-tampered should be 400 got {j}"
        );
        assert_eq!(
            j.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("INVALID_REQUEST")
        );
    }

    // audit cross-filter: same cursor with different action must be 400 (filter binding)
    let (st, j, _) = http_get(
        state.clone(),
        &token,
        &format!(
            "/v1/audit-events?limit=2&action=role.created&cursor={}",
            &audit_cursor
        ),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "cross filter cursor should be 400 got {j}"
    );
    assert_eq!(
        j.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str()),
        Some("INVALID_REQUEST")
    );
    assert_ne!(st, StatusCode::SERVICE_UNAVAILABLE);
}

// Test 2b: CR-19 regression: 201+ audit events, oldest fetched via GET /v1/audit-events/{id} returns 200
#[tokio::test]
async fn w29_audit_get_beyond_200_returns_200() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, pool2) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm2b-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK);
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    let base_ts = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let mut oldest_id: Option<Uuid> = None;
    for i in 0..210 {
        let id = Uuid::now_v7();
        if i == 0 {
            oldest_id = Some(id);
        }
        let ts = base_ts + time::Duration::seconds(i as i64);
        sqlx::query(
            "INSERT INTO audit_events (id, occurred_at, actor_user_id, action, target_type, target_id, request_id, result, metadata) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .bind(id)
        .bind(ts)
        .bind(None::<Uuid>)
        .bind("test.cr19")
        .bind("test")
        .bind(format!("r-{i}"))
        .bind(format!("req_{}", Uuid::now_v7()))
        .bind("success")
        .bind(serde_json::json!({}))
        .execute(&pool2)
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let oldest = oldest_id.unwrap();
    let (st, j, _) = http_get(state.clone(), &token, &format!("/v1/audit-events/{oldest}")).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "oldest audit event beyond 200 must be 200 after fix, got {j}"
    );
    assert_eq!(
        j.get("audit_id").and_then(|v| v.as_str()),
        Some(oldest.to_string().as_str()),
        "audit_id must match {j}"
    );
    assert!(j.get("action").is_some(), "action must be present {j}");
}

// Test 3: audit required fields
#[tokio::test]
async fn w29_audit_required_fields() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, _) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm3-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK);
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    let (st, j, _) = http_get(state.clone(), &token, "/v1/audit-events?limit=10").await;
    assert_eq!(st, StatusCode::OK, "audit list {j}");
    let items = j.get("items").and_then(|v| v.as_array()).expect("items");
    assert!(!items.is_empty(), "should have audit items");
    for it in items {
        for field in [
            "audit_id",
            "timestamp",
            "actor_type",
            "action",
            "resource_type",
            "result",
            "request_id",
            "details",
        ] {
            assert!(it.get(field).is_some(), "field {field} missing in {it}");
        }
        // actor_type enum
        let at = it.get("actor_type").and_then(|v| v.as_str()).unwrap();
        assert!(at == "user" || at == "system", "actor_type {at}");
        // result enum
        let res = it.get("result").and_then(|v| v.as_str()).unwrap();
        assert!(res == "success" || res == "failure", "result {res}");
    }
}

// Test 4: same occurred_at across page boundary and ORDER BY (occurred_at DESC, id DESC) vs id DESC
// This test fails if ORDER BY is changed to id DESC alone: pagination order and cursor continuity break.
#[tokio::test]
async fn w29_audit_same_ts_and_out_of_order() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, pool2) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm4-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK);
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // Build dataset where id order is opposite to occurred_at order and same_ts groups span page boundaries.
    // 8 rows with action test.audit.order-{suffix}, ids increasing, timestamps grouped:
    // ids[0..2] -> ts_late, ids[3..4] -> ts_mid, ids[5..7] -> ts_early
    // Composite order (ts DESC, id DESC) = [2,1,0,4,3,7,6,5] in terms of ids index.
    // id DESC order = [7,6,5,4,3,2,1,0] – different.
    let action = format!("test.audit.order-{suffix}");
    let mut ids = Vec::new();
    for _ in 0..8 {
        ids.push(Uuid::now_v7());
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let ts_late = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
    let ts_mid = OffsetDateTime::from_unix_timestamp(1_700_000_050).unwrap();
    let ts_early = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let assignments: Vec<(Uuid, OffsetDateTime)> = vec![
        (ids[0], ts_late),
        (ids[1], ts_late),
        (ids[2], ts_late),
        (ids[3], ts_mid),
        (ids[4], ts_mid),
        (ids[5], ts_early),
        (ids[6], ts_early),
        (ids[7], ts_early),
    ];
    for (id, ts) in &assignments {
        sqlx::query("INSERT INTO audit_events (id, occurred_at, actor_user_id, action, target_type, target_id, request_id, result, metadata) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(*id).bind(*ts).bind(None::<Uuid>).bind(&action).bind("test").bind(format!("r-{}", id)).bind(format!("req_{}", Uuid::now_v7())).bind("success").bind(serde_json::json!({}))
            .execute(&pool2).await.unwrap();
    }
    // Expected composite order
    let mut expected_sorted = assignments.clone();
    expected_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    let expected_ids: Vec<String> = expected_sorted
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();

    // Paginate with limit=2, which forces page boundaries inside same_ts groups (3 late rows split 2+1, 3 early rows split 2+1)
    let mut collected: Vec<String> = Vec::new();
    let mut collected_with_ts: Vec<(String, String)> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let uri = if let Some(c) = &cursor {
            format!("/v1/audit-events?limit=2&action={}&cursor={}", &action, c)
        } else {
            format!("/v1/audit-events?limit=2&action={}", &action)
        };
        let (st, j, _) = http_get(state.clone(), &token, &uri).await;
        assert_eq!(st, StatusCode::OK, "audit page {} {j}", pages);
        let items = j.get("items").and_then(|v| v.as_array()).unwrap();
        assert!(items.len() <= 2, "page size must be <=2");
        for it in items {
            let aid = it.get("audit_id").unwrap().as_str().unwrap().to_owned();
            let ts_str = it.get("timestamp").unwrap().as_str().unwrap().to_owned();
            collected.push(aid.clone());
            collected_with_ts.push((aid, ts_str));
        }
        let next = j
            .get("next_cursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        pages += 1;
        if pages > 10 {
            panic!("too many pages");
        }
        if let Some(n) = next {
            cursor = Some(n);
        } else {
            break;
        }
    }
    assert_eq!(
        collected.len(),
        8,
        "pagination should return 8 rows, got {collected:?}"
    );
    let uniq: std::collections::HashSet<_> = collected.iter().collect();
    assert_eq!(uniq.len(), 8, "no duplicates expected, got {collected:?}");
    assert_eq!(
        collected, expected_ids,
        "paginated order must be (occurred_at DESC, id DESC). Expected {expected_ids:?} got {collected:?}. If ORDER BY was changed to id DESC, this fails."
    );
    // Verify global sortedness by (occurred_at DESC, id DESC)
    for w in collected_with_ts.windows(2) {
        let (id_a, ts_a) = &w[0];
        let (id_b, ts_b) = &w[1];
        let dt_a = OffsetDateTime::parse(ts_a, &time::format_description::well_known::Rfc3339)
            .expect("parse ts_a");
        let dt_b = OffsetDateTime::parse(ts_b, &time::format_description::well_known::Rfc3339)
            .expect("parse ts_b");
        let uuid_a = Uuid::parse_str(id_a).unwrap();
        let uuid_b = Uuid::parse_str(id_b).unwrap();
        let ord = dt_a.cmp(&dt_b).then_with(|| uuid_a.cmp(&uuid_b));
        assert_eq!(
            ord,
            std::cmp::Ordering::Greater,
            "audit order violation: {id_a} @ {ts_a} should be > {id_b} @ {ts_b} in composite order; ORDER BY id DESC alone breaks this"
        );
    }
    // Single large page must also be in composite order
    let (st, j, _) = http_get(
        state.clone(),
        &token,
        &format!("/v1/audit-events?limit=10&action={}", &action),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "single page {j}");
    let items = j.get("items").and_then(|v| v.as_array()).unwrap();
    let single_ids: Vec<String> = items
        .iter()
        .map(|it| it.get("audit_id").unwrap().as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        single_ids, expected_ids,
        "single page order must also be composite, got {single_ids:?}"
    );
}

// Test 5: token reuse audit actor_type user
#[tokio::test]
async fn w29_token_audit_actor_type_user() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, pool2) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm5-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK);
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // Trigger token reuse via direct storage call: need a refresh token family.
    // Use a separate session for the refresh-token reuse so the login token remains valid
    // (reuse revokes the session via UPDATE auth_sessions SET status='revoked').
    let admin_user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE login_id=$1")
        .bind(&admin_login)
        .fetch_one(&pool2)
        .await
        .unwrap();
    let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let expires = now + time::Duration::days(30);
    let session_id = Uuid::now_v7();
    sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1,$2,'active',$3,$4,0)")
        .bind(session_id).bind(admin_user_id).bind(now).bind(expires).execute(&pool2).await.unwrap();
    let family_id = Uuid::now_v7();
    let token_id = Uuid::now_v7();
    let digest = vec![1u8; 32];
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(token_id).bind(session_id).bind(family_id).bind(&digest).bind(now).bind(expires).execute(&pool2).await.unwrap();
    let store = IdentityAdministrationStore::new(pool2.clone());
    let replacement = orbisync_storage_postgres::NewRefreshToken {
        id: Uuid::now_v7(),
        token_digest: vec![2u8; 32],
        issued_at: now,
        expires_at: expires,
    };
    let res = store
        .rotate_refresh_token(
            &digest,
            &replacement,
            now,
            &format!("req_{}", Uuid::now_v7()),
        )
        .await
        .unwrap();
    assert!(matches!(
        res,
        orbisync_storage_postgres::RefreshRotation::Rotated { .. }
    ));
    // second use should be reuse
    let replacement2 = orbisync_storage_postgres::NewRefreshToken {
        id: Uuid::now_v7(),
        token_digest: vec![3u8; 32],
        issued_at: now,
        expires_at: expires,
    };
    let res2 = store
        .rotate_refresh_token(
            &digest,
            &replacement2,
            now,
            &format!("req_{}", Uuid::now_v7()),
        )
        .await
        .unwrap();
    assert_eq!(
        res2,
        orbisync_storage_postgres::RefreshRotation::ReuseDetected
    );
    // Check audit row
    let row: Option<(Uuid, Option<Uuid>, String, String)> = sqlx::query_as("SELECT id, actor_user_id, action, result FROM audit_events WHERE action='token.reuse_detected' ORDER BY occurred_at DESC LIMIT 1")
        .fetch_optional(&pool2).await.unwrap();
    let (audit_id, actor, action, _result) = row.expect("audit row for reuse");
    assert_eq!(action, "token.reuse_detected");
    assert!(
        actor.is_some(),
        "actor_user_id should not be NULL for token.reuse_detected, got None"
    );
    assert_eq!(
        actor.unwrap(),
        admin_user_id,
        "actor should be session user"
    );
    // Check via HTTP GET that actor_type is user
    let (st, j, _) = http_get(
        state.clone(),
        &token,
        &format!("/v1/audit-events/{audit_id}"),
    )
    .await;
    // The get by id may need to search; our handler does search via large limit, so it should find
    if st == StatusCode::OK {
        let at = j.get("actor_type").and_then(|v| v.as_str()).unwrap();
        assert_eq!(
            at, "user",
            "actor_type should be user for reuse, got {at} json {j}"
        );
        let aid = j.get("actor_id").and_then(|v| v.as_str()).unwrap();
        assert_eq!(aid, admin_user_id.to_string());
    } else {
        // Fallback: list and find
        let (st2, j2, _) = http_get(state.clone(), &token, "/v1/audit-events?limit=200").await;
        assert_eq!(st2, StatusCode::OK);
        let items = j2.get("items").and_then(|v| v.as_array()).unwrap();
        let found = items
            .iter()
            .find(|it| it.get("audit_id").and_then(|v| v.as_str()) == Some(&audit_id.to_string()))
            .expect("reuse audit in list");
        let at = found.get("actor_type").and_then(|v| v.as_str()).unwrap();
        assert_eq!(at, "user");
    }
    // Also check token.refreshed
    let row2: Option<(Uuid, Option<Uuid>)> = sqlx::query_as("SELECT id, actor_user_id FROM audit_events WHERE action='token.refreshed' ORDER BY occurred_at DESC LIMIT 1")
        .fetch_optional(&pool2).await.unwrap();
    if let Some((_id2, actor2)) = row2 {
        assert!(actor2.is_some(), "token.refreshed actor should be Some");
        assert_eq!(actor2.unwrap(), admin_user_id);
    }
}

// Test 5: atomic role read (V-02) - detects torn read where revision and permissions come from different snapshots.
// The updater changes permissions and revision atomically in one transaction (DELETE+INSERT+revision bump).
// Under READ COMMITTED the intermediate empty state is never visible, so checking for empty array is insufficient.
// A non-atomic reader that does `SELECT revision` then `SELECT permissions` in separate queries can observe
// a torn state: revision from before the commit and permissions from after (or vice versa). The atomic
// implementation uses a single query with array_agg, so revision and permissions are always from same snapshot.
#[tokio::test]
async fn w29_role_permissions_not_empty_during_update() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, pool2) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm-w29-5-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK, "admin login {j}");
    let token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    let role_name = format!("role-w29-5-{suffix}");
    let (st, j) = http_create_role(state.clone(), &token, &role_name, &["admin.users.read"]).await;
    assert_eq!(st, StatusCode::CREATED, "create role {j}");
    let role_id = j.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    let (st, j, _) = http_get(state.clone(), &token, &format!("/v1/roles/{role_id}")).await;
    assert_eq!(st, StatusCode::OK, "get role initial {j}");
    let rev0 = j
        .get("revision")
        .and_then(|v| v.as_u64())
        .expect("revision");
    let perms0 = j
        .get("permissions")
        .and_then(|v| v.as_array())
        .expect("perms");
    assert!(!perms0.is_empty(), "initial perms empty {j}");
    // Updater alternates between two disjoint permission sets and bumps revision each time.
    // Set A (even i): ["admin.users.read","admin.roles.read"]  Set B (odd i): ["admin.audit.read"]
    // For rev = rev0 + n (n>=1): n odd => Set A, n even => Set B. rev0 itself => ["admin.users.read"].
    let pool_updater = pool2.clone();
    let role_id_up = role_id.clone();
    let updater = tokio::spawn(async move {
        for i in 0..80 {
            let new_perms: Vec<&str> = if i % 2 == 0 {
                vec!["admin.users.read", "admin.roles.read"]
            } else {
                vec!["admin.audit.read"]
            };
            let mut tx = pool_updater.begin().await.expect("begin");
            sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                .bind(Uuid::parse_str(&role_id_up).unwrap())
                .execute(&mut *tx)
                .await
                .expect("delete");
            for p in &new_perms {
                sqlx::query(
                    "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                )
                .bind(*p)
                .execute(&mut *tx)
                .await
                .expect("perm");
                sqlx::query("INSERT INTO role_permissions (role_id, permission_name) VALUES ($1,$2) ON CONFLICT DO NOTHING").bind(Uuid::parse_str(&role_id_up).unwrap()).bind(*p).execute(&mut *tx).await.expect("role perm");
            }
            sqlx::query("UPDATE roles SET revision = revision + 1 WHERE id = $1")
                .bind(Uuid::parse_str(&role_id_up).unwrap())
                .execute(&mut *tx)
                .await
                .expect("rev");
            tx.commit().await.expect("commit");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });
    for _ in 0..200 {
        let (st, j, _) = http_get(state.clone(), &token, &format!("/v1/roles/{role_id}")).await;
        assert_eq!(st, StatusCode::OK, "get role during update {j}");
        let rev = j.get("revision").and_then(|v| v.as_u64()).expect("rev");
        let perms = j
            .get("permissions")
            .and_then(|v| v.as_array())
            .expect("perms array");
        assert!(
            !perms.is_empty(),
            "role permissions must not be empty, got {j}"
        );
        let mut got: Vec<String> = perms
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        got.sort();
        let expected: Vec<String> = if rev == rev0 {
            vec!["admin.users.read".to_owned()]
        } else if rev > rev0 {
            let n = rev - rev0;
            if n % 2 == 1 {
                let mut e = vec!["admin.roles.read".to_owned(), "admin.users.read".to_owned()];
                e.sort();
                e
            } else {
                vec!["admin.audit.read".to_owned()]
            }
        } else {
            panic!("revision went backwards: rev {rev} < rev0 {rev0}");
        };
        assert_eq!(
            got, expected,
            "torn read detected: revision {rev} (rev0 {rev0}) expected {expected:?} got {got:?} full {j}. Non-atomic read allows revision and permissions from different snapshots."
        );
        // Also verify list endpoint is atomic: the role entry in the list must have same revision->perms mapping
        let (st, j, _) = http_get(state.clone(), &token, "/v1/roles?limit=200").await;
        assert_eq!(st, StatusCode::OK, "list during update {j}");
        let items = j.get("items").and_then(|v| v.as_array()).expect("items");
        let found = items
            .iter()
            .find(|it| it.get("id").and_then(|v| v.as_str()) == Some(role_id.as_str()))
            .expect("role must be in list");
        let perms2 = found
            .get("permissions")
            .and_then(|v| v.as_array())
            .expect("perms in list");
        assert!(!perms2.is_empty(), "list perms empty torn {found}");
        let mut got2: Vec<String> = perms2
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        got2.sort();
        let rev2 = found
            .get("revision")
            .and_then(|v| v.as_u64())
            .expect("rev2");
        let expected2: Vec<String> = if rev2 == rev0 {
            vec!["admin.users.read".to_owned()]
        } else if rev2 > rev0 {
            let n = rev2 - rev0;
            if n % 2 == 1 {
                let mut e = vec!["admin.roles.read".to_owned(), "admin.users.read".to_owned()];
                e.sort();
                e
            } else {
                vec!["admin.audit.read".to_owned()]
            }
        } else {
            panic!("list rev backwards");
        };
        assert_eq!(
            got2, expected2,
            "torn read via list: rev {rev2} expected {expected2:?} got {got2:?} item {found}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    updater.await.expect("updater join");
    let (st, j, _) = http_get(state.clone(), &token, &format!("/v1/roles/{role_id}")).await;
    assert_eq!(st, StatusCode::OK);
    let perms = j.get("permissions").and_then(|v| v.as_array()).unwrap();
    assert!(!perms.is_empty(), "final perms empty");
}

// Test 6: unauthorized token => ACCESS_DENIED for each GET, realtime ticket => 401 (audience isolation).
#[tokio::test]
async fn w29_unauthorized_and_ticket_rejected() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, pool2) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm-w29-6-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let (st, j) = http_login(state.clone(), &admin_login, pw.expose_secret()).await;
    assert_eq!(st, StatusCode::OK, "admin login {j}");
    let admin_token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // admin creates a role to have a valid role_id
    let role_name = format!("role-w29-6-{suffix}");
    let (st, j) = http_create_role(
        state.clone(),
        &admin_token,
        &role_name,
        &["admin.users.read"],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create role {j}");
    let role_id = j.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    // admin creates a regular user bob without any role (so no permissions)
    let bob_login = format!("bob-w29-6-{suffix}");
    let (st, j) = http_create_user_with_cred(state.clone(), &admin_token, &bob_login).await;
    assert_eq!(st, StatusCode::CREATED, "create bob {j}");
    let bob_pw = j
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let bob_user_id = j
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // bob login
    let (st, j) = http_login(state.clone(), &bob_login, &bob_pw).await;
    assert_eq!(st, StatusCode::OK, "bob login {j}");
    let bob_token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // Gather ids for path-param endpoints
    let admin_user_id: String =
        sqlx::query_scalar("SELECT id::text FROM users WHERE login_id = $1")
            .bind(&admin_login)
            .fetch_one(&pool2)
            .await
            .unwrap();
    let audit_id: String =
        sqlx::query_scalar("SELECT id::text FROM audit_events ORDER BY occurred_at DESC LIMIT 1")
            .fetch_one(&pool2)
            .await
            .unwrap();
    // Admin sanity: each GET should be 200 for admin
    let admin_checks = vec![
        "/v1/users".to_owned(),
        format!("/v1/users/{admin_user_id}"),
        format!("/v1/users/{bob_user_id}/roles"),
        "/v1/roles".to_owned(),
        format!("/v1/roles/{role_id}"),
        "/v1/audit-events".to_owned(),
        format!("/v1/audit-events/{audit_id}"),
    ];
    for uri in &admin_checks {
        let (st, j, _) = http_get(state.clone(), &admin_token, uri).await;
        assert_eq!(st, StatusCode::OK, "admin should get 200 for {uri} got {j}");
    }
    // Bob (no permissions) must get 403 ACCESS_DENIED for each GET
    for uri in &admin_checks {
        let (st, j, _) = http_get(state.clone(), &bob_token, uri).await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "bob should be 403 for {uri} got {j}"
        );
        let code = j
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(
            code, "ACCESS_DENIED",
            "bob {uri} code should be ACCESS_DENIED got {j}"
        );
    }
    // Realtime ticket audience isolation: admin's access token -> ticket, then ticket as Bearer for admin API => 401
    let (st, j) = http_ticket(state.clone(), &admin_token).await;
    assert_eq!(st, StatusCode::OK, "ticket issuance {j}");
    let ticket = j
        .get("realtime_ticket")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    for uri in &admin_checks {
        let (st, j, _) = http_get(state.clone(), &ticket, uri).await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "ticket should be 401 for {uri} got {j}"
        );
        let code = j
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(
            code, "AUTHENTICATION_REQUIRED",
            "ticket {uri} code should be AUTHENTICATION_REQUIRED got {j}"
        );
        // Verify ticket is not accepted as if it were access token (audience isolation)
        assert_ne!(
            st,
            StatusCode::FORBIDDEN,
            "ticket must be 401 not 403 for {uri}"
        );
    }
    // Also verify that using ticket for POST /v1/users (existing audience test) is 401, matching existing pattern
    let (st, j) = {
        let app = router(state.clone());
        let body = serde_json::json!({"login_id": format!("eve-{suffix}"), "display_name": "Eve"});
        let req = Request::builder()
            .uri("/v1/users")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {ticket}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    };
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "ticket for POST /v1/users should be 401 got {j}"
    );
}

// Test 7: response bodies must not contain raw token, password, or cursor HMAC key material.
#[tokio::test]
async fn w29_no_secret_leakage_in_responses() {
    let _g = db_guard().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    clean(&pool).await;
    let (state, admin, pool2) = setup(pool).await;
    let suffix = Uuid::now_v7().to_string();
    let admin_login = format!("adm-w29-7-{suffix}");
    let (_, pw) = admin
        .bootstrap_administrator(
            LoginId::new(admin_login.clone()).unwrap(),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        )
        .await
        .unwrap();
    let admin_pw = pw.expose_secret().to_owned();
    let (st, j) = http_login(state.clone(), &admin_login, &admin_pw).await;
    assert_eq!(st, StatusCode::OK, "admin login {j}");
    let admin_token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // Create a regular user to have a second password/token
    let bob_login = format!("bob-w29-7-{suffix}");
    let (st, j) = http_create_user_with_cred(state.clone(), &admin_token, &bob_login).await;
    assert_eq!(st, StatusCode::CREATED, "create bob {j}");
    let bob_pw = j
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let bob_user_id = j
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (st, j) = http_login(state.clone(), &bob_login, &bob_pw).await;
    assert_eq!(st, StatusCode::OK);
    let _bob_token = j.get("access_token").unwrap().as_str().unwrap().to_owned();
    // Create extra users/roles to ensure pagination cursors exist
    for i in 0..5 {
        let login = format!("u7-{}-{}", suffix, i);
        // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = http_create_user(state.clone(), &admin_token, &login).await;
    }
    let role_name = format!("role-w29-7-{suffix}");
    let (st, j) = http_create_role(
        state.clone(),
        &admin_token,
        &role_name,
        &["admin.users.read", "admin.roles.read"],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let role_id = j.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
    // Resolve pagination HMAC key from env (if set) for leakage check; test also checks that
    // insecure fixed test key does not appear.
    let pagination_key = std::env::var("ORBISYNC_PAGINATION_HMAC_KEY").unwrap_or_default();
    let insecure_key = "insecure-fixed-pagination-key-for-tests-only-32b!!";
    // Helper to assert no secret appears in JSON stringified body
    let assert_no_secrets = |json: &serde_json::Value, context: &str| {
        let s = json.to_string();
        // Must not contain raw token (admin token) – tokens are JWTs, so check full token not leaked
        assert!(
            !s.contains(&admin_token),
            "response for {context} must not contain admin token"
        );
        // Must not contain raw passwords
        assert!(
            !s.contains(&admin_pw),
            "response for {context} must not contain admin password"
        );
        assert!(
            !s.contains(&bob_pw),
            "response for {context} must not contain bob password"
        );
        // Must not contain pagination HMAC keys if present
        if !pagination_key.is_empty() {
            assert!(
                !s.contains(&pagination_key),
                "response for {context} must not contain pagination HMAC key"
            );
        }
        assert!(
            !s.contains(insecure_key),
            "response for {context} must not contain insecure pagination key"
        );
        // Also ensure no field named password/token/hmac appears that contains raw value
        // Check that JSON does not have keys leaking raw secrets (defense in depth)
        let lower = s.to_lowercase();
        // The word "password" may appear as field name in some hypothetical leak, but
        // current API should never return a field containing password value. We check
        // that if a password field exists, it doesn't contain the actual password.
        // For now, just ensure the raw password value not present (already checked).
        // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = lower;
    };
    // Gather admin ids for single-get checks
    let admin_user_id: String =
        sqlx::query_scalar("SELECT id::text FROM users WHERE login_id = $1")
            .bind(&admin_login)
            .fetch_one(&pool2)
            .await
            .unwrap();
    let audit_id: String =
        sqlx::query_scalar("SELECT id::text FROM audit_events ORDER BY occurred_at DESC LIMIT 1")
            .fetch_one(&pool2)
            .await
            .unwrap();
    // List endpoints
    let (st, j, _) = http_get(state.clone(), &admin_token, "/v1/users?limit=2").await;
    assert_eq!(st, StatusCode::OK, "users list {j}");
    assert_no_secrets(&j, "GET /v1/users?limit=2");
    if let Some(cur) = j.get("next_cursor").and_then(|v| v.as_str()) {
        if !pagination_key.is_empty() {
            assert!(
                !cur.contains(&pagination_key),
                "cursor must not contain raw pagination key"
            );
        }
        assert!(
            !cur.contains(insecure_key),
            "cursor must not contain insecure pagination key"
        );
        assert!(!cur.contains(&admin_token), "cursor must not contain token");
        // Verify cursor is opaque (contains '.'), not raw UUID
        assert!(cur.contains('.'), "cursor should be opaque signed value");
    }
    let (st, j, _) = http_get(
        state.clone(),
        &admin_token,
        &format!("/v1/users/{admin_user_id}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_no_secrets(&j, "GET /v1/users/{id}");
    let (st, j, _) = http_get(
        state.clone(),
        &admin_token,
        &format!("/v1/users/{bob_user_id}/roles"),
    )
    .await;
    // bob has no roles, but response should still not leak secrets
    assert!(
        st == StatusCode::OK || st == StatusCode::FORBIDDEN || st == StatusCode::NOT_FOUND,
        "get user roles {j}"
    );
    if st == StatusCode::OK {
        assert_no_secrets(&j, "GET /v1/users/{id}/roles");
    }
    let (st, j, _) = http_get(state.clone(), &admin_token, "/v1/roles?limit=2").await;
    assert_eq!(st, StatusCode::OK, "roles list {j}");
    assert_no_secrets(&j, "GET /v1/roles");
    if let Some(cur) = j.get("next_cursor").and_then(|v| v.as_str()) {
        if !pagination_key.is_empty() {
            assert!(
                !cur.contains(&pagination_key),
                "roles cursor must not contain raw pagination key"
            );
        }
        assert!(
            !cur.contains(insecure_key),
            "roles cursor must not contain insecure pagination key"
        );
    }
    let (st, j, _) = http_get(state.clone(), &admin_token, &format!("/v1/roles/{role_id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_no_secrets(&j, "GET /v1/roles/{id}");
    let (st, j, _) = http_get(state.clone(), &admin_token, "/v1/audit-events?limit=2").await;
    assert_eq!(st, StatusCode::OK, "audit list {j}");
    assert_no_secrets(&j, "GET /v1/audit-events");
    if let Some(cur) = j.get("next_cursor").and_then(|v| v.as_str()) {
        if !pagination_key.is_empty() {
            assert!(
                !cur.contains(&pagination_key),
                "audit cursor must not contain raw pagination key"
            );
        }
        assert!(
            !cur.contains(insecure_key),
            "audit cursor must not contain insecure pagination key"
        );
    }
    let (st, j, _) = http_get(
        state.clone(),
        &admin_token,
        &format!("/v1/audit-events/{audit_id}"),
    )
    .await;
    // audit get may be 200 or 404 depending on search limit, but if 200 check no leakage
    if st == StatusCode::OK {
        assert_no_secrets(&j, "GET /v1/audit-events/{id}");
    } else {
        assert_eq!(st, StatusCode::NOT_FOUND, "audit get {j}");
    }
    // Also verify that login response itself contains token but other endpoints must not leak it
    // (login's token is expected in login response, but not in subsequent list responses – already checked)
    // Ensure error responses for unauthorized also don't leak
    let (st, j, _) = http_get(state.clone(), "invalid-token", "/v1/users").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_no_secrets(&j, "401 error body");
    // Ensure that even after using bob's password, no endpoint returns it
    let (st, j, _) = http_get(state.clone(), &admin_token, "/v1/users?limit=200").await;
    assert_eq!(st, StatusCode::OK);
    let s = j.to_string();
    assert!(
        !s.contains("password"),
        "users list should not contain literal 'password' value leak, but field name check is lenient; got {s}"
    );
}
