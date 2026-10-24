//! Authorized local JWKS reload through HTTP and the real audit adapter.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityProvider, IdentityRepository};
use orbisync_domain::{AuthSession, AuthSessionId, Clock, Timestamp, UserId};
use orbisync_identity::{JwtIdentityProvider, token::AccessTokenService};
use orbisync_storage_postgres::{PgExternalKeyAuditStore, PgIdentityRepository};
use orbisync_testkit::{
    FixedClock,
    external_idp::{LocalSigningIssuer, TestClaims},
};
use orbisync_transport_http::{HttpState, router};
use std::sync::Arc;
use tower::ServiceExt as _;
use uuid::Uuid;

const PRIVATE_PEM:&[u8]=b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM:&[u8]=b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";
struct KeyFile(std::path::PathBuf);
impl Drop for KeyFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            eprintln!("test key fixture cleanup failed: {error}");
        }
    }
}
async fn reload(state: &HttpState, token: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .uri("/v1/auth/external/keys/reload")
        .method("POST");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn key_reload_requires_separate_permission_and_retains_keys_on_invalid_replacement() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let db_now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let clock = Arc::new(FixedClock::new(Timestamp::from_offset_date_time(db_now)));
    let now = clock.now();
    let user = UserId::generate();
    sqlx::query("INSERT INTO users(id,login_id,display_name,status,must_change_password,revision,created_at,updated_at) VALUES($1,$2,'Reload operator','active',false,1,$3,$3)")
        .bind(user.as_uuid()).bind(format!("reload-{user}")).bind(db_now).execute(&pool).await.unwrap();
    let session = AuthSession::new(
        AuthSessionId::generate(),
        user,
        now,
        Timestamp::from_offset_date_time(db_now + time::Duration::hours(1)),
    )
    .unwrap();
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
    repo.save_session(&session).await.unwrap();
    let tokens = Arc::new(
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test",
        )
        .unwrap(),
    );
    let token = tokens.issue(user, session.id(), now).unwrap();
    let key_file =
        KeyFile(std::env::temp_dir().join(format!("orbisync-key-reload-{}.json", Uuid::now_v7())));
    let old = LocalSigningIssuer::new("old", [11; 32]);
    let new = LocalSigningIssuer::new("new", [12; 32]);
    old.write_jwks(&key_file.0);
    let provider = Arc::new(
        JwtIdentityProvider::from_jwks_file(
            &key_file.0,
            "https://idp.test".into(),
            "engine".into(),
            "EdDSA",
            0,
        )
        .unwrap(),
    );
    let base = HttpState::new(
        vec![],
        "orbisync",
        "test",
        1,
        clock,
        900,
        2592000,
        b"refresh-test-key".to_vec(),
    )
    .with_token_service(tokens)
    .with_identity_repository(repo);
    let state = base.clone().with_external_key_management(
        provider.clone(),
        Arc::new(PgExternalKeyAuditStore::new(pool.clone())),
    );
    assert_eq!(reload(&state, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        reload(&state, Some(token.expose_secret())).await.0,
        StatusCode::FORBIDDEN
    );
    let role = Uuid::now_v7();
    sqlx::query("INSERT INTO roles(id,name,description,revision) VALUES($1,$2,'reload-only',1)")
        .bind(role)
        .bind(format!("reload-role-{role}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO permissions(name) VALUES('admin.auth.keys.reload') ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions(role_id,permission_name) VALUES($1,'admin.auth.keys.reload')",
    )
    .bind(role)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_roles(user_id,role_id) VALUES($1,$2)")
        .bind(user.as_uuid())
        .bind(role)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = reload(&base, Some(token.expose_secret())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "AUTH_METHOD_DISABLED");
    new.write_jwks(&key_file.0);
    let (status, body) = reload(&state, Some(token.expose_secret())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["generation"], 2);
    assert_eq!(body["key_count"], 1);
    assert_eq!(body["key_ids"], serde_json::json!(["new"]));
    let seconds = db_now.unix_timestamp();
    let claims = TestClaims {
        iss: "https://idp.test".into(),
        sub: "subject".into(),
        aud: "engine".into(),
        exp: seconds + 3600,
        nbf: seconds - 60,
        iat: seconds - 60,
        email: None,
        role: None,
    };
    assert!(provider.verify(&old.sign(&claims), now).await.is_err());
    provider.verify(&new.sign(&claims), now).await.unwrap();
    for bad in [
        "not JSON",
        r#"{"keys":[{"kid":"bad","alg":"EdDSA","kty":"OKP","x":"AA"}]}"#,
    ] {
        std::fs::write(&key_file.0, bad).unwrap();
        assert_eq!(
            reload(&state, Some(token.expose_secret())).await.0,
            StatusCode::BAD_REQUEST
        );
        provider
            .verify(&new.sign(&claims), now)
            .await
            .expect("invalid candidate must retain the active keys");
    }
    let events:Vec<(String,serde_json::Value,String)>=sqlx::query_as("SELECT result,metadata,request_id FROM audit_events WHERE actor_user_id=$1 AND action='auth.external_keys.reload' ORDER BY id")
        .bind(user.as_uuid()).fetch_all(&pool).await.unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].0, "success");
    assert!(events[1..].iter().all(|event| event.0 == "failure"));
    assert!(
        events
            .iter()
            .all(|event| event.1["generation"] == 2 && event.2.starts_with("req_"))
    );
    let evidence = serde_json::to_string(&events).unwrap();
    for forbidden in [
        token.expose_secret(),
        key_file.0.to_str().unwrap(),
        "PRIVATE KEY",
        "refresh_token",
    ] {
        assert!(!evidence.contains(forbidden));
    }
}
