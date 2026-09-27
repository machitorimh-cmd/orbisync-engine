//! CR-H: POST /v1/auth/refresh wiring + CR-20 rejected audit (HTTP + real DB)

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, RefreshTokenCreationStore, SecretString};
use orbisync_domain::{Credential, LoginId, Timestamp, User, UserId};
use orbisync_identity::{LoginService, PasswordPolicy, PasswordService, token::AccessTokenService};
use orbisync_storage_postgres::{
    IdentityAdministrationStore, PgIdentityRepository, PgRealtimeTicketStore,
};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::HttpState;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt as _;
use uuid::Uuid;

mod common;

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

async fn pool() -> Option<PgPool> {
    common::pool_or_skip().await
}

fn fixed_now() -> (Timestamp, OffsetDateTime) {
    let ts = Timestamp::from_unix_millis(1_700_000_000_000).expect("ts");
    (ts, ts.as_offset_date_time())
}

#[allow(dead_code)]
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
        b"test-refresh-hmac-key-32bytes!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn test_login_service_pg_with_ttl(
    pool: PgPool,
    repo: Arc<dyn IdentityRepository>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
    refresh_ttl: u64,
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
        b"test-refresh-hmac-key-32bytes!!".to_vec(),
        900,
        refresh_ttl,
    ))
}

fn fixed_clock(ts: Timestamp) -> Arc<FixedClock> {
    Arc::new(FixedClock::new(ts))
}

async fn insert_user(pool: &PgPool, user: &User, cred: &Credential) {
    sqlx::query(
        "INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, 'active', $4, $5, $6, $7)",
    )
    .bind(user.id().as_uuid())
    .bind(user.login_id().as_str())
    .bind(user.display_name())
    .bind(user.must_change_password())
    .bind(i64::try_from(user.revision().as_u64()).expect("rev"))
    .bind(user.created_at().as_offset_date_time())
    .bind(user.updated_at().as_offset_date_time())
    .execute(pool)
    .await
    .expect("insert user");
    sqlx::query(
        "INSERT INTO user_credentials (user_id, password_hash, password_changed_at, failed_login_count, locked_until) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(cred.user_id().as_uuid())
    .bind(cred.password_hash().expose_phc())
    .bind(cred.password_changed_at().as_offset_date_time())
    .bind(i32::try_from(cred.failed_login_count()).expect("count"))
    .bind(cred.locked_until().map(|t| t.as_offset_date_time()))
    .execute(pool)
    .await
    .expect("insert cred");
}

async fn http_state(pool: PgPool, clock: Arc<FixedClock>, refresh_ttl: u64) -> HttpState {
    use orbisync_domain::Clock;

    let tokens = Arc::new(
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("token service")
        .with_access_token_ttl(900),
    );
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo: Arc<dyn IdentityRepository> = Arc::new(PgIdentityRepository::new(pool.clone()));
    let rotation_store: Arc<dyn orbisync_application::RefreshTokenRotationStore> =
        Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let creation_store: Arc<dyn RefreshTokenCreationStore> =
        Arc::new(IdentityAdministrationStore::new(pool.clone()));
    // Use pagination HMAC key env value equivalent for tests: "test-hmac-key-32bytes!!"
    let hmac_key = b"test-refresh-hmac-key-32bytes!!".to_vec();
    let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
        Arc::new(PgRealtimeTicketStore::new(pool.clone()));
    HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        refresh_ttl,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_login_service(test_login_service_pg_with_ttl(
        pool.clone(),
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
        refresh_ttl,
    ))
    .with_identity_repository(repo)
    .with_password_service(passwords)
    .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    .with_realtime_ticket_store(ticket_store)
    .with_refresh_token_hmac_key(hmac_key)
    .with_refresh_creation_store(creation_store)
    .with_refresh_rotation_store(rotation_store)
}

async fn http_login(
    state: HttpState,
    login_id: &str,
    password: &str,
) -> (StatusCode, serde_json::Value) {
    let app = orbisync_transport_http::router(state);
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
    let app = orbisync_transport_http::router(state);
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

async fn http_realtime_ticket(state: HttpState, token: &str) -> StatusCode {
    let app = orbisync_transport_http::router(state);
    let req = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    resp.status()
}

// ---------------------------------------------------------------------------
// 1. login refresh token -> 200 token pair
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_login_refresh_returns_200() {
    let Some(pool) = pool().await else { return };
    let (now_ts, _) = fixed_now();
    let clock = fixed_clock(now_ts);
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let user_id = UserId::generate();
    let login = format!("crh1-{}", Uuid::now_v7());
    let pw = "Cedar!Lake7-Comet";
    let hash = passwords.hash(SecretString::new(pw)).await.expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "CRH1",
        now_ts,
    )
    .expect("user");
    let cred = Credential::new(user_id, hash, now_ts);
    insert_user(&pool, &user, &cred).await;
    let state = http_state(pool.clone(), clock, 2_592_000).await;
    let (status, json) = http_login(state.clone(), &login, pw).await;
    assert_eq!(status, StatusCode::OK, "login must be 200 {json}");
    let refresh = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .expect("refresh_token");
    assert!(!refresh.is_empty());
    let access = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .expect("access_token");
    // Now refresh
    let (status2, json2) = http_refresh(state.clone(), refresh).await;
    assert_eq!(status2, StatusCode::OK, "refresh must be 200 {json2}");
    assert!(json2.get("access_token").is_some());
    assert!(json2.get("refresh_token").is_some());
    let new_refresh = json2.get("refresh_token").and_then(|v| v.as_str()).unwrap();
    assert_ne!(refresh, new_refresh, "rotated refresh must differ");
    let new_access = json2.get("access_token").and_then(|v| v.as_str()).unwrap();
    assert_ne!(access, new_access);
}

// ---------------------------------------------------------------------------
// 2. same refresh twice -> second 401 + reuse_detected audit
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_reuse_second_is_401_and_audited() {
    let Some(pool) = pool().await else { return };
    let (now_ts, _) = fixed_now();
    let clock = fixed_clock(now_ts);
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let user_id = UserId::generate();
    let login = format!("crh2-{}", Uuid::now_v7());
    let pw = "Cedar!Lake7-Comet";
    let hash = passwords.hash(SecretString::new(pw)).await.expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "CRH2",
        now_ts,
    )
    .expect("user");
    let cred = Credential::new(user_id, hash, now_ts);
    insert_user(&pool, &user, &cred).await;
    let state = http_state(pool.clone(), clock, 2_592_000).await;
    let (s, j) = http_login(state.clone(), &login, pw).await;
    assert_eq!(s, StatusCode::OK, "{j}");
    let refresh = j
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // first refresh
    let (s1, _) = http_refresh(state.clone(), &refresh).await;
    assert_eq!(s1, StatusCode::OK);
    // second with same token
    let (s2, _) = http_refresh(state.clone(), &refresh).await;
    assert_eq!(s2, StatusCode::UNAUTHORIZED, "second use must be 401");
    // audit row for reuse_detected for this user/session
    // Find session id via audit target_id
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, result FROM audit_events WHERE action='token.reuse_detected' AND result='failure' ORDER BY occurred_at DESC LIMIT 5",
    )
    .fetch_all(&pool)
    .await
    .expect("audit");
    assert!(
        rows.iter()
            .any(|(a, r)| a == "token.reuse_detected" && r == "failure"),
        "reuse audit must exist {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. unknown token -> 401 + token.rejected audit
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_unknown_is_401_and_audited() {
    let Some(pool) = pool().await else { return };
    let (now_ts, _) = fixed_now();
    let clock = fixed_clock(now_ts);
    let state = http_state(pool.clone(), clock, 2_592_000).await;
    // count before
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='token.rejected'")
            .fetch_one(&pool)
            .await
            .expect("count");
    let fake = "unknown-refresh-token-not-in-db-0001";
    let (status, _) = http_refresh(state, fake).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='token.rejected' AND result='failure'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert!(
        after > before,
        "unknown token must create token.rejected audit"
    );
}

// ---------------------------------------------------------------------------
// 4. expired token -> 401 + audit
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_expired_is_401_and_audited() {
    let Some(pool) = pool().await else { return };
    let (now_ts, now_odt) = fixed_now();
    let past = now_odt - Duration::days(1);
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let family_id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'Expired', 'active', true, 1, $3, $3)")
        .bind(user_id)
        .bind(format!("login-{user_id}"))
        .bind(now_odt)
        .execute(&pool)
        .await
        .expect("user");
    sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1, $2, 'active', $3, $4, 0)")
        .bind(session_id)
        .bind(user_id)
        .bind(now_odt)
        .bind(now_odt + Duration::days(30))
        .execute(&pool)
        .await
        .expect("session");
    // Generate a token and compute digest via same HMAC key as http_state uses
    let raw = orbisync_transport_http::generate_refresh_token()
        .expose_secret()
        .to_owned();
    let key = b"test-refresh-hmac-key-32bytes!!";
    let digest =
        orbisync_transport_http::refresh_token_digest(key, &SecretString::new(raw.clone()));
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(Uuid::now_v7())
        .bind(session_id)
        .bind(family_id)
        .bind(digest.as_slice())
        .bind(past)
        .bind(past + Duration::seconds(10)) // already expired (past +10s < now)
        .execute(&pool)
        .await
        .expect("token");
    let clock = fixed_clock(now_ts);
    let state = http_state(pool.clone(), clock, 2_592_000).await;
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='token.rejected'")
            .fetch_one(&pool)
            .await
            .expect("count");
    let (status, _) = http_refresh(state, &raw).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='token.rejected' AND result='failure'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert!(after > before, "expired must be audited");
}

// ---------------------------------------------------------------------------
// 5. audit rows contain no token/digest
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_audit_contains_no_secret() {
    let Some(pool) = pool().await else { return };
    let (now_ts, _) = fixed_now();
    let clock = fixed_clock(now_ts);
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let user_id = UserId::generate();
    let login = format!("crh5-{}", Uuid::now_v7());
    let pw = "Cedar!Lake7-Comet";
    let hash = passwords.hash(SecretString::new(pw)).await.expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "CRH5",
        now_ts,
    )
    .expect("user");
    let cred = Credential::new(user_id, hash, now_ts);
    insert_user(&pool, &user, &cred).await;
    let state = http_state(pool.clone(), clock, 2_592_000).await;
    let (s, j) = http_login(state.clone(), &login, pw).await;
    assert_eq!(s, StatusCode::OK, "{j}");
    let refresh = j
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (s2, j2) = http_refresh(state.clone(), &refresh).await;
    assert_eq!(s2, StatusCode::OK, "{j2}");
    let new_refresh = j2.get("refresh_token").and_then(|v| v.as_str()).unwrap();
    // Query recent audit rows
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as("SELECT action, metadata FROM audit_events WHERE action IN ('token.refreshed','token.reuse_detected','token.rejected') ORDER BY occurred_at DESC LIMIT 20")
        .fetch_all(&pool)
        .await
        .expect("audit");
    for (action, meta) in rows {
        let s = format!("{action} {}", meta);
        assert!(
            !s.contains(&refresh),
            "audit must not contain raw refresh token"
        );
        assert!(
            !s.contains(new_refresh),
            "audit must not contain new raw token"
        );
        assert!(
            !s.to_lowercase().contains("digest"),
            "audit must not contain digest word"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. old access token remains valid after rotation (session not revoked)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_old_access_token_still_valid_after_rotation() {
    let Some(pool) = pool().await else { return };
    let (now_ts, _) = fixed_now();
    let clock = fixed_clock(now_ts);
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let user_id = UserId::generate();
    let login = format!("crh6-{}", Uuid::now_v7());
    let pw = "Cedar!Lake7-Comet";
    let hash = passwords.hash(SecretString::new(pw)).await.expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "CRH6",
        now_ts,
    )
    .expect("user");
    let cred = Credential::new(user_id, hash, now_ts);
    insert_user(&pool, &user, &cred).await;
    let state = http_state(pool.clone(), clock, 2_592_000).await;
    let (s, j) = http_login(state.clone(), &login, pw).await;
    assert_eq!(s, StatusCode::OK, "{j}");
    let old_access = j
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let refresh = j
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    // rotate
    let (s2, _) = http_refresh(state.clone(), &refresh).await;
    assert_eq!(s2, StatusCode::OK);
    // old access token should still be valid for realtime ticket (session not revoked on normal refresh)
    let status = http_realtime_ticket(state.clone(), &old_access).await;
    // Design: normal rotation does NOT revoke session, so old access token remains valid until its own TTL (15 min).
    // If this were failing with 401, it would mean rotation incorrectly revoked the session.
    assert_eq!(
        status,
        StatusCode::OK,
        "old access token should still be valid after normal rotation"
    );
}

// ---------------------------------------------------------------------------
// 7. ORBISYNC_AUTH_REFRESH_TOKEN_TTL_SECONDS changes expiry
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cr_h_ttl_setting_changes_expiry() {
    let Some(pool) = pool().await else { return };
    let (now_ts, now_odt) = fixed_now();
    // Two states with different TTLs
    let ttl_short = 1_000u64;
    let ttl_long = 10_000u64;
    let clock = fixed_clock(now_ts);
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let mut exps: Vec<OffsetDateTime> = Vec::new();
    for (idx, ttl) in [(0, ttl_short), (1, ttl_long)].iter() {
        let user_id = UserId::generate();
        let login = format!("crh7-{}-{}", idx, Uuid::now_v7());
        let pw = "Cedar!Lake7-Comet";
        let hash = passwords.hash(SecretString::new(pw)).await.expect("hash");
        let user = User::new(
            user_id,
            LoginId::new(login.clone()).expect("login"),
            format!("CRH7-{idx}"),
            now_ts,
        )
        .expect("user");
        let cred = Credential::new(user_id, hash, now_ts);
        insert_user(&pool, &user, &cred).await;
        let state = http_state(pool.clone(), clock.clone(), *ttl).await;
        let (s, _) = http_login(state.clone(), &login, pw).await;
        assert_eq!(s, StatusCode::OK);
        // Check session expires_at is now + ttl
        let expires: OffsetDateTime = sqlx::query_scalar("SELECT expires_at FROM auth_sessions WHERE user_id = $1 ORDER BY created_at DESC LIMIT 1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("session");
        let expected = now_odt + Duration::seconds(*ttl as i64);
        let diff = (expires - expected).whole_seconds().abs();
        assert!(
            diff <= 2,
            "ttl {} should set expires_at {} but got {} diff {}",
            ttl,
            expected,
            expires,
            diff
        );
        // Also check refresh token expires_at
        let token_exp: OffsetDateTime = sqlx::query_scalar("SELECT expires_at FROM refresh_tokens WHERE session_id = (SELECT id FROM auth_sessions WHERE user_id=$1 ORDER BY created_at DESC LIMIT 1) ORDER BY issued_at DESC LIMIT 1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("token");
        let diff2 = (token_exp - expected).whole_seconds().abs();
        assert!(
            diff2 <= 2,
            "refresh token expiry must follow TTL, diff {}",
            diff2
        );
        exps.push(expires);
    }
    assert!(
        exps[1] > exps[0],
        "long TTL must produce later expiry: {:?} vs {:?}",
        exps[1],
        exps[0]
    );
}
