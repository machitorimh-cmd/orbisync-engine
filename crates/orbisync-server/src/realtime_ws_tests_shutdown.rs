fn admission88_state() -> RealtimeState {
    RealtimeState::new(
        orbisync_config::Config::default().realtime,
        Arc::new(RuntimeRegistry::new()),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
}

fn admission88_hello() -> Message {
    Message::Binary(
        Envelope {
            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
            sequence: 1,
            payload: Some(envelope::Payload::ClientHello(
                orbisync_protocol::v1::ClientHello {
                    realtime_ticket: "test-ticket".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
        .encode_to_vec()
        .into(),
    )
}

struct Admission88Verifier {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl orbisync_realtime::gateway::RealtimeTicketVerifier for Admission88Verifier {
    async fn verify(
        &self,
        _ticket: &str,
    ) -> Result<UserId, orbisync_realtime::gateway::GatewayError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.expect("release").forget();
        Ok(UserId::generate())
    }
}

#[tokio::test]
async fn admission88_hello_after_begin_does_not_consume_ticket_without_notice() {
    let mut state = admission88_state();
    let calls = Arc::new(AtomicUsize::new(0));
    state.tickets = Arc::new(Admission88Verifier {
        calls: calls.clone(),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Semaphore::new(1)),
    });
    let state = Arc::new(state);
    let (socket, input, _output) = RealtimeSocket::controlled();
    let auth = authenticate_connection(
        socket,
        state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    );
    tokio::pin!(auth);
    // Poll authentication to its waiting-for-hello boundary, then begin
    // shutdown without broadcasting Begin (the actual notification gap).
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(auth.as_mut().poll(cx).is_pending()))
            .await
    );
    state.shutdown.begin();
    input.send(admission88_hello()).await.expect("hello");
    assert!(auth.await.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn admission88_shutdown_during_ticket_consume_has_no_server_hello() {
    let mut state = admission88_state();
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    state.tickets = Arc::new(Admission88Verifier {
        calls: calls.clone(),
        entered: entered.clone(),
        release: release.clone(),
    });
    let state = Arc::new(state);
    let (socket, input, mut output) = RealtimeSocket::controlled();
    input.send(admission88_hello()).await.expect("hello");
    let auth = tokio::spawn(authenticate_connection(
        socket,
        state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    ));
    entered.notified().await;
    state.shutdown.begin();
    release.add_permits(1);
    assert!(auth.await.expect("auth task").is_none());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "accepted ticket consumption finishes"
    );
    while let Some(message) = output.recv().await {
        if let Message::Binary(bytes) = message {
            assert!(!matches!(
                decode_envelope(&bytes).expect("envelope").payload,
                Some(envelope::Payload::ServerHello(_))
            ));
        }
    }
}

#[tokio::test]
async fn admission88_join_and_resume_after_begin_rejected_without_notice() {
    let mut state = admission88_state();
    state.tickets = Arc::new(orbisync_realtime::gateway::StubTicketVerifier::new());
    let state = Arc::new(state);
    let (socket, input, mut output) = RealtimeSocket::controlled();
    let task = tokio::spawn(handle_socket(
        socket,
        state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    ));
    input.send(admission88_hello()).await.expect("hello");
    let Message::Binary(bytes) = output.recv().await.expect("server hello") else {
        panic!("binary");
    };
    assert!(matches!(
        decode_envelope(&bytes).expect("hello").payload,
        Some(envelope::Payload::ServerHello(_))
    ));
    state.shutdown.begin();
    input
        .send(Message::Binary(
            Envelope {
                protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                sequence: 2,
                payload: Some(envelope::Payload::JoinInstance(
                    orbisync_protocol::v1::JoinInstance {
                        world_instance_id: InstanceId::generate().to_string(),
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .expect("join");
    let Message::Binary(bytes) = output.recv().await.expect("refusal") else {
        panic!("binary");
    };
    assert!(matches!(decode_envelope(&bytes).expect("refusal").payload,
            Some(envelope::Payload::Error(error)) if error.code == "SERVER_SHUTTING_DOWN"));
    assert!(state.registry.is_empty().expect("registry"));
    input
        .send(Message::Binary(
            Envelope {
                protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                sequence: 3,
                payload: Some(envelope::Payload::ResumeSession(
                    orbisync_protocol::v1::ResumeSession {
                        resume_token: "unused".into(),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .expect("resume");
    let Message::Binary(bytes) = output.recv().await.expect("resume refusal") else {
        panic!("binary");
    };
    assert!(matches!(decode_envelope(&bytes).expect("refusal").payload,
            Some(envelope::Payload::Error(error)) if error.code == "SERVER_SHUTTING_DOWN"));
    drop(input);
    task.await.expect("socket task");
}

#[tokio::test]
async fn admission88_upgrade_refused_before_connection_permit_or_task() {
    use axum::extract::FromRequestParts;
    let state = Arc::new(admission88_state());
    let permits = state.connection_semaphore.available_permits();
    let mut request = axum::http::Request::builder()
        .uri("/ws")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .expect("request");
    // Extract an upgrade-capable request without opening a listener.
    let upgrade = hyper::upgrade::on(&mut request);
    request.extensions_mut().insert(upgrade);
    let (mut parts, ()) = request.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .expect("upgrade extractor");
    state.shutdown.begin();
    let response = realtime_ws_handler(ws, parts.headers, State(state.clone()))
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(state.connection_semaphore.available_permits(), permits);
    assert_eq!(state.active_connections.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn admission88_accepted_socket_join_finishes_but_next_command_is_refused() {
    struct Store {
        entered: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl CheckpointStore for Store {
        async fn load_latest(
            &self,
            _: InstanceId,
        ) -> Result<
            Option<orbisync_application::AppCheckpoint>,
            orbisync_application::ApplicationError,
        > {
            self.entered.notify_one();
            self.release.acquire().await.expect("gate").forget();
            Ok(None)
        }
        async fn save_checkpoint(
            &self,
            _: orbisync_application::AppCheckpoint,
        ) -> Result<
            Vec<orbisync_application::CheckpointSaveReceipt>,
            orbisync_application::ApplicationError,
        > {
            panic!("socket join must not save a legacy checkpoint");
        }
    }
    let (_, instance, world, _, _) = snapshot_fixture();
    let id = instance.id();
    let directory = Arc::new(orbisync_testkit::FakeWorldDirectoryStore::new());
    directory.insert_world(world);
    directory.insert_instance(instance);
    let store = Arc::new(Store {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut state = admission88_state();
    state.world_store = directory;
    state.checkpoint_store = Some(store.clone());
    state.tickets = Arc::new(orbisync_realtime::gateway::StubTicketVerifier::new());
    let state = Arc::new(state);
    let (socket, input, mut output) = RealtimeSocket::controlled();
    let task = tokio::spawn(handle_socket(
        socket,
        state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    ));
    input.send(admission88_hello()).await.expect("hello");
    assert!(output.recv().await.is_some());
    input
        .send(Message::Binary(
            Envelope {
                protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                sequence: 2,
                payload: Some(envelope::Payload::JoinInstance(
                    orbisync_protocol::v1::JoinInstance {
                        world_instance_id: id.to_string(),
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .expect("join");
    store.entered.notified().await;
    state.shutdown.begin();
    let barrier = state.shutdown.drain_admissions();
    tokio::pin!(barrier);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(barrier.as_mut().poll(cx).is_pending()))
            .await
    );
    store.release.add_permits(1);
    let Message::Binary(bytes) = output.recv().await.expect("accepted join") else {
        panic!("binary");
    };
    assert!(matches!(
        decode_envelope(&bytes).expect("join").payload,
        Some(envelope::Payload::JoinAccepted(_))
    ));
    // The legacy snapshot follows JoinAccepted; no network service is used.
    assert!(output.recv().await.is_some());
    barrier.await;
    assert_eq!(state.registry.member_count(), 1);
    input
        .send(Message::Binary(
            Envelope {
                protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                sequence: 3,
                payload: Some(envelope::Payload::EntityCommand(
                    orbisync_protocol::v1::EntityCommand {
                        command_id: orbisync_domain::CommandId::generate().to_string(),
                        entity_id: EntityId::generate().to_string(),
                        operation: "spawn".into(),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .expect("command");
    loop {
        let Message::Binary(bytes) = output.recv().await.expect("refusal") else {
            continue;
        };
        match decode_envelope(&bytes).expect("envelope").payload {
            Some(envelope::Payload::Heartbeat(_)) => continue,
            Some(envelope::Payload::Error(error)) => {
                assert_eq!(error.code, "SERVER_SHUTTING_DOWN");
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(
        state
            .registry
            .read_snapshot(id)
            .await
            .expect("actor")
            .entities
            .is_empty()
    );
    drop(input);
    task.await.expect("socket task");
}

#[tokio::test]
async fn admission88_activation_refused_before_storage_without_notice() {
    let loads = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(RuntimeRegistry::new());
    let state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        registry.clone(),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_checkpoint_store(Arc::new(ActivationCheckpointStore {
        checkpoint: None,
        loads: loads.clone(),
    }))
    .build();
    state.shutdown.begin();
    assert!(
        state
            .ensure_instance_activated(InstanceId::generate())
            .await
            .is_err()
    );
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert!(registry.is_empty().expect("registry"));
}
