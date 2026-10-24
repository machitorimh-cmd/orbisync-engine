//! Concurrent guest admission bounds real database rows and connections.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod common;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use orbisync_domain::{AuthMethod, RoleId, Timestamp};
use orbisync_identity::{
    EphemeralMethodPolicy, EphemeralSubjectService, SessionIssuer, token::AccessTokenService,
};
use orbisync_storage_postgres::PgLoginStore;
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{
    HttpState,
    login_rate_limit::{AuthIssuanceLimits, LoginRateLimiter},
    router,
};
use std::sync::Arc;
use tower::ServiceExt as _;
use uuid::Uuid;
const PRIVATE_PEM:&[u8]=b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM:&[u8]=b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";
async fn guest(state: &HttpState, peer: u8) -> (StatusCode, u64) {
    let mut request = Request::builder()
        .uri("/v1/auth/guest")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from((
            [192, 0, 2, peer],
            443,
        ))));
    let response = router(state.clone()).oneshot(request).await.unwrap();
    (
        response.status(),
        response
            .headers()
            .get("retry-after")
            .map_or(0, |v| v.to_str().unwrap().parse().unwrap()),
    )
}
#[tokio::test]
async fn concurrent_issuance_respects_ip_temporary_budget_and_database_pool_bound() {
    let Some(initial_pool) = common::pool_or_skip().await else {
        return;
    };
    let options = (*initial_pool.connect_options()).clone();
    initial_pool.close().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap();
    let now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let clock = Arc::new(FixedClock::new(Timestamp::from_offset_date_time(now)));
    let role = RoleId::generate();
    sqlx::query("INSERT INTO roles(id,name,revision) VALUES($1,$2,1)")
        .bind(role.as_uuid())
        .bind(format!("issuance-budget-{role}"))
        .execute(&pool)
        .await
        .unwrap();
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
    let issuer = Arc::new(SessionIssuer::new(
        Arc::new(PgLoginStore::new(pool.clone())),
        tokens,
        clock.clone(),
        b"test-refresh-hmac".to_vec(),
        900,
        2592000,
    ));
    let service = Arc::new(EphemeralSubjectService::new(
        issuer,
        Some(EphemeralMethodPolicy {
            method: AuthMethod::Guest,
            grant_roles: vec![role],
            session_ttl_seconds: 3600,
            allowed_worlds: vec![Uuid::now_v7()],
            display_name_prefix: "Load guest".into(),
        }),
        None,
    ));
    let state = HttpState::new(
        vec![],
        "orbisync",
        "test",
        1,
        clock,
        900,
        2592000,
        b"test-refresh-hmac".to_vec(),
    )
    .with_ephemeral_subject_service(service)
    .with_enabled_auth_methods(vec![AuthMethod::Guest])
    .with_login_rate_limiter(Arc::new(
        LoginRateLimiter::new(100, 300, 100).with_issuance_limits(AuthIssuanceLimits {
            per_ip_per_minute: 2,
            per_method_per_minute: [2; 4],
            max_concurrent: 2,
            temporary_subjects_per_minute: 3,
        }),
    ));
    let mut burst = tokio::task::JoinSet::new();
    for _ in 0..40 {
        let state = state.clone();
        burst.spawn(async move { guest(&state, 1).await });
    }
    let mut issued = 0;
    while let Some(result) = burst.join_next().await {
        let (status, retry) = result.unwrap();
        if status == StatusCode::OK {
            issued += 1;
        } else {
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            assert!(retry >= 1);
        }
    }
    assert_eq!(issued, 2);
    assert_eq!(
        guest(&state, 2).await.0,
        StatusCode::OK,
        "another source may use remaining budget"
    );
    assert_eq!(
        guest(&state, 3).await,
        (StatusCode::TOO_MANY_REQUESTS, 60),
        "temporary budget spans source IPs"
    );
    let subjects: i64 = sqlx::query_scalar("SELECT count(*) FROM user_roles WHERE role_id=$1")
        .bind(role.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    let sessions:i64=sqlx::query_scalar("SELECT count(*) FROM auth_sessions s JOIN user_roles r ON r.user_id=s.user_id WHERE r.role_id=$1").bind(role.as_uuid()).fetch_one(&pool).await.unwrap();
    let refresh:i64=sqlx::query_scalar("SELECT count(*) FROM refresh_tokens t JOIN auth_sessions s ON s.id=t.session_id JOIN user_roles r ON r.user_id=s.user_id WHERE r.role_id=$1").bind(role.as_uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (subjects, sessions, refresh),
        (3, 3, 3),
        "rejected requests cannot create partial or excess rows"
    );
    assert!(
        pool.size() <= 2,
        "database connection count stays within the configured test budget"
    );
    pool.close().await;
}
