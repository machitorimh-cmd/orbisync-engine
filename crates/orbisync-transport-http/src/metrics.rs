//! Metrics endpoint and HTTP observability middleware.

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use crate::HttpState;
use orbisync_application::metrics::{
    Counter, Histogram, HttpMethod, HttpStatusClass, RateLimitScope,
};

/// Returns true when the peer is allowed to scrape `/metrics`.
///
/// Allowed: loopback and private networks (10/8, 172.16/12, 192.168/16) and
/// link-local. Everything else is denied. This matches ADR-009
/// localhost/internal network limitation without trusting `X-Forwarded-For`.
#[must_use]
pub fn is_allowed_metrics_peer(addr: SocketAddr) -> bool {
    let ip = addr.ip();
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// `GET /metrics` – Prometheus exposition format.
///
/// Authentication is not required, but the peer must be on the internal
/// network (ADR-009). `X-Forwarded-For` is ignored – only the transport peer
/// (`ConnectInfo`) is checked.
pub async fn handler(
    State(state): State<HttpState>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    let Some(peer) = peer else {
        // Without peer info we cannot verify internal network – deny.
        // In tests the peer is injected via `Request::extensions_mut().insert(ConnectInfo(...))`.
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    };
    if !is_allowed_metrics_peer(peer) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let exporter = state.metrics_exporter();
    let body = exporter.render();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

/// Middleware that records `http_requests_total` and `http_request_duration_seconds`.
///
/// It uses `State<HttpState>` to obtain the recorder; if no recorder is
/// wired it is a no-op. The middleware is added via
/// `from_fn_with_state` so it can read `HttpState`.
pub async fn middleware(
    State(state): State<HttpState>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Extract method before moving request.
    let method_str = req.method().to_string();
    let rec = state.metrics_recorder();
    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16();
    let method = HttpMethod::from_method(&method_str);
    let status_class = HttpStatusClass::from_status(status);
    rec.incr(Counter::HttpRequests {
        method,
        status: status_class,
    });
    rec.observe(Histogram::HttpRequestDuration { method }, elapsed);
    response
}

/// Helper to record a rate-limit rejection for the given scope.
pub fn record_rate_limit_rejected(state: &HttpState, scope: RateLimitScope) {
    state
        .metrics_recorder()
        .incr(Counter::RateLimitRejected { scope });
}

/// Helper to record a login failure.
pub fn record_login_failure(state: &HttpState) {
    state.metrics_recorder().incr(Counter::AuthLoginFailures);
}

#[cfg(test)]
mod tests {
    use super::is_allowed_metrics_peer;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    #[test]
    fn loopback_allowed() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1234);
        assert!(is_allowed_metrics_peer(addr));
        let addr6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 1234);
        assert!(is_allowed_metrics_peer(addr6));
    }

    #[test]
    fn private_allowed() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 1234);
        assert!(is_allowed_metrics_peer(addr));
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 1234);
        assert!(is_allowed_metrics_peer(addr));
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 5, 4)), 1234);
        assert!(is_allowed_metrics_peer(addr));
    }

    #[test]
    fn external_denied() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 1234);
        assert!(!is_allowed_metrics_peer(addr));
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 1234);
        assert!(!is_allowed_metrics_peer(addr));
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod handler_tests {
    use crate::{HttpState, router};
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::metrics::{
        Counter, Gauge, Histogram, MetricsExporter, MetricsRecorder,
    };
    use orbisync_domain::{Clock, SystemClock};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use tower::ServiceExt as _;

    #[derive(Debug, Default)]
    struct FakeMetrics {
        http_requests: AtomicU64,
        http_duration_count: AtomicU64,
        auth_failures: AtomicU64,
        rate_limit: AtomicU64,
        db_duration_count: AtomicU64,
    }

    impl MetricsRecorder for FakeMetrics {
        fn incr(&self, counter: Counter) {
            match counter {
                Counter::HttpRequests { .. } => {
                    self.http_requests.fetch_add(1, Ordering::SeqCst);
                }
                Counter::AuthLoginFailures => {
                    self.auth_failures.fetch_add(1, Ordering::SeqCst);
                }
                Counter::RateLimitRejected { .. } => {
                    self.rate_limit.fetch_add(1, Ordering::SeqCst);
                }
                Counter::InstanceMailboxSaturated { .. }
                | Counter::InstanceMailboxDropped { .. }
                | Counter::ExtensionDelivery { .. }
                | Counter::ExtensionDeliveryWorkerFailure
                | Counter::ExtensionOutboxDroppedTotal
                | Counter::ExtensionOutboxDeletedTotal
                | Counter::WebsocketConnectionsTotal
                | Counter::WebsocketDisconnects { .. }
                | Counter::InstanceCommandsTotal
                | Counter::StateUpdatesDroppedTotal
                | Counter::BroadcastCellCandidatesTotal
                | Counter::BroadcastFullScanTotal
                | Counter::ResumeAttemptsTotal
                | Counter::ResumeSuccessTotal
                | Counter::SnapshotBytesTotal
                | Counter::DeltaBytesTotal
                | Counter::EntityPersistenceFailuresTotal
                | Counter::CheckpointSaveRejectedTotal
                | Counter::CheckpointRestoreRejectedTotal
                | Counter::CheckpointSaveFailuresTotal
                | Counter::CheckpointRestoreFailuresTotal => {}
            }
        }
        fn add(&self, counter: Counter, n: u64) {
            match counter {
                Counter::HttpRequests { .. } => {
                    self.http_requests.fetch_add(n, Ordering::SeqCst);
                }
                Counter::AuthLoginFailures => {
                    self.auth_failures.fetch_add(n, Ordering::SeqCst);
                }
                Counter::RateLimitRejected { .. } => {
                    self.rate_limit.fetch_add(n, Ordering::SeqCst);
                }
                Counter::InstanceMailboxSaturated { .. }
                | Counter::InstanceMailboxDropped { .. }
                | Counter::ExtensionDelivery { .. }
                | Counter::ExtensionDeliveryWorkerFailure
                | Counter::ExtensionOutboxDroppedTotal
                | Counter::ExtensionOutboxDeletedTotal
                | Counter::WebsocketConnectionsTotal
                | Counter::WebsocketDisconnects { .. }
                | Counter::InstanceCommandsTotal
                | Counter::StateUpdatesDroppedTotal
                | Counter::BroadcastCellCandidatesTotal
                | Counter::BroadcastFullScanTotal
                | Counter::ResumeAttemptsTotal
                | Counter::ResumeSuccessTotal
                | Counter::SnapshotBytesTotal
                | Counter::DeltaBytesTotal
                | Counter::EntityPersistenceFailuresTotal
                | Counter::CheckpointSaveRejectedTotal
                | Counter::CheckpointRestoreRejectedTotal
                | Counter::CheckpointSaveFailuresTotal
                | Counter::CheckpointRestoreFailuresTotal => {}
            }
        }
        fn set(&self, _gauge: Gauge, _value: i64) {}
        fn observe(&self, histogram: Histogram, _value: f64) {
            match histogram {
                Histogram::HttpRequestDuration { .. } => {
                    self.http_duration_count.fetch_add(1, Ordering::SeqCst);
                }
                Histogram::DbQueryDuration => {
                    self.db_duration_count.fetch_add(1, Ordering::SeqCst);
                }
                Histogram::InterestVisibleSetSize
                | Histogram::TickDuration
                | Histogram::RealtimeApplicationDuration => {}
            }
        }
    }

    impl MetricsExporter for FakeMetrics {
        fn render(&self) -> String {
            // Minimal Prometheus exposition containing the 5 required metrics
            let mut s = String::new();
            s.push_str("# HELP http_requests_total Number of HTTP requests\n");
            s.push_str("# TYPE http_requests_total counter\n");
            s.push_str(&format!(
                "http_requests_total{{method=\"GET\",status=\"2xx\"}} {}\n",
                self.http_requests.load(Ordering::SeqCst)
            ));
            s.push_str("# HELP http_request_duration_seconds HTTP duration\n");
            s.push_str("# TYPE http_request_duration_seconds histogram\n");
            s.push_str("http_request_duration_seconds_bucket{method=\"GET\",le=\"0.005\"} 0\n");
            s.push_str("http_request_duration_seconds_sum 0\n");
            s.push_str("http_request_duration_seconds_count 0\n");
            s.push_str("# HELP auth_login_failures_total Failures\n");
            s.push_str("# TYPE auth_login_failures_total counter\n");
            s.push_str(&format!(
                "auth_login_failures_total {}\n",
                self.auth_failures.load(Ordering::SeqCst)
            ));
            s.push_str("# HELP db_query_duration_seconds DB duration\n");
            s.push_str("# TYPE db_query_duration_seconds histogram\n");
            s.push_str("db_query_duration_seconds_bucket{le=\"0.005\"} 0\n");
            s.push_str("db_query_duration_seconds_sum 0\n");
            s.push_str("db_query_duration_seconds_count 0\n");
            s.push_str("# HELP rate_limit_rejected_total Rejected\n");
            s.push_str("# TYPE rate_limit_rejected_total counter\n");
            s.push_str(&format!(
                "rate_limit_rejected_total{{scope=\"user\"}} {}\n",
                self.rate_limit.load(Ordering::SeqCst)
            ));
            s.push_str("# HELP process_cpu_seconds_total CPU\n");
            s.push_str("# TYPE process_cpu_seconds_total counter\n");
            s.push_str("process_cpu_seconds_total 0\n");
            s.push_str("# HELP process_resident_memory_bytes RSS\n");
            s.push_str("# TYPE process_resident_memory_bytes gauge\n");
            s.push_str("process_resident_memory_bytes 0\n");
            s.push_str("# EOF\n");
            s
        }
    }

    fn build_state(metrics: Arc<FakeMetrics>) -> HttpState {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let recorder: Arc<dyn MetricsRecorder> = metrics.clone();
        let exporter: Arc<dyn MetricsExporter> = metrics.clone();
        HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock,
            900,
            2_592_000,
            b"test-refresh-hmac-key-32b!!".to_vec(),
        )
        .with_metrics_recorder(recorder)
        .with_metrics_exporter(exporter)
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_format_for_loopback() {
        let metrics = Arc::new(FakeMetrics::default());
        let state = build_state(metrics.clone());
        let app = router(state);
        let mut req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            12345,
        )));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("http_requests_total"),
            "missing http_requests_total"
        );
        assert!(
            text.contains("http_request_duration_seconds"),
            "missing http_request_duration_seconds"
        );
        assert!(
            text.contains("auth_login_failures_total"),
            "missing auth_login_failures_total"
        );
        assert!(
            text.contains("db_query_duration_seconds"),
            "missing db_query_duration_seconds"
        );
        assert!(
            text.contains("rate_limit_rejected_total"),
            "missing rate_limit_rejected_total"
        );
        assert!(
            text.contains("process_cpu_seconds_total"),
            "missing process_cpu_seconds_total"
        );
        assert!(
            text.contains("process_resident_memory_bytes"),
            "missing process_resident_memory_bytes"
        );
        assert!(text.contains("# EOF"), "missing EOF");
    }

    #[tokio::test]
    async fn metrics_endpoint_denied_for_external_peer() {
        let metrics = Arc::new(FakeMetrics::default());
        let state = build_state(metrics);
        let app = router(state);
        let mut req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            12345,
        )));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn metrics_endpoint_denied_without_connect_info() {
        let metrics = Arc::new(FakeMetrics::default());
        let state = build_state(metrics);
        let app = router(state);
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn metrics_middleware_increments_http_requests() {
        let metrics = Arc::new(FakeMetrics::default());
        let state = build_state(metrics.clone());
        let app = router(state);
        let mut req = Request::builder()
            .uri("/health/live")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            12345,
        )));
        // Also need loopback for metrics? No, /health/live is allowed without metrics check
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Middleware should have incremented http_requests and observed duration
        assert_eq!(metrics.http_requests.load(Ordering::SeqCst), 1);
        assert_eq!(metrics.http_duration_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn x_forwarded_for_does_not_bypass_metrics_access_control() {
        let metrics = Arc::new(FakeMetrics::default());
        let state = build_state(metrics);
        let app = router(state);
        let mut req = Request::builder()
            .uri("/metrics")
            .header("x-forwarded-for", "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        // Peer is external, but XFF is loopback – must still be denied
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            12345,
        )));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "X-Forwarded-For must not bypass peer check"
        );
    }
}
