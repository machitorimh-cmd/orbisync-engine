#[tokio::test]
#[allow(clippy::panic)] // Test-only sentinel: issuance must never consume a ticket.
async fn admission88_ticket_storage_finishes_accepted_request_and_refuses_new_one() {
    use orbisync_application::{IdentityRepository, RealtimeTicketStore};
    use orbisync_domain::{AuthSession, AuthSessionId, Clock, UserId};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Tickets {
        calls: AtomicUsize,
        committed: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl RealtimeTicketStore for Tickets {
        async fn create(
            &self,
            _: orbisync_application::CreateRealtimeTicketCommand,
        ) -> Result<(), orbisync_application::IdentityPortError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.acquire().await.expect("gate").forget();
            }
            self.committed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn consume(
            &self,
            _: [u8; 32],
            _: orbisync_domain::Timestamp,
        ) -> Result<
            orbisync_application::RealtimeTicketConsumption,
            orbisync_application::IdentityPortError,
        > {
            panic!("HTTP ticket issuance is not connection acceptance");
        }
    }
    let tickets = Arc::new(Tickets {
        calls: AtomicUsize::new(0),
        committed: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    // Public fixture key also used by the existing auth_w13 contract tests.
    let tokens = Arc::new(orbisync_identity::token::AccessTokenService::from_ed25519_private_pem(
            b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n",
            "orbisync", "orbisync-api", "test-key-1",
        ).expect("tokens"));
    let repo = Arc::new(orbisync_testkit::FakeIdentityRepository::new());
    let now = SystemClock::new().now();
    let user = UserId::generate();
    let session = AuthSessionId::generate();
    repo.save_session(
        &AuthSession::new(
            session,
            user,
            now,
            now.checked_add_millis(60_000).expect("expiry"),
        )
        .expect("session"),
    )
    .await
    .expect("save session");
    let token = tokens.issue(user, session, now).expect("token");
    let flag = Arc::new(AtomicBool::new(false));
    let app = router(
        test_state()
            .with_shutdown_flag(flag.clone())
            .with_token_service(tokens)
            .with_identity_repository(repo)
            .with_realtime_ticket_hmac_key(vec![7; 32])
            .with_realtime_ticket_store(tickets.clone()),
    );
    let request = || {
        Request::post("/v1/realtime/tickets")
            .header("authorization", format!("Bearer {}", token.expose_secret()))
            .body(Body::empty())
            .expect("request")
    };
    let accepted = tokio::spawn(app.clone().oneshot(request()));
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tickets.entered.notified(),
    )
    .await
    .expect("accepted storage entry");
    flag.store(true, Ordering::Release);
    let refused = app.oneshot(request()).await.expect("new response");
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        tickets.calls.load(Ordering::SeqCst),
        1,
        "new request never entered storage"
    );
    assert_eq!(tickets.committed.load(Ordering::SeqCst), 0);
    tickets.release.add_permits(1);
    assert_eq!(
        accepted
            .await
            .expect("task")
            .expect("accepted response")
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        tickets.committed.load(Ordering::SeqCst),
        1,
        "accepted storage not cancelled"
    );
}

#[tokio::test]
async fn admission88_shutdown_rejects_new_http_before_body_poll() {
    use axum::http::Method;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct ObservedBody(Arc<AtomicUsize>);
    impl http_body::Body for ObservedBody {
        type Data = axum::body::Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(None)
        }
    }
    let polls = Arc::new(AtomicUsize::new(0));
    let app = router(test_state().with_shutdown_flag(Arc::new(AtomicBool::new(true))));
    for (method, path) in [
        (Method::POST, "/v1/realtime/tickets"),
        (Method::POST, "/v1/auth/login"),
        (Method::GET, "/v1/worlds"),
        (Method::POST, "/v1/instances"),
    ] {
        let body = Body::new(ObservedBody(polls.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(body)
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
    }
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "no body/auth/storage admission"
    );
}
