//! Live OpenAPI route gate - W-26.
//!
//! Builds the real Axum Router the server serves and probes every operation
//! declared in `openapi/orbisync-v1.yaml` (and `openapi/orbisync-v1-planned.yaml`)
//! via `tower::ServiceExt::oneshot`.  A route is *routable* when the response is
//! neither 404 nor 405.  401/403/400/422 all count as served - we test
//! routability, not behaviour.
//!
//! This replaces the previous textual derivation (`scripts/validate_openapi_routes.py`
//! grepped `.route("...")`).  Text that never runs (helper function never called,
//! cfg-gated, shadowed Router, `//` comment) is now correctly NOT counted as
//! served, so the gate cannot be fooled by dead code.
//!
//! Preservation: implemented + planned must remain 37 paths / 46 ops, and both
//! directions must fail (declared-but-not-routable AND served-but-not-declared via
//! planned becoming routable).  Methods are distinguished.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::while_let_on_iterator,
    clippy::collapsible_if
)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use orbisync_transport_http::{HttpState, router};
use tower::ServiceExt;

const DUMMY_UUID_V7: &str = "01999999-9999-7999-8000-000000000001";

fn repo_root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"))
        .join("../..")
}

fn yaml_path(name: &str) -> PathBuf {
    repo_root().join("openapi").join(name)
}

/// Very small YAML ops extractor — mirrors the Python fallback regex so we don't
/// need a YAML crate in dev-dependencies.  It handles the shape produced by
/// `openapi/orbisync-v1*.yaml` ( `  /path:` + `    get|post|...:` ).
fn load_yaml_ops(path: &Path) -> HashSet<(String, String)> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut ops = HashSet::new();
    let mut current_path: Option<String> = None;
    for line in text.lines() {
        // path line: exactly two spaces + /... + colon
        if let Some(p) = line.strip_prefix("  /") {
            if let Some(colon) = p.find(':') {
                let rest = &p[colon..];
                if rest.trim() == ":" || rest.trim().starts_with(':') {
                    // reconstruct full path
                    let raw = format!("/{}", &p[..colon]);
                    // ensure next char after colon is not part of path (simple)
                    current_path = Some(raw);
                    continue;
                }
            }
            // fallback: regex-like detection for "  /xxx:"
            let trimmed = line.trim_start();
            if trimmed.starts_with('/') && trimmed.ends_with(':') {
                let path = trimmed.trim_end_matches(':').trim().to_string();
                // only consider if indented exactly 2
                if line.starts_with("  /") {
                    current_path = Some(path);
                }
                continue;
            }
        }
        // method line: exactly four spaces + method + colon
        if let Some(cp) = &current_path {
            let trimmed = line.trim();
            for m in ["get", "post", "put", "patch", "delete"] {
                if trimmed == format!("{m}:") {
                    ops.insert((m.to_string(), cp.clone()));
                }
            }
        }
        // alternative detection for method lines with 4-space indent
        if line.starts_with("    ") {
            let t = line.trim();
            for m in ["get", "post", "put", "patch", "delete"] {
                if t == format!("{m}:") {
                    if let Some(cp) = &current_path {
                        ops.insert((m.to_string(), cp.clone()));
                    }
                }
            }
        }
    }
    ops
}

fn load_yaml_paths(path: &Path) -> HashSet<String> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut paths = HashSet::new();
    for line in text.lines() {
        if line.starts_with("  /") && line.trim_end().ends_with(':') {
            let trimmed = line.trim();
            let p = trimmed.trim_end_matches(':').trim().to_string();
            if p.starts_with('/') {
                paths.insert(p);
            }
        }
    }
    paths
}

fn expand_path(template: &str) -> String {
    // Replace {param} with dummy UUID v7
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            // consume until }
            while let Some(n) = chars.next() {
                if n == '}' {
                    break;
                }
            }
            out.push_str(DUMMY_UUID_V7);
        } else {
            out.push(c);
        }
    }
    out
}

async fn is_routable(router: axum::Router, method: &str, path: &str) -> bool {
    let expanded = expand_path(path);
    // Build request — for POST/PUT/PATCH we send a minimal JSON body so Axum
    // routes the request (empty body may still be 400, not 404).
    let req = Request::builder()
        .uri(expanded.as_str())
        .method(method.to_ascii_uppercase().as_str())
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("request builder");
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    status != StatusCode::NOT_FOUND && status != StatusCode::METHOD_NOT_ALLOWED
}

/// Original reviewed design size.  Only change this when a new operation is
/// deliberately added to the design (planned → implemented or brand new path).
/// Implemented + planned must always sum to this so no design is lost.
/// W-29 V-05 added GET /v1/users/{user_id}/roles on an existing path, so ops
/// becomes 39 while paths stays 30.
/// ADR-026 added four authentication paths -- mode discovery plus the guest,
/// name-only and external endpoints -- each with one operation, so both counts
/// rise by four. Engine Web administration adds one permission-probe operation.
/// Operational diagnostics and ADR-007 add one path and operation each.
const ORIGINAL_PATHS: usize = 37;
const ORIGINAL_OPS: usize = 46;

#[tokio::test]
async fn openapi_implemented_ops_are_routable() {
    let implemented_path = yaml_path("orbisync-v1.yaml");
    let planned_path = yaml_path("orbisync-v1-planned.yaml");

    let implemented_ops = load_yaml_ops(&implemented_path);
    let planned_ops = load_yaml_ops(&planned_path);
    let implemented_paths = load_yaml_paths(&implemented_path);
    let planned_paths = load_yaml_paths(&planned_path);

    // Preservation: combined must be ORIGINAL_PATHS / ORIGINAL_OPS.
    // This is the single meaningful fixed total - only change the constants
    // when a new operation is deliberately added to the design.
    let combined_paths: HashSet<_> = implemented_paths.union(&planned_paths).cloned().collect();
    let combined_ops: HashSet<_> = implemented_ops.union(&planned_ops).cloned().collect();
    assert_eq!(
        combined_paths.len(),
        ORIGINAL_PATHS,
        "implemented + planned paths = {} != {} (design lost)",
        combined_paths.len(),
        ORIGINAL_PATHS
    );
    assert_eq!(
        combined_ops.len(),
        ORIGINAL_OPS,
        "implemented + planned ops = {} != {} (design lost)",
        combined_ops.len(),
        ORIGINAL_OPS
    );
    assert!(
        implemented_ops.is_disjoint(&planned_ops),
        "implemented and planned must be disjoint at op level, overlap {:?}",
        implemented_ops
            .intersection(&planned_ops)
            .collect::<Vec<_>>()
    );

    // Build the real Router the server builds (transport-http)
    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0-test",
        1,
        Arc::new(orbisync_testkit::FixedClock::new(
            orbisync_domain::Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        )) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    );
    let router = router(state);

    // 1. Every implemented op must be routable (not 404/405)
    for (method, path) in &implemented_ops {
        let routable = is_routable(router.clone(), method, path).await;
        assert!(
            routable,
            "declared but not routable (would be 404/405): {} {} - Router does not serve it (dead code or missing .route)",
            method.to_uppercase(),
            path
        );
    }

    // 2. No planned op must be routable (planned is NOT served)
    for (method, path) in &planned_ops {
        let routable = is_routable(router.clone(), method, path).await;
        assert!(
            !routable,
            "planned operation is actually routable (should be unreachable): {} {}",
            method.to_uppercase(),
            path
        );
    }

    // 3. Method distinction: for each implemented path, the *other* methods must NOT be routable.
    // This catches mutation 2 (POST /v1/users -> GET) — GET should be 404.
    for (method, path) in &implemented_ops {
        for other in ["get", "post", "put", "patch", "delete"] {
            if other == method {
                continue;
            }
            // Only test method mismatch for the same path where we know the wrong method is not declared.
            // If the path has multiple methods in implemented (it doesn't currently), skip.
            if implemented_ops.contains(&(other.to_string(), path.clone())) {
                continue;
            }
            // For planned overlap paths (/v1/users GET is planned, POST is implemented), we already assert planned GET is not routable.
            // But also verify that GET /v1/users is not routable when probing implemented POST path.
            let routable = is_routable(router.clone(), other, path).await;
            assert!(
                !routable,
                "method confusion: {} {} is routable but only {} {} is declared (method must be distinguished)",
                other.to_uppercase(),
                path,
                method.to_uppercase(),
                path
            );
        }
    }

    // 4. Extra sanity: live routable count among the universe must equal implemented count.
    // This ensures no hidden planned route became live and no implemented route silently vanished.
    let mut live_count = 0;
    for op in implemented_ops.union(&planned_ops) {
        if is_routable(router.clone(), &op.0, &op.1).await {
            live_count += 1;
        }
    }
    assert_eq!(
        live_count,
        implemented_ops.len(),
        "live routable ops among implemented+planned = {live_count} != implemented {} - hidden or missing route",
        implemented_ops.len()
    );

    println!(
        "openapi_route_gate: OK - {} implemented ops routable, {} planned ops correctly not routable, method distinction OK, combined {}/{} preserved",
        implemented_ops.len(),
        planned_ops.len(),
        ORIGINAL_PATHS,
        ORIGINAL_OPS
    );
}

#[tokio::test]
async fn openapi_fake_path_is_not_routable() {
    // Ensures that a completely unknown path is 404, so adding POST /v1/does-not-exist to the YAML would be caught
    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0-test",
        1,
        Arc::new(orbisync_testkit::FixedClock::new(
            orbisync_domain::Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        )) as Arc<dyn orbisync_domain::Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    );
    let router = router(state);
    let routable = is_routable(router, "post", "/v1/does-not-exist").await;
    assert!(
        !routable,
        "fake path /v1/does-not-exist should be 404, not routable"
    );
}
