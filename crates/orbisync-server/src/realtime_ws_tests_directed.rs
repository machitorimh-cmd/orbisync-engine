#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use orbisync_protocol::v1::{DirectedEvent, DomainEvent};

const FEATURE: &str = "orbisync.directed-events.v1";

struct TicketUser;

#[async_trait::async_trait]
impl RealtimeTicketVerifier for TicketUser {
    async fn verify(
        &self,
        ticket: &str,
    ) -> Result<UserId, orbisync_realtime::gateway::GatewayError> {
        UserId::parse(ticket).map_err(|_| orbisync_realtime::gateway::GatewayError::InvalidTicket)
    }
}

struct Fixture {
    state: Arc<RealtimeState>,
    instance: InstanceId,
    sender: (PresenceId, UserId),
    recipient: (PresenceId, UserId),
    second_device: PresenceId,
    bystander: (PresenceId, UserId),
}

impl Fixture {
    fn new() -> Self {
        Self::with_config(orbisync_config::Config::default().realtime)
    }

    fn with_config(config: RealtimeConfig) -> Self {
        let instance = InstanceId::generate();
        let world_id = orbisync_domain::WorldId::generate();
        let now = orbisync_domain::Timestamp::from_unix_millis(1_000).unwrap();
        let world = orbisync_domain::World::new(
            world_id,
            "directed",
            None,
            Transform::identity(),
            100,
            now,
        )
        .unwrap();
        let descriptor = orbisync_domain::WorldInstance::new(instance, world_id, 100, now).unwrap();
        let directory = Arc::new(orbisync_testkit::FakeWorldDirectoryStore::new());
        directory.insert_world(world);
        directory.insert_instance(descriptor);
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        let sender = (PresenceId::generate(), UserId::generate());
        let recipient = (PresenceId::generate(), UserId::generate());
        let second_device = PresenceId::generate();
        let bystander = (PresenceId::generate(), UserId::generate());
        for (presence_id, user_id) in [sender, recipient, (second_device, recipient.1), bystander] {
            assert!(matches!(
                actor.handle(InstanceCommand::Join {
                    presence_id,
                    user_id,
                    instance_id: instance,
                    capacity: 100,
                }),
                CommandOutcome::Applied { .. }
            ));
        }
        let registry = Arc::new(RuntimeRegistry::new());
        registry.insert(instance, actor).unwrap();
        let state = Arc::new(
            RealtimeState::builder(
                config,
                registry,
                Arc::new(DeliveryRegistry::new()),
                Arc::new(orbisync_domain::SystemClock::new()),
            )
            .with_world_store(directory)
            .with_tickets(Arc::new(TicketUser))
            .build(),
        );
        for (presence, user) in [sender, recipient, (second_device, recipient.1), bystander] {
            state.resume_store.issue(
                user,
                instance,
                presence,
                0,
                orbisync_domain::Revision::INITIAL,
            );
        }
        Self {
            state,
            instance,
            sender,
            recipient,
            second_device,
            bystander,
        }
    }

    fn sink(
        &self,
        member: (PresenceId, UserId),
        enabled: bool,
        pending: bool,
    ) -> Arc<RealtimeDeliverySink> {
        let sink = Arc::new(RealtimeDeliverySink::new(
            self.state.clone(),
            self.instance,
            Vec3::new(0.0, 0.0, 0.0).unwrap(),
            member.1,
            None,
            Default::default(),
        ));
        sink.bind_presence(member.0, 0);
        sink.set_directed_events_enabled(enabled);
        if pending {
            sink.begin_snapshot();
        }
        sink
    }

    async fn publish(
        &self,
        event: DirectedEvent,
    ) -> Result<Option<Vec<u8>>, (&'static str, String)> {
        let _guard = self.state.registry.lifecycle_guard(self.instance).await;
        publish_directed_event(
            &self.state,
            self.instance,
            &self.instance.to_string(),
            self.sender.1,
            self.sender.0,
            event,
            0,
        )
        .await
    }
}

fn request() -> DirectedEvent {
    DirectedEvent {
        event: Some(DomainEvent {
            event_id: orbisync_domain::CommandId::generate().to_string(),
            event_type: "custom.message".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn take(sink: &RealtimeDeliverySink) -> Vec<Envelope> {
    let queued = sink.queue().lock().unwrap().drain_all();
    queued
        .iter()
        .filter_map(|bytes| sink.prepare_delivery(bytes))
        .collect()
}

fn directed(env: &Envelope) -> &DirectedEvent {
    let Some(envelope::Payload::DirectedEvent(event)) = env.payload.as_ref() else {
        panic!("directed event expected");
    };
    event
}

fn nested_data_value(depth: usize) -> prost_types::Value {
    let mut value = prost_types::Value {
        kind: Some(prost_types::value::Kind::BoolValue(true)),
    };
    for _ in 1..depth {
        value = prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
                fields: [("child".to_owned(), value)].into_iter().collect(),
            })),
        };
    }
    value
}

#[test]
fn directed_json_data_matches_sdk_depth_numbers_and_server_stamped_byte_budget() {
    use prost_types::value::Kind;
    let user = UserId::generate();
    let presence = PresenceId::generate();
    let mut valid = request();
    valid.recipient_user_ids = vec![user.to_string()];
    valid.event.as_mut().unwrap().data = Some(prost_types::Struct {
        fields: [
            ("depth64".to_owned(), nested_data_value(64)),
            (
                "positive".to_owned(),
                prost_types::Value {
                    kind: Some(Kind::NumberValue(9_007_199_254_740_991.0)),
                },
            ),
            (
                "negative".to_owned(),
                prost_types::Value {
                    kind: Some(Kind::NumberValue(-9_007_199_254_740_991.0)),
                },
            ),
            (
                "decimal".to_owned(),
                prost_types::Value {
                    kind: Some(Kind::NumberValue(0.5)),
                },
            ),
            (
                "list".to_owned(),
                prost_types::Value {
                    kind: Some(Kind::ListValue(prost_types::ListValue {
                        values: vec![prost_types::Value {
                            kind: Some(Kind::NullValue(0)),
                        }],
                    })),
                },
            ),
            // Authoritative sender stamping precedes validation, so a client
            // cannot choose these identities or poison them with an empty Value.
            (
                "sender_user_id".to_owned(),
                prost_types::Value { kind: None },
            ),
        ]
        .into_iter()
        .collect(),
    });
    prepare_directed_event(&mut valid, user, presence).unwrap();
    assert_eq!(
        valid.event.as_ref().unwrap().data.as_ref().unwrap().fields["sender_user_id"].kind,
        Some(Kind::StringValue(user.to_string()))
    );
    let mut too_large = request();
    too_large.recipient_user_ids = vec![user.to_string()];
    let data = prost_types::Struct {
        fields: [(
            "payload".to_owned(),
            prost_types::Value {
                kind: Some(Kind::StringValue("x".repeat(65_450))),
            },
        )]
        .into_iter()
        .collect(),
    };
    assert!(data.encoded_len() <= 65_536);
    too_large.event.as_mut().unwrap().data = Some(data);
    assert!(
        prepare_directed_event(&mut too_large, user, presence).is_err(),
        "the byte limit includes the server-stamped sender fields"
    );
}

#[tokio::test]
async fn directed_malformed_json_is_rejected_before_revision_history_or_delivery() {
    use prost_types::value::Kind;
    let fixture = Fixture::new();
    let recipient = fixture.sink(fixture.recipient, true, false);
    let sender = fixture.sink(fixture.sender, true, false);
    let _registrations: Vec<_> = [&sender, &recipient]
        .into_iter()
        .map(|sink| {
            fixture
                .state
                .delivery
                .register_sink(fixture.instance, sink.clone())
        })
        .collect();
    let before = fixture
        .state
        .registry
        .read_snapshot(fixture.instance)
        .await
        .unwrap()
        .revision;
    let mut invalid: Vec<_> = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        9_007_199_254_740_992.0,
        -9_007_199_254_740_992.0,
    ]
    .into_iter()
    .map(|value| prost_types::Value {
        kind: Some(Kind::NumberValue(value)),
    })
    .collect();
    invalid.push(prost_types::Value { kind: None });
    invalid.push(prost_types::Value {
        kind: Some(Kind::NullValue(42)),
    });
    invalid.push(nested_data_value(65));
    for value in invalid {
        let mut event = request();
        event.recipient_presence_ids = vec![fixture.recipient.0.to_string()];
        event.event.as_mut().unwrap().data = Some(prost_types::Struct {
            fields: [("payload".to_owned(), value)].into_iter().collect(),
        });
        assert_eq!(
            fixture.publish(event).await.unwrap_err().0,
            "INVALID_ARGUMENT"
        );
        let read = fixture
            .state
            .registry
            .read_snapshot(fixture.instance)
            .await
            .unwrap();
        assert_eq!(read.revision, before);
        assert!(read.reliable_events.is_empty());
        assert!(take(&sender).is_empty() && take(&recipient).is_empty());
    }
}

#[test]
fn directed_validation_rejects_ambiguous_invalid_and_server_owned_recipients() {
    let user = UserId::generate();
    let presence = PresenceId::generate();
    let mut valid = request();
    valid.recipient_user_ids = vec![user.to_string()];
    assert!(prepare_directed_event(&mut valid.clone(), user, presence).is_ok());
    let mut cases = vec![request()];
    let mut mixed = valid.clone();
    mixed.recipient_presence_ids.push(presence.to_string());
    cases.push(mixed);
    let mut forged = valid.clone();
    forged.resolved_presence_ids.push(presence.to_string());
    cases.push(forged);
    let mut duplicate = valid.clone();
    duplicate.recipient_user_ids.push(user.to_string());
    cases.push(duplicate);
    let mut invalid = valid.clone();
    invalid.recipient_user_ids = vec!["not-a-uuid".into()];
    cases.push(invalid);
    let mut legacy_uuid = valid.clone();
    legacy_uuid.recipient_user_ids = vec!["9b9d31fe-ef57-4cb6-a480-cfbc5fbd904e".into()];
    cases.push(legacy_uuid);
    let mut too_many = valid.clone();
    too_many.recipient_user_ids = (0..65).map(|_| UserId::generate().to_string()).collect();
    cases.push(too_many);
    let mut missing = valid.clone();
    missing.event = None;
    cases.push(missing);
    let mut core = valid.clone();
    core.event.as_mut().unwrap().event_type = "member.left".into();
    cases.push(core);
    for mut candidate in cases {
        assert!(prepare_directed_event(&mut candidate, user, presence).is_err());
    }

    let mut reverse = request();
    reverse.recipient_user_ids = (0..64).map(|_| UserId::generate().to_string()).collect();
    let mut ordered = reverse.clone();
    reverse.recipient_user_ids.reverse();
    prepare_directed_event(&mut reverse, user, presence).unwrap();
    prepare_directed_event(&mut ordered, user, presence).unwrap();
    assert_eq!(reverse, ordered);
}

#[tokio::test]
async fn directed_users_resolve_all_current_devices_and_echo_without_broadcasting_to_others() {
    let fixture = Fixture::new();
    let sender = fixture.sink(fixture.sender, true, false);
    let recipient = fixture.sink(fixture.recipient, true, false);
    let second = fixture.sink((fixture.second_device, fixture.recipient.1), true, false);
    let bystander = fixture.sink(fixture.bystander, true, false);
    let _registrations: Vec<_> = [&sender, &recipient, &second, &bystander]
        .into_iter()
        .map(|sink| {
            fixture
                .state
                .delivery
                .register_sink(fixture.instance, sink.clone())
        })
        .collect();
    let mut legacy = fixture.state.delivery.register(fixture.instance);
    let mut event = request();
    event.recipient_user_ids = vec![fixture.recipient.1.to_string()];
    event.event.as_mut().unwrap().instance_revision = u64::MAX;
    assert!(fixture.publish(event.clone()).await.unwrap().is_none());
    let echoes = take(&sender);
    assert_eq!(echoes.len(), 1);
    let original = echoes[0].encode_to_vec();
    let payload = directed(&echoes[0]);
    let expected: std::collections::BTreeSet<_> =
        [fixture.sender.0, fixture.recipient.0, fixture.second_device]
            .map(|id| id.to_string())
            .into_iter()
            .collect();
    assert_eq!(
        payload
            .resolved_presence_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    let event_data = payload.event.as_ref().unwrap().data.as_ref().unwrap();
    assert_eq!(
        event_data.fields["sender_user_id"].kind,
        Some(prost_types::value::Kind::StringValue(
            fixture.sender.1.to_string()
        ))
    );
    assert_ne!(payload.event.as_ref().unwrap().instance_revision, u64::MAX);
    assert_eq!(take(&recipient).len(), 1);
    assert_eq!(take(&second).len(), 1);
    assert!(take(&bystander).is_empty());
    assert!(
        legacy.try_recv().is_err(),
        "unbound compatibility channels must fail closed"
    );

    let late = PresenceId::generate();
    fixture
        .state
        .registry
        .submit(
            fixture.instance,
            InstanceCommand::Join {
                presence_id: late,
                user_id: fixture.recipient.1,
                instance_id: fixture.instance,
                capacity: 100,
            },
        )
        .await
        .unwrap();
    fixture.state.resume_store.issue(
        fixture.recipient.1,
        fixture.instance,
        late,
        0,
        orbisync_domain::Revision::INITIAL,
    );
    let late_sink = fixture.sink((late, fixture.recipient.1), true, false);
    let _late_registration = fixture
        .state
        .delivery
        .register_sink(fixture.instance, late_sink.clone());
    assert_eq!(
        fixture.publish(event).await.unwrap(),
        Some(original.clone())
    );
    assert!(
        take(&recipient).is_empty(),
        "duplicate acknowledgement must not rebroadcast"
    );
    assert!(take(&late_sink).is_empty());
    assert!(
        late_sink.prepare_delivery(&original).is_none(),
        "retained audience cannot expand on retry/replay"
    );
    assert!(!directed_event_visible_to(
        &decode_envelope(&original).unwrap(),
        Some(late)
    ));
}

#[tokio::test]
async fn directed_presence_targets_exclude_other_devices_and_snapshot_handoff_cannot_leak() {
    let fixture = Fixture::new();
    let recipient = fixture.sink(fixture.recipient, true, true);
    let second = fixture.sink((fixture.second_device, fixture.recipient.1), true, true);
    let bystander = fixture.sink(fixture.bystander, true, true);
    let _registrations: Vec<_> = [&recipient, &second, &bystander]
        .into_iter()
        .map(|sink| {
            fixture
                .state
                .delivery
                .register_sink(fixture.instance, sink.clone())
        })
        .collect();
    let mut event = request();
    event.recipient_presence_ids = vec![fixture.recipient.0.to_string()];
    fixture.publish(event).await.unwrap();
    assert_eq!(recipient.queue().lock().unwrap().depth().reliable, 1);
    for sink in [&second, &bystander] {
        assert_eq!(sink.queue().lock().unwrap().len(), 0);
        assert!(sink.filter.lock().unwrap().raw_snapshot_messages.is_empty());
    }
    recipient.install_snapshot(100, Default::default());
    let messages = take(&recipient);
    assert_eq!(
        messages.len(),
        1,
        "entity snapshot boundary must not swallow an addressed event"
    );
    let raw = messages[0].encode_to_vec();
    assert!(second.prepare_delivery(&raw).is_none());
    assert!(bystander.prepare_delivery(&raw).is_none());
    recipient.finish_snapshot();
    assert!(!recipient.filter.lock().unwrap().snapshot_pending);
}

#[tokio::test]
async fn directed_invalid_audience_is_all_or_none_and_cannot_read_another_senders_acknowledgement()
{
    let fixture = Fixture::new();
    let foreign_instance = InstanceId::generate();
    let foreign_member = (PresenceId::generate(), UserId::generate());
    let mut foreign_actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id: foreign_instance,
        state: RuntimeState::Running,
        revision: orbisync_domain::Revision::INITIAL,
    });
    foreign_actor.start();
    assert!(matches!(
        foreign_actor.handle(InstanceCommand::Join {
            presence_id: foreign_member.0,
            user_id: foreign_member.1,
            instance_id: foreign_instance,
            capacity: 100,
        }),
        CommandOutcome::Applied { .. }
    ));
    fixture
        .state
        .registry
        .insert(foreign_instance, foreign_actor)
        .unwrap();
    let recipient = fixture.sink(fixture.recipient, true, false);
    let _registration = fixture
        .state
        .delivery
        .register_sink(fixture.instance, recipient.clone());
    let before = fixture
        .state
        .registry
        .read_snapshot(fixture.instance)
        .await
        .unwrap()
        .revision;
    for by_user in [false, true] {
        let mut event = request();
        if by_user {
            event.recipient_user_ids = vec![
                fixture.recipient.1.to_string(),
                foreign_member.1.to_string(),
            ];
        } else {
            event.recipient_presence_ids = vec![
                fixture.recipient.0.to_string(),
                foreign_member.0.to_string(),
            ];
        }
        assert_eq!(
            fixture.publish(event).await.unwrap_err().0,
            "RECIPIENT_NOT_JOINED"
        );
        assert!(take(&recipient).is_empty());
        let read = fixture
            .state
            .registry
            .read_snapshot(fixture.instance)
            .await
            .unwrap();
        assert_eq!(read.revision, before);
        assert!(read.reliable_events.is_empty());
    }
    let mut event = request();
    event.recipient_presence_ids = vec![fixture.recipient.0.to_string()];
    fixture.publish(event.clone()).await.unwrap();
    take(&recipient);
    let mut changed = event.clone();
    changed.recipient_presence_ids = vec![fixture.bystander.0.to_string()];
    assert_eq!(
        fixture.publish(changed).await.unwrap_err().0,
        "EVENT_ID_CONFLICT"
    );
    assert_eq!(
        publish_directed_event(
            &fixture.state,
            fixture.instance,
            &fixture.instance.to_string(),
            fixture.bystander.1,
            fixture.bystander.0,
            event,
            0
        )
        .await
        .unwrap_err()
        .0,
        "EVENT_ID_CONFLICT"
    );
    assert!(take(&recipient).is_empty());
}

#[tokio::test]
async fn directed_fails_closed_without_feature_or_presence_and_checks_resolved_wire_size_before_commit()
 {
    let fixture = Fixture::new();
    let old = fixture.sink(fixture.recipient, false, false);
    let unbound = Arc::new(RealtimeDeliverySink::new(
        fixture.state.clone(),
        fixture.instance,
        Vec3::new(0.0, 0.0, 0.0).unwrap(),
        fixture.recipient.1,
        None,
        Default::default(),
    ));
    unbound.set_directed_events_enabled(true);
    let _old = fixture
        .state
        .delivery
        .register_sink(fixture.instance, old.clone());
    let _unbound = fixture
        .state
        .delivery
        .register_sink(fixture.instance, unbound.clone());
    let mut event = request();
    event.recipient_user_ids = vec![fixture.recipient.1.to_string()];
    fixture.publish(event).await.unwrap();
    assert!(take(&old).is_empty());
    assert!(take(&unbound).is_empty());

    let mut config = orbisync_config::Config::default().realtime;
    config.max_message_bytes = 1;
    let small = Fixture::with_config(config);
    let before = small
        .state
        .registry
        .read_snapshot(small.instance)
        .await
        .unwrap()
        .revision;
    let mut event = request();
    event.recipient_user_ids = vec![small.recipient.1.to_string()];
    assert_eq!(
        small.publish(event).await.unwrap_err().0,
        "MESSAGE_TOO_LARGE"
    );
    let after = small
        .state
        .registry
        .read_snapshot(small.instance)
        .await
        .unwrap();
    assert_eq!(after.revision, before);
    assert!(after.reliable_events.is_empty());
}

async fn receive(output: &mut tokio::sync::mpsc::Receiver<Message>) -> Envelope {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), output.recv())
        .await
        .unwrap()
        .unwrap();
    let Message::Binary(bytes) = frame else {
        panic!("binary frame expected");
    };
    decode_envelope(&bytes).unwrap()
}

struct ResumedConnection {
    input: tokio::sync::mpsc::Sender<Message>,
    output: tokio::sync::mpsc::Receiver<Message>,
    task: JoinHandle<()>,
    history: Vec<Envelope>,
}

impl ResumedConnection {
    async fn close(self) -> Vec<Envelope> {
        self.input.send(Message::Close(None)).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap();
        self.history
    }
}

async fn resume_connection(
    fixture: &Fixture,
    member: (PresenceId, UserId),
    cursor: orbisync_domain::Revision,
    enabled: bool,
) -> ResumedConnection {
    let token = fixture
        .state
        .resume_store
        .issue(member.1, fixture.instance, member.0, 0, cursor);
    let (socket, input, mut output) = RealtimeSocket::controlled();
    let task = tokio::spawn(handle_socket(
        socket,
        fixture.state.clone(),
        Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL.into()),
    ));
    input
        .send(Message::Binary(
            Envelope {
                protocol_major: 1,
                sequence: 1,
                payload: Some(envelope::Payload::ClientHello(
                    orbisync_protocol::v1::ClientHello {
                        realtime_ticket: member.1.to_string(),
                        supported_features: if enabled {
                            vec![FEATURE.to_owned()]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .unwrap();
    let hello = receive(&mut output).await;
    assert!(
        matches!(hello.payload, Some(envelope::Payload::ServerHello(ref hello))
        if hello.enabled_features.iter().any(|value| value == FEATURE) == enabled)
    );
    input
        .send(Message::Binary(
            Envelope {
                protocol_major: 1,
                sequence: 2,
                payload: Some(envelope::Payload::ResumeSession(
                    orbisync_protocol::v1::ResumeSession {
                        resume_token: token,
                        last_applied_revision: cursor.as_u64(),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut output).await.payload,
        Some(envelope::Payload::ResumeAccepted(_))
    ));
    let mut events = Vec::new();
    loop {
        let frame = receive(&mut output).await;
        if let Some(envelope::Payload::Snapshot(ref snapshot)) = frame.payload {
            if snapshot.chunk_index + 1 == snapshot.chunk_count {
                break;
            }
        } else {
            events.push(frame);
        }
    }
    ResumedConnection {
        input,
        output,
        task,
        history: events,
    }
}

async fn resumed_history(
    fixture: &Fixture,
    member: (PresenceId, UserId),
    cursor: orbisync_domain::Revision,
    enabled: bool,
) -> Vec<Envelope> {
    resume_connection(fixture, member, cursor, enabled)
        .await
        .close()
        .await
}

#[tokio::test]
async fn directed_actual_resume_replays_only_frozen_audience_and_requires_negotiation() {
    let fixture = Fixture::new();
    let cursor = fixture
        .state
        .registry
        .read_snapshot(fixture.instance)
        .await
        .unwrap()
        .revision;
    let mut event = request();
    event.recipient_user_ids = vec![fixture.recipient.1.to_string()];
    fixture.publish(event).await.unwrap();
    publish_client_event(
        &fixture.state,
        fixture.instance,
        &fixture.instance.to_string(),
        fixture.sender.1,
        fixture.sender.0,
        DomainEvent {
            event_id: orbisync_domain::CommandId::generate().to_string(),
            event_type: "custom.public".into(),
            ..Default::default()
        },
        0,
    )
    .await
    .unwrap();
    let targeted = resumed_history(&fixture, fixture.recipient, cursor, true).await;
    assert_eq!(targeted.len(), 2);
    assert!(matches!(
        targeted[0].payload,
        Some(envelope::Payload::DirectedEvent(_))
    ));
    assert!(matches!(
        targeted[1].payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    let unrelated = resumed_history(&fixture, fixture.bystander, cursor, true).await;
    assert_eq!(unrelated.len(), 1);
    assert!(matches!(
        unrelated[0].payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    let old_client = resumed_history(
        &fixture,
        (fixture.second_device, fixture.recipient.1),
        cursor,
        false,
    )
    .await;
    assert_eq!(old_client.len(), 1);
    assert!(matches!(
        old_client[0].payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    let late = PresenceId::generate();
    fixture
        .state
        .registry
        .submit(
            fixture.instance,
            InstanceCommand::Join {
                presence_id: late,
                user_id: fixture.recipient.1,
                instance_id: fixture.instance,
                capacity: 100,
            },
        )
        .await
        .unwrap();
    // Even an overly permissive cursor cannot widen the fixed private audience.
    let late_client = resumed_history(&fixture, (late, fixture.recipient.1), cursor, true).await;
    assert_eq!(late_client.len(), 1);
    assert!(matches!(
        late_client[0].payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
}

fn wire_event(instance: InstanceId, sequence: u64, event: DirectedEvent) -> Message {
    Message::Binary(
        Envelope {
            protocol_major: 1,
            instance_id: instance.to_string(),
            sequence,
            message_id: uuid::Uuid::now_v7().to_string(),
            payload: Some(envelope::Payload::DirectedEvent(event)),
            ..Default::default()
        }
        .encode_to_vec()
        .into(),
    )
}

#[tokio::test]
async fn directed_connected_socket_enforces_feature_gate_targets_and_duplicate_echo() {
    let fixture = Fixture::new();
    let cursor = fixture
        .state
        .registry
        .read_snapshot(fixture.instance)
        .await
        .unwrap()
        .revision;
    let mut sender = resume_connection(&fixture, fixture.sender, cursor, true).await;
    let mut recipient = resume_connection(&fixture, fixture.recipient, cursor, true).await;
    let mut bystander = resume_connection(&fixture, fixture.bystander, cursor, false).await;
    let mut event = request();
    event.recipient_presence_ids = vec![fixture.recipient.0.to_string()];
    bystander
        .input
        .send(wire_event(fixture.instance, 3, event.clone()))
        .await
        .unwrap();
    assert!(matches!(receive(&mut bystander.output).await.payload,
        Some(envelope::Payload::Error(ref error)) if error.code == "FEATURE_NOT_NEGOTIATED"));
    assert_eq!(
        fixture
            .state
            .registry
            .read_snapshot(fixture.instance)
            .await
            .unwrap()
            .revision,
        cursor
    );

    sender
        .input
        .send(wire_event(fixture.instance, 3, event.clone()))
        .await
        .unwrap();
    let echo = receive(&mut sender.output).await;
    let received = receive(&mut recipient.output).await;
    assert_eq!(directed(&echo), directed(&received));
    assert!(
        directed(&received)
            .resolved_presence_ids
            .contains(&fixture.recipient.0.to_string())
    );
    assert!(
        !directed(&received)
            .resolved_presence_ids
            .contains(&fixture.bystander.0.to_string())
    );
    let committed = fixture
        .state
        .registry
        .read_snapshot(fixture.instance)
        .await
        .unwrap()
        .revision;
    sender
        .input
        .send(wire_event(fixture.instance, 4, event))
        .await
        .unwrap();
    assert_eq!(
        directed(&receive(&mut sender.output).await),
        directed(&echo)
    );
    assert_eq!(
        fixture
            .state
            .registry
            .read_snapshot(fixture.instance)
            .await
            .unwrap()
            .revision,
        committed
    );
    assert!(
        recipient.output.try_recv().is_err(),
        "duplicate is an echo only"
    );
    assert!(
        bystander.output.try_recv().is_err(),
        "private frame cannot reach an unrelated legacy client"
    );
    sender.close().await;
    recipient.close().await;
    bystander.close().await;
}
