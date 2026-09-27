//! PW-1 real PostgreSQL tests for password reset (session revocation, audit, idempotency).
//! Uses Pg stores, not Fake. Verifies that production SQL is exercised.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

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
    IdempotencyStore, IdentityAdministrationStore, PgIdentityRepository, PgRealtimeTicketStore,
};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

mod common;

/// Explicitly opt in with an isolated DB and a freshly built CLI. This test
/// starts no server process and never discovers an installation's .env.
#[tokio::test]
#[ignore = "requires isolated DATABASE_URL and ORBISYNC_RECOVERY_TEST_BINARY"]
async fn local_operator_recovery_cli_preserves_data() {
    use orbisync_application::{RequestId, SecretString};
    use orbisync_domain::LoginId;
    let pool = database().await.expect("isolated DATABASE_URL required");
    let binary = std::env::var("ORBISYNC_RECOVERY_TEST_BINARY").expect("fresh CLI binary required");
    let root = std::env::temp_dir().join(format!("orbisync-recovery-{}", Uuid::now_v7()));
    std::fs::create_dir(&root).unwrap();
    let corpus = root.join("denylist.txt");
    std::fs::write(
        &corpus,
        (0..10_000)
            .map(|n| format!("test-only-denied-{n}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let output = root.join("private").join("temporary.txt");
    let clock = fixed_clock();
    let (state, admin) = http_state(pool.clone(), clock.clone(), token_service());
    let login = format!("recover-{}", Uuid::now_v7());
    let request = || RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap();
    // This opt-in test requires its own empty database, not a shared installation.
    let (user, initial) = admin
        .bootstrap_administrator(
            LoginId::new(&login).unwrap(),
            "Recovery Admin".into(),
            request(),
        )
        .await
        .unwrap();
    let id = user.id().as_uuid();
    let (_, initial_login) = http_login(state.clone(), &login, initial.expose_secret()).await;
    let old_password = "Cedar!Lake7-Comet-Recovery";
    assert_eq!(
        http_change_pg(
            state.clone(),
            initial_login["access_token"].as_str().unwrap(),
            initial.expose_secret(),
            old_password
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let (status, old_login) = http_login(state.clone(), &login, old_password).await;
    assert_eq!(status, StatusCode::OK);

    let valid_ticket = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header(
            "authorization",
            format!("Bearer {}", old_login["access_token"].as_str().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(state.clone())
            .oneshot(valid_ticket)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let other = Uuid::now_v7();
    let other_login = format!("ordinary-{other}");
    let hash = PasswordService::new(PasswordPolicy::new(Vec::new()))
        .unwrap()
        .hash(SecretString::new("Other!Cedar7-Comet"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1,$2,'Retained user','active',false,1,now(),now())").bind(other).bind(&other_login).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO user_credentials (user_id,password_hash,password_changed_at) VALUES ($1,$2,now())").bind(other).bind(hash.expose_phc()).execute(&pool).await.unwrap();
    assert_eq!(
        http_login(state.clone(), &other_login, "Other!Cedar7-Comet")
            .await
            .0,
        StatusCode::OK
    );
    let world = Uuid::now_v7();
    let instance = Uuid::now_v7();
    let entity = Uuid::now_v7();
    sqlx::query("INSERT INTO world_definitions (id,name,status,capacity,default_spawn,metadata,revision,created_at,updated_at) VALUES ($1,'Retained world','active',10,'{}','{\"save\":42}',1,now(),now())").bind(world).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO world_instances (id,world_id,lifecycle,capacity,created_at,revision) VALUES ($1,$2,'stopped',10,now(),1)").bind(instance).bind(world).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO persistent_entities (id,instance_id,kind,owner_id,visibility,revision,created_at,updated_at) VALUES ($1,$2,'object',$3,'{}',1,now(),now())").bind(entity).bind(instance).bind(other).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO persistent_entity_components (entity_id,component_key,payload) VALUES ($1,'game.save',$2)").bind(entity).bind(b"retained game payload".as_slice()).execute(&pool).await.unwrap();

    async fn snapshot(pool: &PgPool, id: Uuid) -> serde_json::Value {
        sqlx::query_scalar("SELECT jsonb_build_object(
            'users',(SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM users t WHERE id <> $1),
            'credentials',(SELECT jsonb_agg(to_jsonb(t) ORDER BY user_id) FROM user_credentials t WHERE user_id <> $1),
            'sessions',(SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM auth_sessions t WHERE user_id <> $1),
            'roles',(SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM roles t),
            'assignments',(SELECT jsonb_agg(to_jsonb(t) ORDER BY user_id,role_id) FROM user_roles t),
            'permissions',(SELECT jsonb_agg(to_jsonb(t) ORDER BY role_id,permission_name) FROM role_permissions t),
            'worlds',(SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM world_definitions t),
            'instances',(SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM world_instances t),
            'entities',(SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM persistent_entities t),
            'components',(SELECT jsonb_agg(to_jsonb(t) ORDER BY entity_id,component_key) FROM persistent_entity_components t),
            'target',(SELECT to_jsonb(t) - 'revision' - 'updated_at' - 'must_change_password' FROM users t WHERE id=$1))")
            .bind(id).fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&pool, id).await;
    let target_before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(t) FROM user_credentials t WHERE user_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let invoke = |target: &str, destination: &std::path::Path| {
        let mut command = std::process::Command::new(&binary);
        command.env_clear();
        for key in ["SystemRoot", "PATH", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .current_dir(&root)
            .env("DATABASE_URL", std::env::var("DATABASE_URL").unwrap())
            .env("ORBISYNC_PASSWORD_DENYLIST_FILE", &corpus)
            .arg("--config")
            .arg(&config)
            .arg("reset-admin-password")
            .arg("--login-id")
            .arg(target)
            .arg("--password-output")
            .arg(destination)
            .output()
            .unwrap()
    };
    for (target, code) in [
        ("missing-admin", "not_found"),
        (other_login.as_str(), "not_authorized"),
    ] {
        let destination = root
            .join("private")
            .join(format!("refused-{}.txt", Uuid::now_v7()));
        let result = invoke(target, &destination);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(code));
        assert!(std::fs::read(&destination).unwrap().is_empty());
        assert_eq!(snapshot(&pool, id).await, before);
        let target_after: serde_json::Value =
            sqlx::query_scalar("SELECT to_jsonb(t) FROM user_credentials t WHERE user_id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(target_after, target_before);
    }
    // A disabled administrator is not silently reactivated by recovery.
    sqlx::query("UPDATE users SET status='disabled' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let refused = invoke(&login, &root.join("private").join("disabled.txt"));
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not active"));
    sqlx::query("UPDATE users SET status='active' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    // The existing reset clears a credential lockout along with failed attempts.
    sqlx::query("UPDATE user_credentials SET failed_login_count=5, locked_until=now()+interval '15 minutes' WHERE user_id=$1").bind(id).execute(&pool).await.unwrap();
    let result = invoke(&login, &output);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let temporary = std::fs::read_to_string(&output).unwrap();
    let temporary = temporary.trim();
    assert!(!temporary.is_empty());
    assert!(result.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&result.stderr).contains(temporary));
    assert_eq!(snapshot(&pool, id).await, before);
    let audit: (Option<Uuid>, i64) = sqlx::query_as("SELECT actor_user_id, count(*) FROM audit_events WHERE action='administrator.password_recovered' AND target_id=$1 GROUP BY actor_user_id").bind(id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(audit, (None, 1));
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auth_sessions WHERE user_id=$1 AND status='active'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 0);
    assert!(
        !invoke(&login, &output).status.success(),
        "must not overwrite existing output"
    );
    assert_eq!(std::fs::read_to_string(&output).unwrap().trim(), temporary);
    assert_eq!(
        http_login(state.clone(), &login, old_password).await.0,
        StatusCode::UNAUTHORIZED
    );
    let request = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header(
            "authorization",
            format!("Bearer {}", old_login["access_token"].as_str().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let request = Request::builder()
        .uri("/v1/auth/refresh")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"refresh_token": old_login["refresh_token"]}).to_string(),
        ))
        .unwrap();
    assert_eq!(
        router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (status, recovered) = http_login(state.clone(), &login, temporary).await;
    assert_eq!(status, StatusCode::OK);
    let required: bool = sqlx::query_scalar("SELECT must_change_password FROM users WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(required);
    let permanent = "Birch!River8-Comet-Recovered";
    assert_eq!(
        http_change_pg(
            state.clone(),
            recovered["access_token"].as_str().unwrap(),
            temporary,
            permanent
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        http_login(state.clone(), &login, temporary).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(http_login(state, &login, permanent).await.0, StatusCode::OK);
    let required: bool = sqlx::query_scalar("SELECT must_change_password FROM users WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!required);
    std::fs::remove_dir_all(root).unwrap();
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
    let admin_store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let idem_store = Arc::new(IdempotencyStore::new(pool.clone()));
    let dyn_store = DynIdentityAdministrationStore(
        admin_store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>
    );
    let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
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

async fn http_reset(
    state: HttpState,
    bearer: &str,
    target: &str,
    key: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let req = Request::builder()
        .uri(format!("/v1/users/{target}/reset-password"))
        .method("POST")
        .header("authorization", format!("Bearer {bearer}"))
        .header("Idempotency-Key", key)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_change_pg(state: HttpState, bearer: &str, current: &str, new: &str) -> StatusCode {
    let app = router(state);
    let body = serde_json::json!({ "current_password": current, "new_password": new });
    let req = Request::builder()
        .uri("/v1/auth/change-password")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("Idempotency-Key", Uuid::now_v7().to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    resp.status()
}

#[tokio::test]
async fn pg_reset_revokes_sessions_and_old_token_401() {
    let Some(pool) = database().await else { return };
    // No TRUNCATE – shared DB truncation races with parallel tests that count
    // audit_events (e.g. world_auth_rv_c:554 left:0 right:1). Each test uses
    // unique Uuid login_ids and filters by target/request_id, so isolation is
    // achieved without emptying tables. See orchestrator analysis: TRUNCATE in
    // one binary empties audit_events while another binary counts.
    let clock = fixed_clock();
    let tokens = token_service();
    let (state, _admin) = http_state(pool.clone(), Arc::clone(&clock), Arc::clone(&tokens));

    // Create admin directly via SQL (unique, no need for empty DB) with
    // required perms for this test: creating users and resetting credentials.
    // This avoids `bootstrap_administrator` which requires `SELECT count(*) FROM users == 0`.
    let admin_login = format!("pg-admin-{}", Uuid::now_v7());
    let admin_password = "AdminPass123!@#";
    let _admin_user_id = {
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
        sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'Pg Admin', 'active', true, 1, $3, $3)")
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
        sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, 'Pg Test Admin', 1)")
            .bind(role_id)
            .bind(format!("pg-role-{}", Uuid::now_v7()))
            .execute(&pool)
            .await
            .expect("insert role");
        for perm in [
            "admin.users.create",
            "admin.users.credentials.reset",
            "admin.users.read",
            "admin.audit.read",
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

    // create bob via HTTP (so must_change true)
    let bob_login = format!("pg-bob-{}", Uuid::now_v7());
    let app = router(state.clone());
    let body = serde_json::json!({ "login_id": bob_login, "display_name": "Pg Bob" });
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

    // login bob to get session token (creates auth_sessions row) and then change password to make must_change false (so M1 is detectable via DB)
    let (status, json) = http_login(state.clone(), &bob_login, &bob_temp).await;
    assert_eq!(status, StatusCode::OK, "bob login {json}");
    let bob_token_tmp = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let new_pass1 = "NewSecurePass123!@#";
    let status = http_change_pg(state.clone(), &bob_token_tmp, &bob_temp, new_pass1).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "change password must be 204"
    );
    // verify DB must_change is false before reset (M1 would leave it false if deleted)
    let must_before: bool =
        sqlx::query_scalar("SELECT must_change_password FROM users WHERE id = $1")
            .bind(Uuid::parse_str(&bob_id).unwrap())
            .fetch_one(&pool)
            .await
            .expect("must_before");
    assert!(
        !must_before,
        "must_change must be false after change-password before reset"
    );

    // login with new password to get fresh session token for pre-reset valid check
    let (status, json) = http_login(state.clone(), &bob_login, new_pass1).await;
    assert_eq!(status, StatusCode::OK, "bob login with new pass {json}");
    let bob_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // verify fresh token works before reset (realtime ticket)
    let app2 = router(state.clone());
    let req2 = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {bob_token}"))
        .body(Body::empty())
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "fresh token valid before reset"
    );

    // fetch session id for bob (fresh active)
    let session_row: Option<(Uuid, String)> = sqlx::query_as("SELECT id, status FROM auth_sessions WHERE user_id = $1 AND status = 'active' ORDER BY created_at DESC LIMIT 1")
        .bind(Uuid::parse_str(&bob_id).unwrap())
        .fetch_optional(&pool)
        .await
        .expect("query");
    assert!(session_row.is_some(), "session should exist before reset");
    let (session_id, status_before) = session_row.unwrap();
    assert_eq!(status_before, "active");

    // admin resets bob
    let key = Uuid::now_v7().to_string();
    let (status, json) = http_reset(state.clone(), &admin_token, &bob_id, &key).await;
    assert_eq!(status, StatusCode::ACCEPTED, "reset 202 {json}");
    assert_eq!(
        json.get("must_change_password").and_then(|v| v.as_bool()),
        Some(true)
    );
    let new_temp = json
        .get("temporary_password")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    assert_eq!(new_temp.chars().count(), 27);
    // verify DB must_change is true after reset (M1)
    let must_after: bool =
        sqlx::query_scalar("SELECT must_change_password FROM users WHERE id = $1")
            .bind(Uuid::parse_str(&bob_id).unwrap())
            .fetch_one(&pool)
            .await
            .expect("must_after");
    assert!(
        must_after,
        "must_change_password must be true in DB after reset (Pg) - M1"
    );
    // ensure new temp is not leaked in audit
    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'password.reset' AND target_id = $1 AND result = 'success'")
        .bind(&bob_id)
        .fetch_one(&pool)
        .await
        .expect("audit count");
    assert_eq!(audit_count, 1, "one success audit for reset");

    // verify DB row is revoked
    let status_after: String = sqlx::query_scalar("SELECT status FROM auth_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .expect("status after");
    assert_eq!(
        status_after, "revoked",
        "auth_sessions row must be revoked after reset (Pg)"
    );
    let reason: Option<String> =
        sqlx::query_scalar("SELECT revocation_reason FROM auth_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("reason");
    assert_eq!(reason.as_deref(), Some("password_reset"));

    // old token must now be 401
    let app3 = router(state.clone());
    let req3 = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST")
        .header("authorization", format!("Bearer {bob_token}"))
        .body(Body::empty())
        .unwrap();
    let resp3 = app3.oneshot(req3).await.unwrap();
    assert_eq!(
        resp3.status(),
        StatusCode::UNAUTHORIZED,
        "old token must be 401 after reset"
    );

    // new temp allows login
    let (status, _json) = http_login(state.clone(), &bob_login, &new_temp).await;
    assert_eq!(status, StatusCode::OK, "new temp must login");

    // AUD-C1: idempotency replay must NOT return temporary_password
    let (status2, json2) = http_reset(state.clone(), &admin_token, &bob_id, &key).await;
    assert_eq!(status2, StatusCode::ACCEPTED);
    assert_eq!(
        json2.get("must_change_password").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        json2.get("temporary_password").is_none(),
        "replay must not contain temporary_password (AUD-C1)"
    );

    // audit must not contain password value
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT action, metadata FROM audit_events WHERE target_id = $1")
            .bind(&bob_id)
            .fetch_all(&pool)
            .await
            .expect("audit rows");
    for (action, meta) in rows {
        let s = format!("{action} {}", meta);
        assert!(
            !s.contains(&new_temp),
            "audit must not contain temp password"
        );
        assert!(!s.contains(&bob_temp), "audit must not contain old temp");
    }

    // AUD-C1: response_body must NOT contain plaintext temporary_password
    let row: (Vec<u8>,) =
        sqlx::query_as("SELECT response_body FROM idempotency_records WHERE key = $1")
            .bind(Uuid::parse_str(&key).unwrap())
            .fetch_one(&pool)
            .await
            .expect("idem row");
    let body_bytes = row.0;
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(
        !body_str.contains(&new_temp),
        "response_body must not contain temporary_password plaintext (AUD-C1)"
    );
    assert!(
        !body_str.contains("temporary_password"),
        "response_body must not contain temporary_password key (AUD-C1)"
    );
    let idem_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM idempotency_records WHERE key = $1")
            .bind(Uuid::parse_str(&key).unwrap())
            .fetch_one(&pool)
            .await
            .expect("idem count");
    assert_eq!(idem_count, 1, "idempotency record must exist");
}
