//! Integration test for the operational HTTP endpoints.
//!
//! The router is driven through `tower::ServiceExt`, so the test exercises the
//! real Axum stack without binding a socket. Readiness is driven by a fake
//! probe from `orbisync-testkit` (`architecture.md` §9.5).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::HealthProbe;
use orbisync_testkit::FakeHealthProbe;
use orbisync_transport_http::{HttpState, X_REQUEST_ID, router};
use tower::ServiceExt as _;

fn state(probe: Arc<dyn HealthProbe>) -> HttpState {
    let clock = Arc::new(orbisync_testkit::FixedClock::new(
        orbisync_domain::Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    )) as Arc<dyn orbisync_domain::Clock>;
    HttpState::new(
        vec![probe],
        "orbisync",
        "0.1.0-test",
        1,
        clock,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
}

async fn get(state: HttpState, path: &str) -> (StatusCode, serde_json::Value) {
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let value = serde_json::from_slice(&bytes).expect("body is JSON");
    (status, value)
}

#[tokio::test]
async fn test_request_id_is_server_generated_once_and_returned() {
    let response = router(state(Arc::new(FakeHealthProbe::healthy("database"))))
        .oneshot(
            Request::builder()
                .uri("/health/ready")
                .header(X_REQUEST_ID, "attacker-controlled")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    let request_id = response
        .headers()
        .get(X_REQUEST_ID)
        .expect("response request id")
        .to_str()
        .expect("ASCII request id");
    assert_ne!(request_id, "attacker-controlled");
    let suffix = request_id.strip_prefix("req_").expect("ADR-014 prefix");
    let parsed = uuid::Uuid::parse_str(suffix).expect("UUID syntax");
    assert_eq!(parsed.get_version_num(), 7);
    assert_eq!(suffix, parsed.hyphenated().to_string());
}

#[tokio::test]
async fn test_liveness_is_independent_of_dependencies() {
    let probe = Arc::new(FakeHealthProbe::healthy("database"));
    probe.fail_with("database is not reachable");

    let (status, body) = get(state(probe), "/health/live").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "live");
}

#[tokio::test]
async fn test_readiness_is_ok_when_every_probe_passes() {
    let probe = Arc::new(FakeHealthProbe::healthy("database"));
    let (status, body) = get(state(probe), "/health/ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["failures"].as_array().expect("array").len(), 0);
}

#[tokio::test]
async fn test_readiness_reports_503_while_the_database_is_down() {
    let probe = Arc::new(FakeHealthProbe::healthy("database"));
    probe.fail_with("database is not reachable");

    let (status, body) = get(state(probe), "/health/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "SERVICE_UNAVAILABLE");
    assert_eq!(body["error"]["message"], "service is not ready");
    assert!(body["error"]["request_id"].as_str().is_some());
    assert_eq!(body["error"]["details"]["failures"][0], "database");
}

#[tokio::test]
async fn test_readiness_body_does_not_leak_the_failure_detail() {
    let probe = Arc::new(FakeHealthProbe::healthy("database"));
    probe.fail_with("postgres://orbisync:secret@db:5432/orbisync refused");

    let (_, body) = get(state(probe), "/health/ready").await;
    assert!(!body.to_string().contains("secret"));
    assert!(!body.to_string().contains("postgres://"));
}

#[tokio::test]
async fn test_version_reports_service_version_and_protocol_major() {
    let probe = Arc::new(FakeHealthProbe::healthy("database"));
    let (status, body) = get(state(probe), "/version").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["service"], "orbisync");
    assert_eq!(body["version"], "0.1.0-test");
    assert_eq!(body["protocol_major"], 1);
}
