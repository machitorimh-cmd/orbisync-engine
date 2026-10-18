#[test]
fn review_active_token_prune_then_disconnect_reclaims_actual_membership() {
    let (actor, instance, _, user, presence) = snapshot_fixture();
    let clock = Arc::new(orbisync_testkit::FixedClock::new(
        orbisync_domain::Timestamp::from_unix_millis(1_000).unwrap(),
    ));
    let registry = Arc::new(RuntimeRegistry::new());
    registry.insert(instance.id(), actor).unwrap();
    let state = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        registry.clone(),
        Arc::new(DeliveryRegistry::new()),
        clock.clone(),
    )
    .build();
    let token = state.resume_store.issue(
        user,
        instance.id(),
        presence,
        0,
        orbisync_domain::Revision::INITIAL,
    );
    let guard = PresenceGuard {
        delivery: state.delivery.clone(),
        instance_id: instance.id(),
        presence,
        epoch: 0,
        resume_store: state.resume_store.clone(),
    };
    clock.advance_millis(61_000);
    assert_eq!(state.prune_expired_resume_sessions(), 0);
    assert!(state.resume_store.consume_for_user(&token, user).is_none());
    assert_eq!(
        registry.member_count(),
        1,
        "token TTL must not evict a live socket"
    );
    drop(guard);
    clock.advance_millis(60_001);
    assert_eq!(state.prune_expired_resume_sessions(), 1);
    assert_eq!(
        registry.member_count(),
        0,
        "disconnect cleanup survives token expiry"
    );
}

#[tokio::test]
async fn review_membership_cleanup_retries_full_mailbox_for_expiry_and_eviction() {
    for evict in [false, true] {
        let (actor, instance, _, user, presence) = snapshot_fixture();
        let clock = Arc::new(orbisync_testkit::FixedClock::new(
            orbisync_domain::Timestamp::from_unix_millis(1_000).unwrap(),
        ));
        let registry = Arc::new(RuntimeRegistry::new());
        let handle = registry.ensure_instance(actor);
        let state = RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            registry.clone(),
            Arc::new(DeliveryRegistry::new()),
            clock.clone(),
        )
        .build();
        state.resume_store.issue(
            user,
            instance.id(),
            presence,
            0,
            orbisync_domain::Revision::INITIAL,
        );
        state.resume_store.mark_disconnected_epoch(presence, 0);
        if !evict {
            clock.advance_millis(60_001);
        }
        // No await: on this current-thread runtime the owner task cannot drain
        // its 256-slot outer mailbox before the cleanup attempt below.
        for _ in 0..256 {
            handle.try_submit(InstanceCommand::Tick).unwrap();
        }
        assert!(matches!(
            handle.try_submit(InstanceCommand::Tick),
            Err(MailboxSendError::Full { .. })
        ));
        if evict {
            let binding = state
                .resume_store
                .evict_oldest_for_instance(instance.id())
                .unwrap();
            assert!(!state.release_resume_membership(&binding).await);
        } else {
            assert_eq!(state.prune_expired_resume_sessions_async().await, 0);
        }
        let pending = state.resume_store.prune_expired();
        assert_eq!(
            pending.len(),
            1,
            "Full must retain cleanup ownership (evict={evict})"
        );
        assert!(state.resume_store.rebind(&pending[0]).is_none());
        assert_eq!(registry.is_present(instance.id(), presence), Some(true));
        // A queued read waits for the backlog to drain, without timing sleeps.
        handle.read_snapshot().await.unwrap();
        assert_eq!(state.prune_expired_resume_sessions_async().await, 1);
        assert_eq!(registry.is_present(instance.id(), presence), Some(false));
        assert!(
            state.resume_store.prune_expired().is_empty(),
            "only successful Leave releases the claim"
        );
    }
}

#[test]
fn review_partial_commit_views_preserve_other_entities_hysteresis() {
    use crate::delivery::DeliverySink;
    let (mut actor, instance, _, user, _) = snapshot_fixture();
    let a = EntityId::generate();
    let b = EntityId::generate();
    for (id, x) in [(a, 31.0), (b, 1.0)] {
        assert!(matches!(
            actor.handle(InstanceCommand::SpawnEntity {
                command_id: None,
                entity_id: id,
                kind: EntityKind::Object,
                owner: Some(user),
                transform: n3_make_entity(id, instance.id(), x).transform(),
                visibility: VisibilityPolicy::spatial(100.0).unwrap(),
                requester: user,
                permissions: WorldPermissions::all(),
            }),
            CommandOutcome::Applied { .. }
        ));
    }
    let registry = Arc::new(RuntimeRegistry::new());
    registry.insert(instance.id(), actor).unwrap();
    let views = registry.interest_views(instance.id()).unwrap();
    let state = Arc::new(RealtimeState::new(
        orbisync_config::Config::default().realtime,
        registry,
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    ));
    // A was already subscribed at 29m before the current 31m position.
    let sink = RealtimeDeliverySink::new(
        state,
        instance.id(),
        Vec3::new(0.0, 0.0, 0.0).unwrap(),
        user,
        None,
        std::collections::HashMap::from([(a, true), (b, true)]),
    );
    let only_b = std::iter::once(views.get(b).unwrap().clone()).collect();
    assert_eq!(
        sink.enqueue(
            &n3_make_payload(b, 1, 1.0),
            Reliability::LatestWins,
            Some(&only_b)
        )
        .delivered,
        1
    );
    assert_eq!(sink.filter.lock().unwrap().subscribed.get(&a), Some(&true));
    let only_a = std::iter::once(views.get(a).unwrap().clone()).collect();
    assert_eq!(
        sink.enqueue(
            &n3_make_payload(a, 2, 31.0),
            Reliability::LatestWins,
            Some(&only_a)
        )
        .delivered,
        1,
        "an unrelated partial commit must not reset the 35m retention threshold"
    );
}

struct ReviewParticipationScope {
    allowed: AtomicBool,
    deadline: orbisync_domain::Timestamp,
}

#[async_trait::async_trait]
impl orbisync_application::EphemeralSubjectScope for ReviewParticipationScope {
    async fn decide(
        &self,
        _: UserId,
        _: uuid::Uuid,
        now: orbisync_domain::Timestamp,
    ) -> Result<orbisync_application::ScopeDecision, orbisync_application::ApplicationError> {
        Ok(
            if self.allowed.load(Ordering::Acquire) && now < self.deadline {
                orbisync_application::ScopeDecision::Allowed
            } else {
                orbisync_application::ScopeDecision::Denied
            },
        )
    }
}

fn review_event(instance: InstanceId, sequence: u64) -> Message {
    Message::Binary(
        Envelope {
            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
            sequence,
            instance_id: instance.to_string(),
            message_id: uuid::Uuid::now_v7().to_string(),
            payload: Some(envelope::Payload::DomainEvent(
                orbisync_protocol::v1::DomainEvent {
                    event_id: orbisync_domain::CommandId::generate().to_string(),
                    event_type: "custom.chat.message".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
        .encode_to_vec()
        .into(),
    )
}

fn review_runtime_connection(
    state: Arc<RealtimeState>,
    world: orbisync_domain::WorldId,
    user: UserId,
    instance: InstanceId,
    presence: PresenceId,
    session: Option<orbisync_domain::AuthSessionId>,
) -> (
    tokio::sync::mpsc::Sender<Message>,
    tokio::sync::mpsc::Receiver<Message>,
    tokio::task::JoinHandle<()>,
) {
    let (socket, input, output) = RealtimeSocket::controlled();
    let sink = Arc::new(RealtimeDeliverySink::new(
        state.clone(),
        instance,
        Vec3::new(0.0, 0.0, 0.0).unwrap(),
        user,
        None,
        Default::default(),
    ));
    sink.bind_presence(presence, 0);
    let registration = state.delivery.register_sink(instance, sink.clone());
    let guard = PresenceGuard {
        delivery: state.delivery.clone(),
        instance_id: instance,
        presence,
        epoch: 0,
        resume_store: state.resume_store.clone(),
    };
    let task = tokio::spawn(run_connected_socket(
        socket,
        state.clone(),
        state.shutdown.subscribe(),
        ConnectionState::Active,
        orbisync_realtime::gateway::HandshakeSuccess {
            next_state: ConnectionState::Ready,
            server_hello_bytes: Vec::new(),
            negotiated_minor: 0,
            connection_id: RealtimeConnectionId::generate(),
            realtime_ticket: String::new(),
        },
        HeartbeatPolicy::from_config(&state.config),
        user,
        session,
        WorldPermissions::all(),
        None,
        sink,
        registration,
        instance,
        world,
        presence,
        3,
        3,
        false,
        Some(guard),
    ));
    (input, output, task)
}

async fn review_receive(output: &mut tokio::sync::mpsc::Receiver<Message>) -> Envelope {
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), output.recv())
        .await
        .unwrap()
        .unwrap();
    let Message::Binary(bytes) = message else {
        panic!("expected binary response");
    };
    decode_envelope(&bytes).unwrap()
}

#[tokio::test]
async fn review_custom_events_recheck_deadline_world_scope_and_session_on_existing_socket() {
    for cause in [
        "guest_expiry",
        "world_scope",
        "session_revocation",
        "session_expiry",
    ] {
        let (actor, instance, world, user, presence) = snapshot_fixture();
        let clock = Arc::new(orbisync_testkit::FixedClock::new(
            orbisync_domain::Timestamp::from_unix_millis(1_000).unwrap(),
        ));
        let scope = Arc::new(ReviewParticipationScope {
            allowed: AtomicBool::new(true),
            deadline: orbisync_domain::Timestamp::from_unix_millis(if cause == "guest_expiry" {
                2_000
            } else {
                100_000
            })
            .unwrap(),
        });
        let repository = Arc::new(orbisync_testkit::FakeIdentityRepository::new());
        let mut session = orbisync_domain::AuthSession::new(
            orbisync_domain::AuthSessionId::generate(),
            user,
            clock.now(),
            orbisync_domain::Timestamp::from_unix_millis(10_000).unwrap(),
        )
        .unwrap();
        repository.save_session(&session).await.unwrap();
        let registry = Arc::new(RuntimeRegistry::new());
        registry.insert(instance.id(), actor).unwrap();
        let state = Arc::new(
            RealtimeState::builder(
                orbisync_config::Config::default().realtime,
                registry.clone(),
                Arc::new(DeliveryRegistry::new()),
                clock.clone(),
            )
            .with_identity_repository(repository.clone())
            .with_ephemeral_scope(scope.clone())
            .build(),
        );
        state.resume_store.issue(
            user,
            instance.id(),
            presence,
            0,
            orbisync_domain::Revision::INITIAL,
        );
        let (input, mut output, task) = review_runtime_connection(
            state,
            world.id(),
            user,
            instance.id(),
            presence,
            Some(session.id()),
        );
        input.send(review_event(instance.id(), 3)).await.unwrap();
        assert!(matches!(
            review_receive(&mut output).await.payload,
            Some(envelope::Payload::DomainEvent(_))
        ));
        let before = registry.read_snapshot(instance.id()).await.unwrap();
        match cause {
            "guest_expiry" => clock.advance_millis(1_000),
            "world_scope" => scope.allowed.store(false, Ordering::Release),
            "session_revocation" => {
                session.revoke(clock.now());
                repository.save_session(&session).await.unwrap();
            }
            _ => clock.advance_millis(9_000),
        }
        input.send(review_event(instance.id(), 4)).await.unwrap();
        assert!(
            matches!(review_receive(&mut output).await.payload,
            Some(envelope::Payload::Error(ref error)) if error.code == "NOT_AUTHORIZED"),
            "{cause}"
        );
        let after = registry.read_snapshot(instance.id()).await.unwrap();
        assert_eq!(
            after.revision, before.revision,
            "{cause} must not advance the actor"
        );
        assert_eq!(
            after.reliable_events.len(),
            before.reliable_events.len(),
            "{cause} must not enter replay history"
        );
        drop(input);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn review_superseded_socket_cannot_publish_or_disconnect_new_owner() {
    let (actor, instance, world, user, presence) = snapshot_fixture();
    let registry = Arc::new(RuntimeRegistry::new());
    registry.insert(instance.id(), actor).unwrap();
    let state = Arc::new(RealtimeState::new(
        orbisync_config::Config::default().realtime,
        registry.clone(),
        Arc::new(DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    ));
    let token = state.resume_store.issue(
        user,
        instance.id(),
        presence,
        0,
        orbisync_domain::Revision::INITIAL,
    );
    let (input, mut output, task) = review_runtime_connection(
        state.clone(),
        world.id(),
        user,
        instance.id(),
        presence,
        None,
    );
    input.send(review_event(instance.id(), 3)).await.unwrap();
    assert!(matches!(
        review_receive(&mut output).await.payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    let previous = state.resume_store.consume_for_user(&token, user).unwrap();
    let (_, epoch) = state.resume_store.rebind(&previous).unwrap();
    let before = registry
        .read_snapshot(instance.id())
        .await
        .unwrap()
        .revision;
    input.send(review_event(instance.id(), 4)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(output.recv().await, Some(Message::Close(_))));
    assert_eq!(
        registry
            .read_snapshot(instance.id())
            .await
            .unwrap()
            .revision,
        before
    );
    assert!(
        state.resume_store.is_current(presence, epoch),
        "old guard cleanup cannot disconnect the new epoch"
    );
    assert!(
        state
            .resume_store
            .evict_oldest_for_instance(instance.id())
            .is_none()
    );
}

struct ReviewUserVerifier(UserId);

#[async_trait::async_trait]
impl RealtimeTicketVerifier for ReviewUserVerifier {
    async fn verify(&self, _: &str) -> Result<UserId, orbisync_realtime::gateway::GatewayError> {
        Ok(self.0)
    }
}

async fn review_receive_snapshot(output: &mut tokio::sync::mpsc::Receiver<Message>) {
    loop {
        let envelope::Payload::Snapshot(snapshot) = review_receive(output).await.payload.unwrap()
        else {
            panic!("expected snapshot after admission");
        };
        if snapshot.chunk_index + 1 == snapshot.chunk_count {
            break;
        }
    }
}

#[tokio::test]
async fn review_resume_overlap_keeps_new_socket_active_after_old_transport_closes() {
    let (actor, instance, world, user, _) = snapshot_fixture();
    let id = instance.id();
    let directory = Arc::new(orbisync_testkit::FakeWorldDirectoryStore::new());
    directory.insert_world(world);
    directory.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    registry.insert(id, actor).unwrap();
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            registry,
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_world_store(directory)
        .with_tickets(Arc::new(ReviewUserVerifier(user)))
        .build(),
    );
    let (old_socket, old_input, mut old_output) = RealtimeSocket::controlled();
    let old_task = tokio::spawn(handle_socket(
        old_socket,
        state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    ));
    old_input.send(admission88_hello()).await.unwrap();
    assert!(matches!(
        review_receive(&mut old_output).await.payload,
        Some(envelope::Payload::ServerHello(_))
    ));
    old_input
        .send(Message::Binary(
            Envelope {
                protocol_major: 1,
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
        .unwrap();
    let envelope::Payload::JoinAccepted(join) =
        review_receive(&mut old_output).await.payload.unwrap()
    else {
        panic!("join accepted");
    };
    review_receive_snapshot(&mut old_output).await;
    let presence: PresenceId = join.presence_id.parse().unwrap();

    let (new_socket, new_input, mut new_output) = RealtimeSocket::controlled();
    let new_task = tokio::spawn(handle_socket(
        new_socket,
        state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    ));
    new_input.send(admission88_hello()).await.unwrap();
    assert!(matches!(
        review_receive(&mut new_output).await.payload,
        Some(envelope::Payload::ServerHello(_))
    ));
    new_input
        .send(Message::Binary(
            Envelope {
                protocol_major: 1,
                sequence: 2,
                payload: Some(envelope::Payload::ResumeSession(
                    orbisync_protocol::v1::ResumeSession {
                        resume_token: join.resume_token,
                        last_applied_revision: join.instance_revision,
                        received_message_ids: Vec::new(),
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(review_receive(&mut new_output).await.payload,
        Some(envelope::Payload::ResumeAccepted(ref accepted)) if accepted.presence_id == presence.to_string()));
    review_receive_snapshot(&mut new_output).await;
    old_input.send(Message::Close(None)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), old_task)
        .await
        .unwrap()
        .unwrap();
    assert!(state.resume_store.is_current(presence, 1));
    assert!(state.resume_store.evict_oldest_for_instance(id).is_none());
    new_input.send(review_event(id, 3)).await.unwrap();
    assert!(matches!(
        review_receive(&mut new_output).await.payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    drop(new_input);
    tokio::time::timeout(std::time::Duration::from_secs(2), new_task)
        .await
        .unwrap()
        .unwrap();
}
