//! CR-13 reproduction: broken sid should be 401, fail-open must be detected
#![allow(clippy::expect_used, clippy::unwrap_used, missing_docs)]
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use orbisync_application::{IdentityAdministrationStore, IdentityRepository, RequestId};
use orbisync_domain::{Clock, LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
    PasswordService, token::AccessTokenService,
};
use orbisync_testkit::{FakeIdentityStore, FixedClock};
use orbisync_transport_http::{HttpState, router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower::ServiceExt;
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
        .expect("svc"),
    )
}

fn test_login_service(
    store: Arc<FakeIdentityStore>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<orbisync_identity::LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> = store.clone() as Arc<dyn LoginTransactionStore>;
    let repo_dyn: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    Arc::new(orbisync_identity::LoginService::new(
        repo_dyn,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn fixed_clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ))
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

#[tokio::test]
async fn cr13_broken_sid_is_401_before_fix() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = fixed_clock();
    let tokens = test_token_service();
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    let admin_port: Arc<dyn IdentityAdministrationStore> =
        store.clone() as Arc<dyn IdentityAdministrationStore>;
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(clock.clone() as Arc<dyn Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let (admin_user, admin_pw) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        )
        .await
        .expect("bootstrap");
    let admin_user_id = admin_user.id().to_string();
    let admin_temp_pw = admin_pw.expose_secret().to_owned();

    // Create a real role to use for GET /v1/roles/{role_id} test
    // Use admin service directly to create a role so it exists
    let role = {
        use orbisync_domain::Permission;
        use std::collections::BTreeSet;
        let mut perms = BTreeSet::new();
        perms.insert(Permission::new("admin.roles.read".to_owned()).unwrap());
        let cmd = orbisync_application::CreateRoleCommand {
            name: "test-role".to_owned(),
            description: None,
            permissions: perms,
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        };
        let actor_roles = store.roles_for_user(admin_user.id()).await.unwrap();
        admin
            .create_role(cmd, &actor_roles)
            .await
            .expect("create role")
    };
    let role_id = role.id().to_string();

    // Create broken sid tokens with REAL sub (admin) so that fail-open would proceed to success
    let broken_real_admin = make_broken_sid_token(admin_user_id.clone(), "not-a-uuid".to_owned());
    let broken_v4_real_admin = make_broken_sid_token(
        admin_user_id.clone(),
        "550e8400-e29b-41d4-a716-446655440000".to_owned(),
    );
    // Valid v7 but unknown session – exercises `Ok(None) => 401` branch (M4)
    let unknown_sid_token =
        make_broken_sid_token(admin_user_id.clone(), Uuid::now_v7().to_string());

    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        orbisync_protocol::PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_token_service(tokens.clone())
    .with_identity_repository(repo)
    .with_password_service(passwords.clone())
    .with_identity_admin_service(admin.clone());
    let state = {
        let q: Arc<dyn orbisync_application::IdentityQueryPort> =
            store.clone() as Arc<dyn orbisync_application::IdentityQueryPort>;
        let a: Arc<dyn orbisync_application::AuditQueryPort> =
            store.clone() as Arc<dyn orbisync_application::AuditQueryPort>;
        state.with_identity_query(q).with_audit_query(a)
    };
    async fn test_endpoint(
        state: HttpState,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<String>,
    ) -> StatusCode {
        let app = router(state);
        let mut builder = Request::builder().uri(uri).method(method);
        builder = builder.header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        if uri == "/v1/auth/change-password" {
            builder = builder.header("Idempotency-Key", Uuid::now_v7().to_string());
        }
        let req = if let Some(b) = body {
            builder.body(Body::from(b)).unwrap()
        } else {
            builder.body(Body::empty()).unwrap()
        };
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        println!("{} {} => {} body {}", method, uri, status, json);
        status
    }
    // For change-password, use correct current password (admin_temp_pw) so that fail-open would be 204, not 401 due to wrong password
    let change_pw_body = serde_json::json!({
        "current_password": admin_temp_pw,
        "new_password": "New!Pass12345678"
    })
    .to_string();

    let dummy_world_id = Uuid::now_v7().to_string();
    let endpoints: Vec<(&str, String, Option<String>)> = vec![
        (
            "POST",
            "/v1/users".to_owned(),
            Some(r#"{"login_id":"bob","display_name":"Bob"}"#.to_owned()),
        ),
        (
            "POST",
            "/v1/roles".to_owned(),
            Some(r#"{"name":"test2","permissions":[]}"#.to_owned()),
        ),
        (
            "POST",
            "/v1/auth/change-password".to_owned(),
            Some(change_pw_body),
        ),
        ("GET", "/v1/roles".to_owned(), None),
        ("GET", format!("/v1/roles/{role_id}"), None),
        (
            "POST",
            "/v1/worlds".to_owned(),
            Some(r#"{"name":"test-world","capacity":10}"#.to_owned()),
        ),
        (
            "POST",
            "/v1/instances".to_owned(),
            Some(format!(r#"{{"world_id":"{dummy_world_id}"}}"#)),
        ),
    ];
    println!("--- testing broken sid not-a-uuid with REAL admin sub ---");
    for (m, u, b) in &endpoints {
        let status = test_endpoint(state.clone(), m, u, &broken_real_admin, b.clone()).await;
        println!("result {} {} with broken sid => {}", m, u, status);
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "broken sid should be 401 for {} {}",
            m,
            u
        );
    }
    println!("--- testing broken sid v4 with REAL admin sub ---");
    for (m, u, b) in &endpoints {
        let status = test_endpoint(state.clone(), m, u, &broken_v4_real_admin, b.clone()).await;
        println!("result {} {} with broken v4 sid => {}", m, u, status);
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "broken v4 sid should be 401 for {} {}",
            m,
            u
        );
    }
    println!("--- testing unknown valid v7 sid with REAL admin sub (M4) ---");
    for (m, u, b) in &endpoints {
        let status = test_endpoint(state.clone(), m, u, &unknown_sid_token, b.clone()).await;
        println!("result {} {} with unknown sid => {}", m, u, status);
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "unknown sid should be 401 for {} {}",
            m,
            u
        );
    }
    // Also verify that a valid token still works (sanity)
    let valid_token = {
        // Login via service to get a valid token? Instead use tokens.issue directly and save session
        let user_id = admin_user.id();
        let session_id = orbisync_domain::AuthSessionId::generate();
        let now = clock.now();
        let token = tokens
            .issue(user_id, session_id, now)
            .unwrap()
            .expose_secret()
            .to_owned();
        let session = orbisync_domain::AuthSession::new(
            session_id,
            user_id,
            now,
            now.checked_add_millis(900_000).unwrap(),
        )
        .unwrap();
        store.save_session(&session).await.expect("save session");
        // Need to ensure repo has session
        token
    };
    let (m, u, b) = ("GET", "/v1/roles".to_owned(), None);
    let status = test_endpoint(state.clone(), m, &u, &valid_token, b.clone()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "valid token should be 200 for GET /v1/roles"
    );
}
