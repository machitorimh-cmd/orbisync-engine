//! RV-B: concurrent login failure lockout (C2) – real PostgreSQL via HTTP router.
//!
//! Tests 1..4 from `scratchpad/tasks/RV-B.md`:
//! 1. concurrent wrong-password HTTP logins increment atomically and trigger lockout
//! 2. success immediately after a concurrent failure does not wipe that failure (optimistic predicate)
//! 3. while locked, correct password is rejected
//! 4. after lock expiry, correct password succeeds

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, SecretString};
use orbisync_domain::{Clock, Credential, LoginId, Timestamp, User};
use orbisync_identity::{LoginService, PasswordPolicy, PasswordService, token::AccessTokenService};
use orbisync_storage_postgres::{IdentityAdministrationStore, PgIdentityRepository, PgLoginStore};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use sqlx::PgPool;
use tower::ServiceExt as _;

mod common;

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
    pool: PgPool,
    repo: Arc<dyn IdentityRepository>,
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
        Arc::clone(&clock) as Arc<dyn Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn now_ts() -> Timestamp {
    Timestamp::from_unix_millis(1_700_000_000_000).expect("ts")
}

async fn database() -> Option<PgPool> {
    common::pool_or_skip().await
}

async fn http_login(state: HttpState, login_id: &str, password: &str) -> StatusCode {
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
    // Drain body to avoid connection leak
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = resp.into_body().collect().await;
    status
}

async fn http_login_retry(state: HttpState, login_id: &str, password: &str) -> StatusCode {
    // PasswordService rate-limits concurrent Argon2 (429). Retry with backoff
    // so that every logical failure is eventually recorded (otherwise lockout
    // would be under-counted). This keeps the concurrent shape while staying
    // within the verifier budget.
    for attempt in 0..5 {
        let status = http_login(state.clone(), login_id, password).await;
        if status != StatusCode::TOO_MANY_REQUESTS {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100 * (1 << attempt))).await;
    }
    StatusCode::TOO_MANY_REQUESTS
}

async fn insert_user(pool: &PgPool, user: &User, cred: &Credential) {
    sqlx::query(
        "INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, 'active', true, 1, $4, $4)",
    )
    .bind(user.id().as_uuid())
    .bind(user.login_id().as_str())
    .bind(user.display_name())
    .bind(user.created_at().as_offset_date_time())
    .execute(pool)
    .await
    .expect("insert user");
    sqlx::query(
        "INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, $2, $3)",
    )
    .bind(user.id().as_uuid())
    .bind(cred.password_hash().expose_phc())
    .bind(cred.password_changed_at().as_offset_date_time())
    .execute(pool)
    .await
    .expect("insert cred");
}

fn make_http_state(
    pool: PgPool,
    clock: Arc<FixedClock>,
    passwords: Arc<PasswordService>,
    repo: Arc<PgIdentityRepository>,
) -> HttpState {
    let tokens = test_token_service();
    let store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let repo_dyn: Arc<dyn IdentityRepository> = Arc::clone(&repo) as Arc<dyn IdentityRepository>;
    HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_login_service(test_login_service_pg(
        pool.clone(),
        Arc::clone(&repo_dyn),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_identity_repository(repo_dyn as Arc<dyn IdentityRepository>)
    .with_password_service(passwords)
    .with_refresh_creation_store(
        store.clone() as Arc<dyn orbisync_application::RefreshTokenCreationStore>
    )
    .with_refresh_rotation_store(store as Arc<dyn orbisync_application::RefreshTokenRotationStore>)
}

// ---------------------------------------------------------------------------
// Test 1: concurrent wrong passwords 5+ in parallel → count >=5 and locked
// ---------------------------------------------------------------------------
#[tokio::test]
async fn rv_b_concurrent_wrong_passwords_trigger_lockout() {
    let Some(pool) = database().await else { return };
    let clock = Arc::new(FixedClock::new(now_ts()));
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    // Use low-cost Argon2 hash (m=4096,t=1) so 6 concurrent verifications do not
    // exhaust the PasswordService semaphore (DEFAULT_CONCURRENCY=4). The PHC still
    // validates as Argon2id v19, so the production verify path is used but with
    // minimal CPU, keeping the test concurrent shape without 429.
    let low_hash_phc = {
        use argon2::{Argon2, Params, PasswordHasher};
        use password_hash::{SaltString, rand_core::OsRng};
        let params = Params::new(4096, 1, 1, Some(32)).expect("low params");
        let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let salt = SaltString::generate(&mut OsRng);
        let phc = argon2
            .hash_password(b"Cedar!Lake7-Comet", &salt)
            .expect("low hash")
            .to_string();
        orbisync_domain::PasswordHash::new(phc).expect("phc")
    };
    let hash = low_hash_phc;
    let user = User::new(
        orbisync_domain::UserId::generate(),
        LoginId::new(format!("rvb-{}", uuid::Uuid::now_v7())).expect("login"),
        "RV B User",
        now_ts(),
    )
    .expect("user");
    let cred = Credential::new(user.id(), hash, now_ts());
    let login_str = user.login_id().as_str().to_owned();
    insert_user(&pool, &user, &cred).await;
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
    let state = make_http_state(
        pool.clone(),
        Arc::clone(&clock),
        Arc::clone(&passwords),
        Arc::clone(&repo),
    );

    // 6 concurrent wrong password attempts – low-cost hash keeps verifier under budget
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let s = state.clone();
        let login = login_str.clone();
        tasks.push(tokio::spawn(async move {
            http_login_retry(s, &login, "Wrong!Lake7-Comet").await
        }));
    }
    for t in tasks {
        let st = t.await.expect("task");
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "wrong password must be 401 after retries"
        );
    }

    // Verify DB state: failed_login_count >=5 and locked_until future
    let count: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count");
    assert!(
        count >= 5,
        "concurrent failures must be counted atomically, got {count} expected >=5"
    );
    let locked: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT locked_until FROM user_credentials WHERE user_id = $1")
            .bind(user.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("locked");
    assert!(locked.is_some(), "must be locked after 5 failures");
    assert!(
        locked.unwrap() > now_ts().as_offset_date_time(),
        "locked_until must be in future"
    );

    // Subsequent correct password must still be rejected (locked)
    let status = http_login(state, &login_str, "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "locked account must reject correct password"
    );
}

// ---------------------------------------------------------------------------
// Test 2: success does not wipe a concurrent failure (optimistic predicate)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn rv_b_success_does_not_erase_concurrent_failure() {
    let Some(pool) = database().await else { return };
    let clock = Arc::new(FixedClock::new(now_ts()));
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let hash = passwords
        .hash(SecretString::new("Cedar!Lake7-Comet"))
        .await
        .expect("hash");
    let user = User::new(
        orbisync_domain::UserId::generate(),
        LoginId::new(format!("rvb2-{}", uuid::Uuid::now_v7())).expect("login"),
        "RV B User2",
        now_ts(),
    )
    .expect("user");
    let cred = Credential::new(user.id(), hash, now_ts());
    let login_str = user.login_id().as_str().to_owned();
    insert_user(&pool, &user, &cred).await;
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
    let state = make_http_state(
        pool.clone(),
        Arc::clone(&clock),
        Arc::clone(&passwords),
        Arc::clone(&repo),
    );

    // Read expected values as login path would (find_login snapshot)
    let account = repo
        .find_login(&LoginId::new(login_str.clone()).expect("login"))
        .await
        .expect("find")
        .expect("present");
    let expected_failed = account.credential.failed_login_count();
    let expected_locked = account.credential.locked_until();
    assert_eq!(expected_failed, 0);

    // Simulate concurrent failure recorded via atomic path before success reset
    let store = IdentityAdministrationStore::new(pool.clone());
    store
        .record_login_failure(user.id().as_uuid(), now_ts().as_offset_date_time())
        .await
        .expect("failure increment");

    // Attempt optimistic reset with stale expected (0, None) – must NOT clear
    let reset = repo
        .reset_login_success(user.id(), expected_failed, expected_locked, now_ts(), None)
        .await
        .expect("reset");
    assert!(
        !reset,
        "optimistic predicate must fail when concurrent failure exists"
    );

    let count: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 1, "concurrent failure must survive, not be wiped");

    // Now correct HTTP login should succeed? But since we didn't reset, count is 1, not locked, so login should succeed and then reset to 0.
    // Verify that a real successful login via HTTP now correctly resets only when predicate matches.
    // First, do a successful HTTP login – it will read fresh snapshot (count 1) and reset to 0.
    let status = http_login(state, &login_str, "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "correct password after 1 failure must succeed"
    );
    let count2: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count2");
    assert_eq!(
        count2, 0,
        "successful login must reset count when predicate matches"
    );
}

// ---------------------------------------------------------------------------
// Test 3: while locked, correct password still rejected
// ---------------------------------------------------------------------------
#[tokio::test]
async fn rv_b_locked_rejects_correct_password() {
    let Some(pool) = database().await else { return };
    let clock = Arc::new(FixedClock::new(now_ts()));
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let hash = passwords
        .hash(SecretString::new("Cedar!Lake7-Comet"))
        .await
        .expect("hash");
    let user = User::new(
        orbisync_domain::UserId::generate(),
        LoginId::new(format!("rvb3-{}", uuid::Uuid::now_v7())).expect("login"),
        "RV B User3",
        now_ts(),
    )
    .expect("user");
    let cred = Credential::new(user.id(), hash, now_ts());
    let login_str = user.login_id().as_str().to_owned();
    insert_user(&pool, &user, &cred).await;
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
    let state = make_http_state(
        pool.clone(),
        Arc::clone(&clock),
        Arc::clone(&passwords),
        Arc::clone(&repo),
    );

    // Force 5 failures to lock
    for _ in 0..5 {
        let s = state.clone();
        let l = login_str.clone();
        let st = http_login(s, &l, "Wrong!Lake7-Comet").await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let locked: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT locked_until FROM user_credentials WHERE user_id = $1")
            .bind(user.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("locked");
    assert!(locked.is_some());

    // Correct password while locked must be 401
    let status = http_login(state, &login_str, "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "correct password while locked must be 401"
    );
}

// ---------------------------------------------------------------------------
// Test 4: after lock expiry, correct password succeeds
// ---------------------------------------------------------------------------
#[tokio::test]
async fn rv_b_after_expiry_accepts_correct_password() {
    let Some(pool) = database().await else { return };
    let clock = Arc::new(FixedClock::new(now_ts()));
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let hash = passwords
        .hash(SecretString::new("Cedar!Lake7-Comet"))
        .await
        .expect("hash");
    let user = User::new(
        orbisync_domain::UserId::generate(),
        LoginId::new(format!("rvb4-{}", uuid::Uuid::now_v7())).expect("login"),
        "RV B User4",
        now_ts(),
    )
    .expect("user");
    let cred = Credential::new(user.id(), hash, now_ts());
    let login_str = user.login_id().as_str().to_owned();
    insert_user(&pool, &user, &cred).await;
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
    let state = make_http_state(
        pool.clone(),
        Arc::clone(&clock),
        Arc::clone(&passwords),
        Arc::clone(&repo),
    );

    for _ in 0..5 {
        let s = state.clone();
        let l = login_str.clone();
        assert_eq!(
            http_login(s, &l, "Wrong!Lake7-Comet").await,
            StatusCode::UNAUTHORIZED
        );
    }
    // Advance clock beyond 15 min lock (16 min)
    clock.advance_millis(16 * 60 * 1000);

    let status = http_login(state.clone(), &login_str, "Cedar!Lake7-Comet").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "after expiry correct password must succeed"
    );

    let count: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user.id().as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 0, "successful login after expiry must reset count");
}
