use super::*;
use orbisync_protocol::v1::{ActivityEntry, ActivitySnapshot, ActivityUpdate};

fn entry(index: usize, bytes: usize) -> ActivityEntry {
    ActivityEntry {
        presence_id: PresenceId::generate().to_string(),
        user_id: UserId::generate().to_string(),
        // allow-hardcoded-secret: public activity namespace fixture
        key: format!("example.key_{index}"),
        data: Some(prost_types::Struct {
            fields: [(
                "value".into(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("x".repeat(bytes))),
                },
            )]
            .into_iter()
            .collect(),
        }),
        expires_at_unix_ms: 5000,
    }
}

#[test]
fn activity_snapshot_chunks_preserve_request_fence_order_and_complete_data() {
    let instance = InstanceId::generate();
    let request = orbisync_domain::CommandId::generate().to_string();
    let entries: Vec<_> = (0..40).map(|index| entry(index, 100)).collect();
    let frames = activity_snapshot_frames(
        instance,
        ActivitySnapshot {
            activities: entries.clone(),
            version: 47,
            ..Default::default()
        },
        1024,
        request.clone(),
    )
    .unwrap();
    assert!(frames.len() > 1 && frames.len() <= ACTIVITY_SNAPSHOT_CHUNKS);
    let mut reconstructed = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        assert!(frame.len() <= 1024);
        let env = Envelope::decode(frame.as_slice()).unwrap();
        assert_eq!(env.instance_id, instance.to_string());
        let Some(envelope::Payload::ActivitySnapshot(chunk)) = env.payload else {
            panic!("activity snapshot");
        };
        assert_eq!(chunk.snapshot_id, request);
        assert_eq!(chunk.version, 47);
        assert_eq!(chunk.chunk_index as usize, index);
        assert_eq!(chunk.chunk_count as usize, frames.len());
        reconstructed.extend(chunk.activities);
    }
    assert_eq!(reconstructed, entries);
    assert!(frames.iter().map(Vec::len).sum::<usize>() <= ACTIVITY_SNAPSHOT_BYTES);
    let empty =
        activity_snapshot_frames(instance, ActivitySnapshot::default(), 1024, request.clone())
            .unwrap();
    assert_eq!(
        empty.len(),
        1,
        "an empty baseline must still acknowledge the request"
    );
    let Some(envelope::Payload::ActivitySnapshot(chunk)) =
        Envelope::decode(empty[0].as_slice()).unwrap().payload
    else {
        panic!("snapshot");
    };
    assert_eq!(
        (chunk.snapshot_id, chunk.chunk_index, chunk.chunk_count),
        (request, 0, 1)
    );
}

#[test]
fn activity_snapshot_rejects_unrepresentable_entry_chunk_count_and_total_bytes() {
    let instance = InstanceId::generate();
    let request = orbisync_domain::CommandId::generate().to_string();
    assert_eq!(
        activity_snapshot_frames(
            instance,
            ActivitySnapshot {
                activities: vec![entry(0, 1000)],
                ..Default::default()
            },
            512,
            request.clone()
        ),
        Err(ActivityError::SnapshotTooLarge)
    );
    assert_eq!(
        activity_snapshot_frames(
            instance,
            ActivitySnapshot {
                activities: (0..129).map(|index| entry(index, 200)).collect(),
                ..Default::default()
            },
            600,
            request.clone()
        ),
        Err(ActivityError::SnapshotTooLarge),
        "one entry per frame cannot exceed 128 chunks"
    );
    assert_eq!(
        activity_snapshot_frames(
            instance,
            ActivitySnapshot {
                activities: (0..150).map(|index| entry(index, 4000)).collect(),
                ..Default::default()
            },
            64 * 1024,
            request
        ),
        Err(ActivityError::SnapshotTooLarge),
        "full encoded envelopes count against the aggregate budget"
    );
}

struct Scope(AtomicBool);
#[async_trait::async_trait]
impl orbisync_application::EphemeralSubjectScope for Scope {
    async fn decide(
        &self,
        _: UserId,
        _: uuid::Uuid,
        _: orbisync_domain::Timestamp,
    ) -> Result<orbisync_application::ScopeDecision, orbisync_application::ApplicationError> {
        Ok(if self.0.load(Ordering::Acquire) {
            orbisync_application::ScopeDecision::Allowed
        } else {
            orbisync_application::ScopeDecision::Denied
        })
    }
}

struct Fixture {
    state: Arc<RealtimeState>,
    sender: ActivitySender,
    world: orbisync_domain::WorldId,
    clock: Arc<orbisync_testkit::FixedClock>,
    repository: Arc<orbisync_testkit::FakeIdentityRepository>,
    session: orbisync_domain::AuthSession,
    scope: Arc<Scope>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_config(orbisync_config::Config::default().realtime).await
    }
    async fn with_config(config: RealtimeConfig) -> Self {
        let sender = ActivitySender {
            instance_id: InstanceId::generate(),
            presence_id: PresenceId::generate(),
            user_id: UserId::generate(),
            epoch: 0,
        };
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: sender.instance_id,
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        assert!(matches!(
            actor.handle(InstanceCommand::Join {
                presence_id: sender.presence_id,
                user_id: sender.user_id,
                instance_id: sender.instance_id,
                capacity: 100,
            }),
            CommandOutcome::Applied { .. }
        ));
        let registry = Arc::new(RuntimeRegistry::new());
        registry.insert(sender.instance_id, actor).unwrap();
        let clock = Arc::new(orbisync_testkit::FixedClock::new(
            orbisync_domain::Timestamp::from_unix_millis(1000).unwrap(),
        ));
        let repository = Arc::new(orbisync_testkit::FakeIdentityRepository::new());
        let session = orbisync_domain::AuthSession::new(
            orbisync_domain::AuthSessionId::generate(),
            sender.user_id,
            clock.now(),
            orbisync_domain::Timestamp::from_unix_millis(60_000).unwrap(),
        )
        .unwrap();
        repository.save_session(&session).await.unwrap();
        let scope = Arc::new(Scope(AtomicBool::new(true)));
        let state = Arc::new(
            RealtimeState::builder(
                config,
                registry,
                Arc::new(DeliveryRegistry::new()),
                clock.clone(),
            )
            .with_identity_repository(repository.clone())
            .with_ephemeral_scope(scope.clone())
            .build(),
        );
        state.resume_store.issue(
            sender.user_id,
            sender.instance_id,
            sender.presence_id,
            0,
            orbisync_domain::Revision::INITIAL,
        );
        bind_activity_presence(&state, &sender).unwrap();
        Self {
            state,
            sender,
            world: orbisync_domain::WorldId::generate(),
            clock,
            repository,
            session,
            scope,
        }
    }
    fn sink(&self, enabled: bool) -> Arc<RealtimeDeliverySink> {
        let sink = Arc::new(RealtimeDeliverySink::new(
            self.state.clone(),
            self.sender.instance_id,
            Vec3::new(0.0, 0.0, 0.0).unwrap(),
            self.sender.user_id,
            None,
            Default::default(),
        ));
        sink.bind_presence(self.sender.presence_id, self.sender.epoch);
        sink.set_activities_enabled(enabled);
        sink
    }
    async fn publish(&self) -> Result<(), (&'static str, String)> {
        publish_client_activity(
            &self.state,
            self.sender,
            self.world,
            Some(self.session.id()),
            &self.sender.instance_id.to_string(),
            &ActivityUpdate {
                key: "example.typing".into(),
                ttl_ms: 5000,
                data: Some(prost_types::Struct::default()),
            },
        )
        .await
    }
    async fn snapshot(
        &self,
        sink: &Arc<RealtimeDeliverySink>,
        request: String,
    ) -> Result<(), (&'static str, String)> {
        send_activity_snapshot(
            &self.state,
            sink,
            self.sender,
            self.world,
            Some(self.session.id()),
            request,
        )
        .await
    }
}
fn take(sink: &RealtimeDeliverySink) -> Vec<Envelope> {
    let frames = sink.queue().lock().unwrap().drain_all();
    frames
        .iter()
        .filter_map(|bytes| sink.prepare_delivery(bytes))
        .collect()
}

#[tokio::test]
async fn activity_handoff_orders_snapshot_and_live_changes_without_actor_history() {
    let fixture = Fixture::new().await;
    let sink = fixture.sink(true);
    let unsupported = fixture.sink(false);
    let _registration = fixture
        .state
        .delivery
        .register_sink(fixture.sender.instance_id, sink.clone());
    let _unsupported = fixture
        .state
        .delivery
        .register_sink(fixture.sender.instance_id, unsupported.clone());
    let mut legacy = fixture.state.delivery.register(fixture.sender.instance_id);
    let before = fixture
        .state
        .registry
        .read_snapshot(fixture.sender.instance_id)
        .await
        .unwrap();
    fixture.publish().await.unwrap();
    let request = orbisync_domain::CommandId::generate().to_string();
    fixture.snapshot(&sink, request.clone()).await.unwrap();
    fixture.publish().await.unwrap();
    let frames = take(&sink);
    assert_eq!(frames.len(), 3);
    let Some(envelope::Payload::ActivityChanged(first)) = &frames[0].payload else {
        panic!("first change");
    };
    let Some(envelope::Payload::ActivitySnapshot(snapshot)) = &frames[1].payload else {
        panic!("snapshot");
    };
    let Some(envelope::Payload::ActivityChanged(last)) = &frames[2].payload else {
        panic!("last change");
    };
    assert_eq!(snapshot.snapshot_id, request);
    assert_eq!(snapshot.version, first.version);
    assert_eq!(snapshot.activities[0], *first.activity.as_ref().unwrap());
    assert!(last.version > snapshot.version);
    assert_eq!(
        snapshot.activities[0].presence_id,
        fixture.sender.presence_id.to_string()
    );
    assert_eq!(
        snapshot.activities[0].user_id,
        fixture.sender.user_id.to_string()
    );
    assert!(take(&unsupported).is_empty());
    assert!(legacy.try_recv().is_err());
    let after = fixture
        .state
        .registry
        .read_snapshot(fixture.sender.instance_id)
        .await
        .unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.reliable_events.len(), before.reliable_events.len());
    assert_eq!(
        fixture
            .snapshot(&sink, "invalid-request".into())
            .await
            .unwrap_err()
            .0,
        "INVALID_ARGUMENT"
    );
    assert!(
        take(&sink).is_empty(),
        "an invalid request cannot establish an uncorrelated baseline"
    );
}

#[tokio::test]
async fn activity_updates_and_snapshots_clear_on_session_scope_expiry_and_membership_loss() {
    for snapshot_request in [false, true] {
        for cause in ["revoke", "expire", "scope", "leave"] {
            let mut fixture = Fixture::new().await;
            let sink = fixture.sink(true);
            fixture.publish().await.unwrap();
            match cause {
                "revoke" => {
                    fixture.session.revoke(fixture.clock.now());
                    fixture
                        .repository
                        .save_session(&fixture.session)
                        .await
                        .unwrap();
                }
                "expire" => fixture.clock.advance_millis(59_000),
                "scope" => fixture.scope.0.store(false, Ordering::Release),
                _ => {
                    fixture
                        .state
                        .registry
                        .submit(
                            fixture.sender.instance_id,
                            InstanceCommand::Leave {
                                presence_id: fixture.sender.presence_id,
                            },
                        )
                        .await
                        .unwrap();
                }
            }
            let error = if snapshot_request {
                fixture
                    .snapshot(&sink, orbisync_domain::CommandId::generate().to_string())
                    .await
            } else {
                fixture.publish().await
            };
            assert_eq!(
                error.unwrap_err().0,
                "NOT_AUTHORIZED",
                "{cause}, snapshot={snapshot_request}"
            );
            assert_eq!(
                fixture.state.activities.retained(),
                (0, 0, 0),
                "lost authorization clears this connection's activity immediately"
            );
            assert!(
                take(&sink).is_empty(),
                "unauthorized snapshots must not be queued"
            );
        }
    }
}

#[tokio::test]
async fn activity_connection_guard_and_expiration_publish_removal_without_replay() {
    let fixture = Fixture::new().await;
    let sink = fixture.sink(true);
    let _registration = fixture
        .state
        .delivery
        .register_sink(fixture.sender.instance_id, sink.clone());
    fixture.publish().await.unwrap();
    fixture.clock.advance_millis(5000);
    fixture.state.sweep_activities();
    let frames = take(&sink);
    assert_eq!(frames.len(), 2);
    assert!(
        matches!(frames[1].payload, Some(envelope::Payload::ActivityChanged(ref change)) if change.removed)
    );
    fixture.publish().await.unwrap();
    drop(ActivityPresenceGuard {
        state: fixture.state.clone(),
        sender: fixture.sender,
    });
    let frames = take(&sink);
    assert_eq!(frames.len(), 2);
    assert!(
        matches!(frames[1].payload, Some(envelope::Payload::ActivityChanged(ref change)) if change.removed)
    );
    assert_eq!(fixture.state.activities.retained(), (0, 0, 0));
    assert!(
        fixture
            .state
            .registry
            .read_snapshot(fixture.sender.instance_id)
            .await
            .unwrap()
            .reliable_events
            .is_empty()
    );
}

#[tokio::test]
async fn activity_snapshot_queue_capacity_rejects_candidate_before_mutation_or_partial_baseline() {
    let mut config = orbisync_config::Config::default().realtime;
    config.max_normal_message_bytes = 512;
    config.outbound_queue_capacity = 1;
    let fixture = Fixture::with_config(config).await;
    let request = ActivityUpdate {
        key: "example.first".into(),
        ttl_ms: 5000,
        data: entry(0, 180).data,
    };
    let fixture_ref = &fixture;
    let publish = move |request: ActivityUpdate| async move {
        publish_client_activity(
            &fixture_ref.state,
            fixture_ref.sender,
            fixture_ref.world,
            Some(fixture_ref.session.id()),
            &fixture_ref.sender.instance_id.to_string(),
            &request,
        )
        .await
    };
    publish(request.clone()).await.unwrap();
    let sink = fixture.sink(true);
    let before = fixture.state.activities.retained();
    let second = ActivityUpdate {
        key: "example.second".into(),
        ..request
    };
    assert_eq!(
        publish(second.clone()).await.unwrap_err().0,
        "MESSAGE_TOO_LARGE"
    );
    assert_eq!(fixture.state.activities.retained(), before);
    fixture
        .snapshot(&sink, orbisync_domain::CommandId::generate().to_string())
        .await
        .unwrap();
    let frames = take(&sink);
    assert_eq!(frames.len(), 1);
    let Some(envelope::Payload::ActivitySnapshot(snapshot)) = &frames[0].payload else {
        panic!("snapshot");
    };
    assert_eq!(snapshot.activities.len(), 1);
    assert_eq!(snapshot.activities[0].key, "example.first");
    // Also cover a recovered/pre-existing state larger than a reduced
    // queue configuration: reject before the first frame is queued.
    fixture
        .state
        .activities
        .update(fixture.sender, second, 1000, |_, _| {})
        .unwrap();
    assert_eq!(
        fixture
            .snapshot(&sink, orbisync_domain::CommandId::generate().to_string())
            .await
            .unwrap_err()
            .0,
        "MESSAGE_TOO_LARGE"
    );
    assert!(take(&sink).is_empty());
    assert!(!sink.is_closed());
}

#[tokio::test]
async fn activity_websocket_dispatch_negotiates_feature_and_clears_on_disconnect() {
    for enabled in [true, false] {
        let fixture = Fixture::new().await;
        if !enabled {
            clear_activity_presence(&fixture.state, fixture.sender);
        }
        let sink = fixture.sink(enabled);
        let registration = fixture
            .state
            .delivery
            .register_sink(fixture.sender.instance_id, sink.clone());
        let (socket, input, mut output) = RealtimeSocket::controlled();
        let hello = Envelope {
            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
            payload: Some(envelope::Payload::ServerHello(
                orbisync_protocol::v1::ServerHello {
                    enabled_features: if enabled {
                        vec![ACTIVITY_FEATURE.into()]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
        .encode_to_vec();
        let presence_guard = PresenceGuard {
            delivery: fixture.state.delivery.clone(),
            instance_id: fixture.sender.instance_id,
            presence: fixture.sender.presence_id,
            epoch: fixture.sender.epoch,
            resume_store: fixture.state.resume_store.clone(),
        };
        let task = tokio::spawn(run_connected_socket(
            socket,
            fixture.state.clone(),
            fixture.state.shutdown.subscribe(),
            ConnectionState::Active,
            orbisync_realtime::gateway::HandshakeSuccess {
                next_state: ConnectionState::Ready,
                server_hello_bytes: hello,
                negotiated_minor: 0,
                connection_id: RealtimeConnectionId::generate(),
                realtime_ticket: String::new(),
            },
            HeartbeatPolicy::from_config(&fixture.state.config),
            fixture.sender.user_id,
            Some(fixture.session.id()),
            WorldPermissions::all(),
            None,
            sink,
            registration,
            fixture.sender.instance_id,
            fixture.world,
            fixture.sender.presence_id,
            3,
            3,
            false,
            Some(presence_guard),
        ));
        for sequence in 3..=5 {
            let request_id = orbisync_domain::CommandId::generate().to_string();
            let payload = if sequence == 4 {
                envelope::Payload::ActivityUpdate(ActivityUpdate {
                    key: "example.typing".into(),
                    ttl_ms: 5000,
                    data: Some(prost_types::Struct::default()),
                })
            } else {
                envelope::Payload::ActivitySnapshotRequest(
                    orbisync_protocol::v1::ActivitySnapshotRequest {},
                )
            };
            input
                .send(Message::Binary(
                    Envelope {
                        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                        sequence,
                        message_id: request_id.clone(),
                        instance_id: fixture.sender.instance_id.to_string(),
                        payload: Some(payload),
                        ..Default::default()
                    }
                    .encode_to_vec()
                    .into(),
                ))
                .await
                .unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(2), output.recv())
                .await
                .unwrap()
                .unwrap();
            let Message::Binary(bytes) = response else {
                panic!("binary response");
            };
            let env = decode_envelope(&bytes).unwrap();
            if !enabled {
                assert!(
                    matches!(env.payload, Some(envelope::Payload::Error(ref error)) if error.code == "FEATURE_NOT_NEGOTIATED")
                );
            } else if sequence == 4 {
                assert!(
                    matches!(env.payload, Some(envelope::Payload::ActivityChanged(ref change)) if !change.removed)
                );
            } else {
                let Some(envelope::Payload::ActivitySnapshot(snapshot)) = env.payload else {
                    panic!("snapshot response");
                };
                assert_eq!(snapshot.snapshot_id, request_id);
                assert_eq!(snapshot.activities.len(), usize::from(sequence == 5));
            }
        }
        assert_eq!(fixture.state.activities.retained().0, usize::from(enabled));
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture.state.activities.retained(),
            (0, 0, 0),
            "runtime guard clears activity and bindings on actual transport closure"
        );
    }
}
