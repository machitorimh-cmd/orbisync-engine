//! CR-14: Clock is required in HttpState, no fallback to fixed timestamp.
//! This test verifies that after making `clock` a required constructor argument,
//! constructing `HttpState` without a clock is a compile-time error (see comment
//! below) and that `FixedClock` correctly enforces token expiry.
//!
//! Previous code used `state.clock().map(|c| c.now()).unwrap_or_else(|| Timestamp::from_unix_millis(1_700_000_000_000))`
//! which would silently accept expired tokens when the clock was not wired.
//! After the fix, `HttpState::new` requires `Arc<dyn Clock>` and there is no
//! fallback; time checks use `state.clock().now()` directly.

#![allow(clippy::expect_used, clippy::unwrap_used, missing_docs, unused_imports)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityAdministrationStore, IdentityRepository, RequestId};
use orbisync_domain::{Clock, LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
    PasswordService, token::AccessTokenService,
};
use orbisync_testkit::{FakeIdentityStore, FixedClock};
use orbisync_transport_http::{HttpState, router};
use tower::ServiceExt as _;
use uuid::Uuid;

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
        .expect("token service"),
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

/// HttpState cannot be constructed without a clock – this is enforced by the
/// type system. The following would not compile after CR-14:
///
/// ```compile_fail
/// let _state = HttpState::new(vec![], "orbisync", "0.1.0", 1, 900, 2_592_000, b"test-hmac-key-for-unit-tests-32b!!".to_vec());
/// // missing 5th argument `clock: Arc<dyn Clock>` -> compile error
/// ```
///
/// Therefore there is no runtime fallback to `1_700_000_000_000`; a missing
/// clock is a compile-time failure, not a silent 401 bypass.

#[tokio::test]
async fn cr14_fixed_clock_past_makes_expired_token_401() {
    // Use a time far from 1_700_000_000_000 so that fixed fallback would be wrong.
    // Issue at 1_700_000_000_000, expire at 1_700_000_900, check at 1_700_002_000 (33 min later).
    // Fixed fallback is 1_700_000_000_000, so it would think token is still valid.
    let issued_at = Timestamp::from_unix_millis(1_700_000_000_000).expect("valid");
    let future_now = Timestamp::from_unix_millis(1_700_000_000_000 + 2_000_000).expect("valid");
    let svc = token_service();
    let clock_issued = Arc::new(FixedClock::new(issued_at));
    let clock_future = Arc::new(FixedClock::new(future_now));

    // Bootstrap a real admin and create a real session so that expiry is the only reason for 401
    let store = Arc::new(FakeIdentityStore::new());
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    let admin_port: Arc<dyn IdentityAdministrationStore> =
        store.clone() as Arc<dyn IdentityAdministrationStore>;
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(clock_issued.clone() as Arc<dyn Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let (admin_user, _) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        )
        .await
        .expect("bootstrap");
    let admin_id = admin_user.id();

    // Issue a token for admin at issued_at and persist the session
    let session_id = orbisync_domain::AuthSessionId::generate();
    let token = svc
        .issue(admin_id, session_id, issued_at)
        .expect("issue")
        .expose_secret()
        .to_owned();
    let expires_at = issued_at.checked_add_millis(900_000).unwrap();
    let session = orbisync_domain::AuthSession::new(session_id, admin_id, issued_at, expires_at)
        .expect("session");
    // Save session via the store (FakeIdentityStore implements IdentityRepository)
    store.save_session(&session).await.expect("save session");

    // Validate via service directly: at issued_at it should succeed, at future_now it should fail
    assert!(
        svc.validate(
            &orbisync_application::SecretString::new(token.clone()),
            issued_at
        )
        .is_ok()
    );
    assert!(
        svc.validate(
            &orbisync_application::SecretString::new(token.clone()),
            future_now
        )
        .is_err()
    );

    // Now verify via HTTP handler that the future clock correctly rejects the expired token.
    // The handler uses `state.clock().now()` which should be future_now, so it will be 401.
    // If `authenticate` were changed to `Timestamp::from_unix_millis(1_700_000_000_000)` (fixed),
    // it would incorrectly think `now` is still issued_at and return 200.
    let state_future = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock_future) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&svc),
        Arc::clone(&clock_future),
    ))
    .with_token_service(Arc::clone(&svc))
    .with_identity_repository(Arc::clone(&repo));
    // Need audit/identity query for /v1/roles handler (it checks permission via roles)
    let q: Arc<dyn orbisync_application::IdentityQueryPort> =
        store.clone() as Arc<dyn orbisync_application::IdentityQueryPort>;
    let state_future = state_future.with_identity_query(q);
    let app = router(state_future);
    let req = Request::builder()
        .uri("/v1/roles")
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "expired token should be 401 via HTTP when clock is future"
    );

    // Also verify that with the issued clock, the same token is OK (200)
    let state_issued = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock_issued) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&svc),
        Arc::clone(&clock_issued),
    ))
    .with_token_service(Arc::clone(&svc))
    .with_identity_repository(Arc::clone(&repo));
    let q2: Arc<dyn orbisync_application::IdentityQueryPort> =
        store.clone() as Arc<dyn orbisync_application::IdentityQueryPort>;
    let state_issued = state_issued.with_identity_query(q2);
    let app2 = router(state_issued);
    let req2 = Request::builder()
        .uri("/v1/roles")
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "token should be 200 when validated at issued time"
    );
}

#[tokio::test]
async fn cr14_future_clock_makes_not_yet_valid_token_401() {
    // Use times far from 1_700_000_000_000 so that `let now = Timestamp::from_unix_millis(1_700_000_000_000)`
    // mutation is detectable. Token window is at 1_800_000_100_000; fixed fallback at
    // 1_700_000_000_000 is before nbf, so the valid case would incorrectly be 401.
    let past = Timestamp::from_unix_millis(1_800_000_000_000).expect("valid");
    let future = Timestamp::from_unix_millis(1_800_000_100_000).expect("valid");
    let svc = token_service();
    let clock_past = Arc::new(FixedClock::new(past));
    let clock_future = Arc::new(FixedClock::new(future));

    // Bootstrap admin on the future clock so the persisted user/session align with token time
    let store = Arc::new(FakeIdentityStore::new());
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
    let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
    let admin_port: Arc<dyn IdentityAdministrationStore> =
        store.clone() as Arc<dyn IdentityAdministrationStore>;
    let dyn_store = DynIdentityAdministrationStore(admin_port);
    let dyn_clock = DynClock(clock_future.clone() as Arc<dyn Clock>);
    let admin = Arc::new(IdentityAdministrationService::new(
        Arc::new(dyn_store),
        Arc::new(dyn_clock),
        (*passwords).clone(),
    ));
    let (admin_user, _) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        )
        .await
        .expect("bootstrap");
    let admin_id = admin_user.id();

    // Issue a token at the future nbf and persist the session starting at future
    let session_id = orbisync_domain::AuthSessionId::generate();
    let token = svc
        .issue(admin_id, session_id, future)
        .expect("issue at future")
        .expose_secret()
        .to_owned();
    let expires_at = future.checked_add_millis(900_000).unwrap();
    let session = orbisync_domain::AuthSession::new(session_id, admin_id, future, expires_at)
        .expect("session");
    store.save_session(&session).await.expect("save session");

    // Direct service sanity: past should be invalid (nbf), future should be valid
    assert!(
        svc.validate(
            &orbisync_application::SecretString::new(token.clone()),
            past
        )
        .is_err(),
        "token not yet valid at past"
    );
    assert!(
        svc.validate(
            &orbisync_application::SecretString::new(token.clone()),
            future
        )
        .is_ok(),
        "token should be valid at its nbf"
    );

    // HTTP via past clock should be 401 (nbf not yet valid)
    // If `authenticate` used fixed 1_700_000_000_000, this would still be 401, so we also need the
    // future case: with the future clock it should be 200, but with fixed it would be 401 -> red.
    let state_past = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock_past) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&svc),
        Arc::clone(&clock_past),
    ))
    .with_token_service(Arc::clone(&svc))
    .with_identity_repository(Arc::clone(&repo));
    let q: Arc<dyn orbisync_application::IdentityQueryPort> =
        store.clone() as Arc<dyn orbisync_application::IdentityQueryPort>;
    let state_past = state_past.with_identity_query(q);
    let app = router(state_past);
    let req = Request::builder()
        .uri("/v1/roles")
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "not-yet-valid token should be 401 via HTTP when clock is past (1_800_000_000_000)"
    );

    // HTTP via future clock should be 200
    // With fixed 1_700_000_000_000 this would be 401 instead of 200, making the test fail (red).
    let state_future = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        1,
        Arc::clone(&clock_future) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_login_service(test_login_service(
        Arc::clone(&store),
        Arc::clone(&passwords),
        Arc::clone(&svc),
        Arc::clone(&clock_future),
    ))
    .with_token_service(Arc::clone(&svc))
    .with_identity_repository(Arc::clone(&repo));
    let q2: Arc<dyn orbisync_application::IdentityQueryPort> =
        store.clone() as Arc<dyn orbisync_application::IdentityQueryPort>;
    let state_future = state_future.with_identity_query(q2);
    let app2 = router(state_future);
    let req2 = Request::builder()
        .uri("/v1/roles")
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "token should be 200 when validated at its nbf time (1_800_000_100_000)"
    );
}
