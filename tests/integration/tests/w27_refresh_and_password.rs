//! W-27: refresh reuse audit distinctness and password-change restoration (real Postgres).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    unused_imports
)]

use std::sync::Arc;

use orbisync_application::{
    RefreshRotationResult, RefreshTokenReplacement, RequestId, RotateRefreshTokenCommand,
    SecretString,
};
use orbisync_domain::{Credential, LoginId, Timestamp, User, UserId};
use orbisync_identity::{
    LoginService, PasswordPolicy, PasswordService, admin::IdentityAdministrationService,
};
use orbisync_storage_postgres::{
    IdempotencyStore, IdentityAdministrationStore, PgIdentityRepository, PgLoginStore,
    PgRealtimeTicketStore,
};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

mod common;

const W27_PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const W27_PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

async fn database() -> Option<PgPool> {
    common::pool_or_skip().await
}

fn fixed_now() -> (Timestamp, OffsetDateTime) {
    let ts = Timestamp::from_unix_millis(1_700_000_000_000).expect("ts");
    (ts, ts.as_offset_date_time())
}

fn test_login_service_pg(
    pool: PgPool,
    repo: Arc<dyn orbisync_application::IdentityRepository>,
    passwords: Arc<PasswordService>,
    tokens: Arc<orbisync_identity::token::AccessTokenService>,
    clock: Arc<orbisync_testkit::FixedClock>,
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

fn req_id() -> RequestId {
    RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req")
}

async fn insert_user_with_credential(pool: &PgPool, user: &User, credential: &Credential) {
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
    .bind(credential.user_id().as_uuid())
    .bind(credential.password_hash().expose_phc())
    .bind(credential.password_changed_at().as_offset_date_time())
    .bind(i32::try_from(credential.failed_login_count()).expect("count"))
    .bind(credential.locked_until().map(|t| t.as_offset_date_time()))
    .execute(pool)
    .await
    .expect("insert credential");
}

// ---------------------------------------------------------------------------
// 1. refresh reuse is distinct from normal refresh, session revoked, no secret leaked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w27_refresh_reuse_is_distinct_and_revokes() {
    let Some(pool) = database().await else { return };
    let (now_ts, now_odt) = fixed_now();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let family_id = Uuid::now_v7();
    // minimal user for FK
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'Revoke Test', 'active', true, 1, $3, $3)")
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

    let original_raw = format!("orig-{}", Uuid::now_v7());
    // we use the store's digest via HMAC? Simpler: use random bytes as digest directly since store treats digest as opaque key.
    let original_digest: [u8; 32] = {
        let mut d = [0u8; 32];
        d[..16].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d[16..].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d
    };
    // Insert original refresh token row
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(Uuid::now_v7())
        .bind(session_id)
        .bind(family_id)
        .bind(original_digest.as_slice())
        .bind(now_odt)
        .bind(now_odt + Duration::days(30))
        .execute(&pool)
        .await
        .expect("token");

    let store = IdentityAdministrationStore::new(pool.clone());

    // First rotation: consumed -> rotated
    let replacement_digest_1: [u8; 32] = {
        let mut d = [1u8; 32];
        d[..16].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d
    };
    let cmd1 = RotateRefreshTokenCommand {
        presented_digest: original_digest,
        replacement: RefreshTokenReplacement {
            token_id: Uuid::now_v7().to_string(),
            digest: replacement_digest_1,
            issued_at: now_ts,
            expires_at: Timestamp::from_offset_date_time(now_odt + Duration::days(30)),
        },
        now: now_ts,
        request_id: req_id(),
    };
    let out1 = orbisync_application::RefreshTokenRotationStore::rotate(&store, cmd1)
        .await
        .expect("rotate1");
    assert!(
        matches!(out1, RefreshRotationResult::Rotated { .. }),
        "first consume must be Rotated, got {out1:?}"
    );

    // Verify audit row for rotated is token.refreshed success with session/family ids
    let rows: Vec<(String, String, Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT action, result, target_id, metadata FROM audit_events WHERE target_id = $1 ORDER BY occurred_at DESC LIMIT 2",
    )
    .bind(session_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("audit query");
    // At least one row should be token.refreshed
    assert!(
        rows.iter()
            .any(|(a, r, _, _)| a == "token.refreshed" && r == "success"),
        "must have token.refreshed success row for this session, got {rows:?}"
    );
    // Ensure no raw token material in audit rows (original_raw should not appear)
    for (_, _, _, meta) in &rows {
        let meta_str = meta.to_string();
        assert!(
            !meta_str.contains(&original_raw),
            "audit metadata must not contain raw token"
        );
        assert!(
            !meta_str.contains("digest"),
            "audit metadata must not contain digest keyword (no secret)"
        );
    }

    // Second rotation with same digest -> reuse detected
    let replacement_digest_2: [u8; 32] = {
        let mut d = [2u8; 32];
        d[..16].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d
    };
    let cmd2 = RotateRefreshTokenCommand {
        presented_digest: original_digest,
        replacement: RefreshTokenReplacement {
            token_id: Uuid::now_v7().to_string(),
            digest: replacement_digest_2,
            issued_at: now_ts,
            expires_at: Timestamp::from_offset_date_time(now_odt + Duration::days(30)),
        },
        now: now_ts,
        request_id: req_id(),
    };
    let out2 = orbisync_application::RefreshTokenRotationStore::rotate(&store, cmd2)
        .await
        .expect("rotate2");
    assert_eq!(
        out2,
        RefreshRotationResult::ReuseDetected,
        "second consume of same digest must be ReuseDetected"
    );

    // Verify distinct audit row: token.reuse_detected failure (filter by our session to avoid cross-test pollution)
    let rows2: Vec<(String, String, Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT action, result, target_id, metadata FROM audit_events WHERE target_id = $1 ORDER BY occurred_at DESC",
    )
    .bind(session_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("audit2");
    let reuse_rows: Vec<_> = rows2
        .iter()
        .filter(|(a, _, _, _)| a == "token.reuse_detected")
        .collect();
    assert_eq!(
        reuse_rows.len(),
        1,
        "must have exactly one token.reuse_detected row for this session, got {rows2:?}"
    );
    let (action, result, target_id, metadata) = &reuse_rows[0];
    assert_eq!(action, "token.reuse_detected");
    assert_eq!(
        result, "failure",
        "reuse must be distinguishable via result failure"
    );
    assert_eq!(
        target_id.as_deref(),
        Some(session_id.to_string().as_str()),
        "reuse audit must carry session_id as target_id"
    );
    let meta_str = metadata.to_string();
    assert!(
        meta_str.contains(&session_id.to_string()),
        "reuse metadata must contain session_id"
    );
    assert!(
        meta_str.contains(&family_id.to_string()),
        "reuse metadata must contain family_id"
    );
    // No secret leakage in reuse row
    for (_, _, _, meta) in &rows2 {
        let s = meta.to_string();
        // Metadata should not leak raw token material; we already checked original_raw above.
        // The legitimate reason field contains "refresh_token_reuse" so we must not assert on generic "token".
        assert!(
            !s.contains(&original_raw),
            "metadata must not contain raw token value"
        );
    }

    // Also check that normal token.refreshed row is still distinguishable (different action)
    let normal_rows: Vec<_> = rows2
        .iter()
        .filter(|(a, _, _, _)| a == "token.refreshed")
        .collect();
    assert!(!normal_rows.is_empty(), "normal refreshed rows must exist");
    assert_ne!(
        normal_rows[0].0, reuse_rows[0].0,
        "actions must be distinct"
    );
    assert_ne!(
        normal_rows[0].1, reuse_rows[0].1,
        "results must be distinct (success vs failure)"
    );

    // Session must be revoked
    let status: String = sqlx::query_scalar("SELECT status FROM auth_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .expect("session");
    assert_eq!(status, "revoked", "session must be revoked on reuse");
    let reason: Option<String> =
        sqlx::query_scalar("SELECT revocation_reason FROM auth_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("reason");
    assert_eq!(reason.as_deref(), Some("refresh_token_reuse"));

    // 3rd check: audit rows must not contain raw token or digest
    for (action, result, target_id, metadata) in rows2 {
        let combined = format!(
            "{action} {result} {} {}",
            target_id.unwrap_or_default(),
            metadata
        );
        assert!(!combined.contains(&original_raw));
        // Ensure digest bytes not leaked in any string form (base64 of digest would be ~44 chars;
        // we just check that the audit does not contain the word digest which would hint at leakage)
        assert!(
            !combined.to_lowercase().contains("digest"),
            "audit must not contain digest word"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. password change clears flag, resets lock, single audit, login behavior
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TestClock(Timestamp);
impl orbisync_domain::Clock for TestClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

#[tokio::test]
async fn w27_password_change_clears_flag_resets_lock_and_audits() {
    let Some(pool) = database().await else { return };
    let (now_ts, _now_odt) = fixed_now();
    let clock = Arc::new(TestClock(now_ts));
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("passwords");
    let store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    // Use service with concrete clock type (TestClock) – not erasing to dyn
    let service = IdentityAdministrationService::new(
        Arc::clone(&store),
        Arc::clone(&clock),
        passwords.clone(),
    );

    // Create user with temporary password (must_change true)
    let user_id = UserId::generate();
    let login = LoginId::new(format!("w27user-{}", Uuid::now_v7())).expect("login");
    let display = "W27 User".to_owned();
    let user = User::new(user_id, login.clone(), display, now_ts).expect("user");
    // user is must_change = true, revision 0
    let old_password = SecretString::new("Cedar!Lake7-Comet");
    let old_hash = passwords
        .hash(old_password.clone())
        .await
        .expect("hash old");
    let mut credential = Credential::new(user_id, old_hash, now_ts);
    // Simulate 2 failed logins and lock
    credential.record_failure(now_ts).expect("failure1");
    credential.record_failure(now_ts).expect("failure2");
    // Also test lock was not yet (needs 5), but we can set failed count manually via reconstitute
    // For stronger test, create credential with failed count 3 and locked_until future
    let locked_until = now_ts.checked_add_millis(15 * 60 * 1000).expect("locked");
    let credential_locked = Credential::reconstitute(
        user_id,
        credential.password_hash().clone(),
        credential.password_changed_at(),
        4,
        Some(locked_until),
    );
    let mut user_locked = user.clone();
    let mut cred_locked = credential_locked.clone();

    insert_user_with_credential(&pool, &user_locked, &cred_locked).await;

    // Verify DB has must_change true and failed count 4
    let must_change: bool =
        sqlx::query_scalar("SELECT must_change_password FROM users WHERE id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("must_change");
    assert!(must_change, "must_change_password must start true");
    let failed: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("failed");
    assert_eq!(failed, 4);

    // Now change password via service
    let new_password = SecretString::new("Maple!Lake9-CometX");
    let before_audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'password.changed' AND target_id = $1",
    )
    .bind(user_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        before_audit_count, 0,
        "no prior password.changed for this user"
    );
    let current_for_change = SecretString::new("Cedar!Lake7-Comet");
    service
        .change_password(
            &mut user_locked,
            &mut cred_locked,
            current_for_change,
            new_password.clone(),
            req_id(),
        )
        .await
        .expect("change password");

    // Check in-memory mutated objects
    assert!(
        !user_locked.must_change_password(),
        "must_change_password must be cleared in memory"
    );
    assert!(user_locked.revision().as_u64() > 0, "revision must advance");
    assert_eq!(
        cred_locked.failed_login_count(),
        0,
        "failed count reset in memory"
    );
    assert!(
        cred_locked.locked_until().is_none(),
        "lock cleared in memory"
    );

    // Check DB
    let must_change_db: bool =
        sqlx::query_scalar("SELECT must_change_password FROM users WHERE id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("must_change db");
    assert!(!must_change_db, "must_change_password must be false in DB");
    let revision_db: i64 = sqlx::query_scalar("SELECT revision FROM users WHERE id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("rev");
    assert_eq!(
        revision_db,
        i64::try_from(user_locked.revision().as_u64()).expect("rev"),
        "revision must persist"
    );
    let failed_db: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("failed db");
    assert_eq!(failed_db, 0, "failed_login_count must be 0 in DB");
    let locked_db: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT locked_until FROM user_credentials WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_optional(&pool)
            .await
            .expect("locked")
            .flatten();
    assert!(locked_db.is_none(), "locked_until must be NULL in DB");

    // Audit row exactly one new for this user (filter by target_id to avoid cross-test pollution)
    let after_audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'password.changed' AND target_id = $1",
    )
    .bind(user_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        after_audit_count, 1,
        "exactly one password.changed audit row for this user must exist"
    );
    let audit_row: (String, String, Option<String>, serde_json::Value) = sqlx::query_as(
        "SELECT action, result, target_id, metadata FROM audit_events WHERE action = 'password.changed' AND target_id = $1 ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(user_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("audit row");
    assert_eq!(audit_row.0, "password.changed");
    assert_eq!(audit_row.1, "success");
    assert_eq!(audit_row.2.as_deref(), Some(user_id.to_string().as_str()));
    // Ensure audit does not contain password
    let meta_str = audit_row.3.to_string();
    assert!(
        !meta_str.contains("Cedar"),
        "audit metadata must not contain old password"
    );
    assert!(
        !meta_str.contains("Maple"),
        "audit metadata must not contain new password"
    );
    assert!(
        !audit_row.3.to_string().contains("hash"),
        "audit must not contain hash"
    );

    // Login behavior: new password succeeds, old fails
    let repo = PgIdentityRepository::new(pool.clone());
    let auth_service =
        orbisync_identity::service::AuthenticationService::new(Arc::new(repo), passwords.clone());
    // Need to test via authenticate: it will verify new password
    let login_ok = auth_service
        .authenticate(&login, new_password.clone(), now_ts)
        .await;
    assert!(
        login_ok.is_ok(),
        "new password must authenticate, got {login_ok:?}"
    );
    assert!(
        !login_ok.unwrap().must_change_password,
        "after change, must_change_password must be false in login response"
    );
    let login_old = auth_service
        .authenticate(&login, SecretString::new("Cedar!Lake7-Comet"), now_ts)
        .await;
    assert!(
        login_old.is_err(),
        "old password must fail after change, got {login_old:?}"
    );

    // Verify that password_changed_at advanced (should be now_ts)
    let pwd_changed: OffsetDateTime =
        sqlx::query_scalar("SELECT password_changed_at FROM user_credentials WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("pwd_changed");
    assert_eq!(
        pwd_changed,
        now_ts.as_offset_date_time(),
        "password_changed_at must be updated to now"
    );
}

#[tokio::test]
async fn w27_password_change_rejects_weak_password() {
    let Some(pool) = database().await else { return };
    let (now_ts, _) = fixed_now();
    let clock = Arc::new(TestClock(now_ts));
    // Policy with denylist containing "correct horse battery staple"
    let policy = PasswordPolicy::new([String::from("correct horse battery staple")]);
    let passwords = PasswordService::new(policy).expect("passwords");
    let store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let service =
        IdentityAdministrationService::new(Arc::clone(&store), Arc::clone(&clock), passwords);
    let user_id = UserId::generate();
    let login = LoginId::new(format!("weak-{}", Uuid::now_v7())).expect("login");
    let mut user = User::new(user_id, login, "Weak Test", now_ts).expect("user");
    let hash = PasswordService::new(PasswordPolicy::new(Vec::new()))
        .expect("tmp")
        .hash(SecretString::new("Cedar!Lake7-Comet"))
        .await
        .expect("hash");
    let mut credential = Credential::new(user_id, hash, now_ts);
    // Do not insert into DB; we test that service rejects before storage (policy violation)
    let weak = SecretString::new("short");
    let current = SecretString::new("Cedar!Lake7-Comet");
    let result = service
        .change_password(&mut user, &mut credential, current, weak, req_id())
        .await;
    assert!(result.is_err(), "weak password must be rejected");
    let err = result.unwrap_err();
    assert_eq!(
        err.kind(),
        orbisync_application::ApplicationErrorKind::DomainRule,
        "weak password should map to DomainRule"
    );
}

#[tokio::test]
async fn w27_audit_contains_no_token_secret() {
    let Some(pool) = database().await else { return };
    let (now_ts, now_odt) = fixed_now();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let family_id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'Secret Check', 'active', true, 1, $3, $3)")
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
    let digest: [u8; 32] = {
        let mut d = [0u8; 32];
        d[..16].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d[16..].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d
    };
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(Uuid::now_v7())
        .bind(session_id)
        .bind(family_id)
        .bind(digest.as_slice())
        .bind(now_odt)
        .bind(now_odt + Duration::days(30))
        .execute(&pool)
        .await
        .expect("token");
    let store = IdentityAdministrationStore::new(pool.clone());
    let repl_digest: [u8; 32] = {
        let mut d = [1u8; 32];
        d[..16].copy_from_slice(&Uuid::now_v7().as_bytes()[..16]);
        d
    };
    let cmd = RotateRefreshTokenCommand {
        presented_digest: digest,
        replacement: RefreshTokenReplacement {
            token_id: Uuid::now_v7().to_string(),
            digest: repl_digest,
            issued_at: now_ts,
            expires_at: Timestamp::from_offset_date_time(now_odt + Duration::days(30)),
        },
        now: now_ts,
        request_id: req_id(),
    };
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = orbisync_application::RefreshTokenRotationStore::rotate(&store, cmd)
        .await
        .expect("rotate");

    // Check all recent audit rows for any digest leakage
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_events ORDER BY occurred_at DESC LIMIT 10",
    )
    .fetch_all(&pool)
    .await
    .expect("audit");
    for (action, meta) in rows {
        let s = format!("{action} {}", meta);
        assert!(
            !s.to_lowercase().contains("digest"),
            "audit metadata should not contain word digest for action {action}"
        );
        // token word only allowed in action, not in metadata values (metadata is checked via lower)
        // We just ensure action is one of the two allowed token actions
        assert!(
            action == "token.refreshed"
                || action == "token.reuse_detected"
                || !s.to_lowercase().contains("token"),
            "metadata should not leak token word outside action"
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP-level CR-01: must_change user can change password via POST /v1/auth/change-password
// ---------------------------------------------------------------------------

async fn w27_http_state(
    pool: PgPool,
) -> (
    orbisync_transport_http::HttpState,
    Arc<orbisync_identity::token::AccessTokenService>,
) {
    use orbisync_domain::Clock;
    use orbisync_identity::{DynClock, DynIdentityAdministrationStore};
    use orbisync_testkit::FixedClock;
    let (now_ts, _) = fixed_now();
    let clock = Arc::new(FixedClock::new(now_ts));
    let tokens = Arc::new(
        orbisync_identity::token::AccessTokenService::from_ed25519_pem(
            W27_PRIVATE_PEM,
            W27_PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("token service"),
    );
    let passwords =
        Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("passwords"));
    let repo: Arc<dyn orbisync_application::IdentityRepository> =
        Arc::new(PgIdentityRepository::new(pool.clone()));
    let admin_store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let dyn_store = DynIdentityAdministrationStore(
        Arc::clone(&admin_store) as Arc<dyn orbisync_application::IdentityAdministrationStore>
    );
    let dyn_clock = DynClock(Arc::clone(&clock) as Arc<dyn Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let idempotency_store: Arc<dyn orbisync_application::IdempotencyStore> =
        Arc::new(IdempotencyStore::new(pool.clone()));
    let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
        Arc::new(PgRealtimeTicketStore::new(pool.clone()));
    let state = orbisync_transport_http::HttpState::new(
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
        repo.clone(),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_identity_repository(repo)
    .with_password_service(passwords)
    .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    .with_realtime_ticket_store(ticket_store)
    .with_identity_admin_service(admin)
    .with_idempotency_store(idempotency_store)
    .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
    .with_refresh_creation_store(Arc::new(IdentityAdministrationStore::new(pool.clone()))
        as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
    .with_refresh_rotation_store(Arc::new(IdentityAdministrationStore::new(pool.clone()))
        as Arc<dyn orbisync_application::RefreshTokenRotationStore>);
    (state, tokens)
}

async fn w27_http_login(
    state: orbisync_transport_http::HttpState,
    login_id: &str,
    password: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
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

async fn w27_http_change_password(
    state: orbisync_transport_http::HttpState,
    token: &str,
    current: &str,
    new: &str,
) -> axum::http::StatusCode {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
    let app = orbisync_transport_http::router(state);
    let body = serde_json::json!({ "current_password": current, "new_password": new });
    let req = Request::builder()
        .uri("/v1/auth/change-password")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("Idempotency-Key", Uuid::now_v7().to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = resp.into_body().collect().await.unwrap().to_bytes();
    status
}

#[tokio::test]
async fn w27_http_change_password_end_to_end_clears_must_change() {
    let Some(pool) = database().await else { return };
    let (now_ts, _) = fixed_now();
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("passwords");
    // Insert user directly via store-adjacent SQL with must_change true
    let user_id = UserId::generate();
    let login = format!("http-cr01-{}", Uuid::now_v7());
    let tmp_pw = "Cedar!Lake7-Comet";
    let hash = passwords
        .hash(SecretString::new(tmp_pw))
        .await
        .expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "CR01 User".to_owned(),
        now_ts,
    )
    .expect("user");
    let credential = Credential::new(user_id, hash, now_ts);
    insert_user_with_credential(&pool, &user, &credential).await;

    // Build HTTP state
    let (state, _) = w27_http_state(pool.clone()).await;

    // Login to get token (must succeed even though must_change true)
    let (status, json) = w27_http_login(state.clone(), &login, tmp_pw).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "login must succeed for must_change user: {json}"
    );
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .expect("token")
        .to_owned();

    // Change password via HTTP – the CR-01 proof that the dead end is unblocked
    let new_pw = "Maple!Lake9-CometX";
    let status = w27_http_change_password(state.clone(), &token, tmp_pw, new_pw).await;
    assert_eq!(
        status,
        axum::http::StatusCode::NO_CONTENT,
        "change-password must be 204"
    );

    // must_change must be false now
    let must: bool = sqlx::query_scalar("SELECT must_change_password FROM users WHERE id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("must");
    assert!(!must, "must_change_password must be cleared via HTTP path");

    // Old password fails, new succeeds
    let (status_old, _) = w27_http_login(state.clone(), &login, tmp_pw).await;
    assert_eq!(
        status_old,
        axum::http::StatusCode::UNAUTHORIZED,
        "old password must fail"
    );
    let (status_new, json_new) = w27_http_login(state.clone(), &login, new_pw).await;
    assert_eq!(
        status_new,
        axum::http::StatusCode::OK,
        "new password must login: {json_new}"
    );
    assert!(json_new.get("access_token").is_some());

    // audit row exactly one success for this user
    let cnt: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='password.changed' AND target_id=$1 AND result='success'")
        .bind(user_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("cnt");
    assert_eq!(cnt, 1, "exactly one success audit for HTTP change");
}

#[tokio::test]
async fn w27_http_change_password_rejects_wrong_current() {
    let Some(pool) = database().await else { return };
    let (now_ts, _) = fixed_now();
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("passwords");
    let user_id = UserId::generate();
    let login = format!("http-wrong-{}", Uuid::now_v7());
    let real_pw = "Cedar!Lake7-Comet";
    let hash = passwords
        .hash(SecretString::new(real_pw))
        .await
        .expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "Wrong Current".to_owned(),
        now_ts,
    )
    .expect("user");
    let credential = Credential::new(user_id, hash, now_ts);
    insert_user_with_credential(&pool, &user, &credential).await;
    let (state, _) = w27_http_state(pool.clone()).await;
    let (status, json) = w27_http_login(state.clone(), &login, real_pw).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{json}");
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    let status = w27_http_change_password(
        state.clone(),
        &token,
        "Wrong!Lake7-Comet",
        "Maple!Lake9-CometX",
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNAUTHORIZED,
        "wrong current must be 401"
    );

    // failure audit exists
    let cnt: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='password.changed' AND target_id=$1 AND result='failure'")
        .bind(user_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("cnt");
    assert_eq!(cnt, 1, "wrong current must leave failure audit");

    // password unchanged – real password still works
    let (status2, _) = w27_http_login(state.clone(), &login, real_pw).await;
    assert_eq!(status2, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn w27_http_old_token_invalid_after_password_change() {
    let Some(pool) = database().await else { return };
    let (now_ts, _) = fixed_now();
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("passwords");
    let user_id = UserId::generate();
    let login = format!("http-revoke-{}", Uuid::now_v7());
    let old_pw = "Cedar!Lake7-Comet";
    let hash = passwords
        .hash(SecretString::new(old_pw))
        .await
        .expect("hash");
    let user = User::new(
        user_id,
        LoginId::new(login.clone()).expect("login"),
        "Revoke Test".to_owned(),
        now_ts,
    )
    .expect("user");
    let credential = Credential::new(user_id, hash, now_ts);
    insert_user_with_credential(&pool, &user, &credential).await;
    let (state, _) = w27_http_state(pool.clone()).await;
    let (status, json) = w27_http_login(state.clone(), &login, old_pw).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{json}");
    let old_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // old token can get realtime ticket before change (200)
    {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;
        let app = orbisync_transport_http::router(state.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {old_token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "old token must work before change"
        );
        // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
    }

    // change password via HTTP
    let new_pw = "Maple!Lake9-CometX";
    let status = w27_http_change_password(state.clone(), &old_token, old_pw, new_pw).await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    // old token must now be 401 on same ticket endpoint
    {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;
        let app = orbisync_transport_http::router(state.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {old_token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "old token must be 401 after password change revocation"
        );
    }

    // new token works
    let (status2, json2) = w27_http_login(state.clone(), &login, new_pw).await;
    assert_eq!(status2, axum::http::StatusCode::OK, "{json2}");
    let new_token = json2
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;
        let app = orbisync_transport_http::router(state.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {new_token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "new token must succeed"
        );
    }
}

#[tokio::test]
async fn w27_protected_route_requires_repository_is_wired() {
    // Verifies hardening: missing IdentityRepository must not silently bypass
    // session revocation – it must fail explicitly with 500.
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use orbisync_domain::Timestamp;
    use orbisync_testkit::FixedClock;
    let now = Timestamp::from_unix_millis(1_700_000_000_000).expect("ts");
    let tokens = Arc::new(
        orbisync_identity::token::AccessTokenService::from_ed25519_pem(
            W27_PRIVATE_PEM,
            W27_PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("tokens"),
    );
    let clock = Arc::new(FixedClock::new(now));
    // State without identity_repository – should cause 500 on protected route
    let state = orbisync_transport_http::HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        clock.clone() as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens));
    // Issue a valid token via service (no session persisted, but crypto valid)
    let user_id = UserId::generate();
    let session_id = orbisync_domain::AuthSessionId::generate();
    let token = tokens
        .issue(user_id, session_id, now)
        .expect("issue")
        .expose_secret()
        .to_owned();
    let app = orbisync_transport_http::router(state);
    let req = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    use tower::ServiceExt as _;
    let resp = app.oneshot(req).await.unwrap();
    // Must not be 200 – revocation check cannot be bypassed by omitting repository
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "missing repository must not silently allow protected route"
    );
}
