// Real socket coverage included by realtime_e2e.rs, using its router fixtures.

#[tokio::test]
async fn heartbeat_before_join_keeps_ready_connection_usable() {
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let iid = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    let mut ws = ws_connect(server.addr).await;
    handshake(&mut ws, TICKET).await;
    let mut heartbeat = Envelope::decode(join_instance_bytes(iid).as_slice()).unwrap();
    heartbeat.payload = Some(envelope::Payload::Heartbeat(orbisync_protocol::v1::Heartbeat { client_time_unix_ms: 123 }));
    ws.send(Message::Binary(heartbeat.encode_to_vec().into())).await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
    let Message::Binary(bytes) = response else { panic!("ready connection acknowledges heartbeat"); };
    let response = Envelope::decode(bytes).unwrap();
    assert_eq!(response.sequence, 2);
    assert!(matches!(response.payload, Some(envelope::Payload::HeartbeatAck(ref ack)) if ack.client_time_unix_ms == 123));
    let mut join = Envelope::decode(join_instance_bytes(iid).as_slice()).unwrap();
    join.sequence = 3;
    ws.send(Message::Binary(join.encode_to_vec().into())).await.unwrap();
    let response = recv_envelope(&mut ws).await.expect("join after heartbeat");
    assert_eq!(response.sequence, 3);
    assert!(matches!(response.payload, Some(envelope::Payload::JoinAccepted(_))));
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn invalid_transforms_return_errors_and_keep_the_connection_usable() {
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let iid = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    let mut ws = ws_connect(server.addr).await;
    handshake(&mut ws, TICKET).await;
    join_instance(&mut ws, iid).await;
    let entity = orbisync_domain::EntityId::generate();
    let initial = transform_input_bytes(entity, 1, orbisync_domain::Vec3::new(1.0, 0.0, 0.0).unwrap(), 3);
    let revision = server.registry.read_snapshot(iid).await.unwrap().revision;
    for index in 0..3 {
        let mut request = Envelope::decode(initial.as_slice()).unwrap();
        request.sequence = index + 3;
        if let Some(envelope::Payload::TransformInput(input)) = request.payload.as_mut() {
            match index {
                0 => input.transform = None,
                1 => input.transform.as_mut().unwrap().position_x = f32::NAN,
                _ => input.transform.as_mut().unwrap().rotation_w = f32::INFINITY,
            }
        }
        ws.send(Message::Binary(request.encode_to_vec().into())).await.unwrap();
        let response = recv_envelope(&mut ws).await.expect("invalid transform must receive an error");
        assert!(matches!(response.payload, Some(envelope::Payload::Error(ref error)) if error.code == "INVALID_ARGUMENT" && error.request_message_id == request.message_id));
        assert_eq!(server.registry.read_snapshot(iid).await.unwrap().revision, revision);
    }
    let mut valid = Envelope::decode(initial.as_slice()).unwrap();
    valid.sequence = 6;
    ws.send(Message::Binary(valid.encode_to_vec().into())).await.unwrap();
    assert!(matches!(recv_envelope(&mut ws).await.unwrap().payload, Some(envelope::Payload::StateDelta(_))));
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn custom_event_retry_acknowledges_without_rebroadcast_or_revision_change() {
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let iid = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    let mut sender = ws_connect(server.addr).await;
    handshake(&mut sender, TICKET).await;
    join_instance(&mut sender, iid).await;
    let mut peer = ws_connect(server.addr).await;
    handshake(&mut peer, TICKET).await;
    join_instance(&mut peer, iid).await;
    let mut request = custom_event(iid, 3);
    sender.send(Message::Binary(encode_envelope(&request).into())).await.unwrap();
    let original = recv_envelope(&mut sender).await.unwrap();
    recv_envelope(&mut peer).await.unwrap();
    let revision = server.registry.read_snapshot(iid).await.unwrap().revision;
    request.sequence = 4;
    sender.send(Message::Binary(encode_envelope(&request).into())).await.unwrap();
    let repeated = recv_envelope(&mut sender).await.unwrap();
    assert_eq!(original.message_id, repeated.message_id);
    assert_eq!(original.payload, repeated.payload);
    assert_eq!(server.registry.read_snapshot(iid).await.unwrap().revision, revision);
    assert!(tokio::time::timeout(Duration::from_millis(100), peer.next()).await.is_err(), "duplicate must not reach peers");
    request.sequence = 5;
    if let Some(envelope::Payload::DomainEvent(event)) = request.payload.as_mut() {
        event.event_type = "custom.changed".to_owned();
    }
    sender.send(Message::Binary(encode_envelope(&request).into())).await.unwrap();
    let conflict = recv_envelope(&mut sender).await.unwrap();
    assert!(matches!(conflict.payload, Some(envelope::Payload::Error(ref error)) if error.code == "EVENT_ID_CONFLICT"));
    sender.close(None).await.unwrap();
    peer.close(None).await.unwrap();
}

#[tokio::test]
async fn messages_during_chunked_join_are_delivered_after_the_complete_snapshot() {
    use orbisync_world_runtime::command::{CommandOutcome, InstanceCommand, WorldPermissions};
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let iid = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    let mut sender = ws_connect(server.addr).await;
    handshake(&mut sender, TICKET).await;
    let (joined, _) = join_instance(&mut sender, iid).await;
    let Some(envelope::Payload::JoinAccepted(joined)) = joined.payload else {
        panic!("join");
    };
    let user = orbisync_domain::UserId::parse(&joined.user_id).unwrap();
    let entity = orbisync_domain::EntityId::generate();
    sender
        .send(Message::Binary(
            transform_input_bytes(
                entity,
                1,
                orbisync_domain::Vec3::new(1.0, 0.0, 0.0).unwrap(),
                3,
            )
            .into(),
        ))
        .await
        .unwrap();
    recv_envelope(&mut sender).await.unwrap();
    for index in 0..2 {
        let result = server
            .registry
            .submit(
                iid,
                InstanceCommand::UpdateEntityComponent {
                    command_id: None,
                    entity_id: entity,
                    component_key: format!("test.chunk{index}"),
                    payload_bytes: vec![128; 4000],
                    expected_revision: orbisync_domain::Revision::from_u64(index + 1),
                    now: now_ts(),
                    requester: user,
                    permissions: WorldPermissions::all(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(result, CommandOutcome::Applied { .. }));
    }
    let mut joining = ws_connect(server.addr).await;
    handshake(&mut joining, TICKET).await;
    joining
        .send(Message::Binary(join_instance_bytes(iid).into()))
        .await
        .unwrap();
    assert!(matches!(
        recv_envelope(&mut joining).await.unwrap().payload,
        Some(envelope::Payload::JoinAccepted(_))
    ));
    // Publish while the receiving application has not consumed any snapshot
    // chunks yet; the server must already have installed the live sink.
    let mut ids = Vec::new();
    for sequence in 4..7 {
        let event = custom_event(iid, sequence);
        ids.push(event.message_id.clone());
        sender
            .send(Message::Binary(encode_envelope(&event).into()))
            .await
            .unwrap();
        loop {
            match recv_envelope(&mut sender).await.unwrap().payload {
                // Creation can leave a separate properties fragment queued.
                Some(envelope::Payload::StateDelta(_)) => continue,
                Some(envelope::Payload::DomainEvent(ack)) => {
                    assert_eq!(ack.event_id, event.message_id);
                    break;
                }
                other => panic!("expected event acknowledgement, got {other:?}"),
            }
        }
    }
    server.clock.advance_millis(500);
    sender
        .send(Message::Binary(
            transform_input_bytes(
                entity,
                3,
                orbisync_domain::Vec3::new(2.0, 0.0, 0.0).unwrap(),
                7,
            )
            .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        recv_envelope(&mut sender).await.unwrap().payload,
        Some(envelope::Payload::StateDelta(_))
    ));
    let mut chunks = Vec::new();
    let mut chunk_count = 0;
    loop {
        let envelope = recv_envelope(&mut joining).await.unwrap();
        match envelope.payload {
            Some(envelope::Payload::Snapshot(snapshot)) => {
                chunk_count = snapshot.chunk_count;
                assert_eq!(snapshot.chunk_index as usize, chunks.len());
                chunks.push(snapshot.data);
            }
            Some(envelope::Payload::DomainEvent(_)) => {
                assert!(chunk_count > 1);
                assert_eq!(chunks.len(), chunk_count as usize);
                assert_eq!(envelope.message_id, ids.remove(0));
                if ids.is_empty() {
                    break;
                }
            }
            other => panic!("unexpected handoff payload {other:?}"),
        }
    }
    let snapshot: serde_json::Value = serde_json::from_slice(&chunks.concat()).unwrap();
    let item = snapshot["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["entity_id"] == entity.to_string())
        .unwrap();
    assert_eq!(
        item["components"]["test.chunk1"].as_array().unwrap().len(),
        4000
    );
    let Some(envelope::Payload::StateDelta(delta)) =
        recv_envelope(&mut joining).await.unwrap().payload
    else {
        panic!("live delta after snapshot");
    };
    let update = delta
        .entities
        .iter()
        .find(|value| value.entity_id == entity.to_string())
        .unwrap();
    assert_eq!(update.revision, 4);
    assert_eq!(update.transform.as_ref().unwrap().position_x, 2.0);
    joining.close(None).await.unwrap();
    sender.close(None).await.unwrap();
}

fn custom_event(instance: InstanceId, sequence: u64) -> Envelope {
    let id = uuid::Uuid::now_v7().to_string();
    Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: id.clone(),
        sequence,
        sent_at_unix_ms: 1,
        instance_id: instance.to_string(),
        payload: Some(envelope::Payload::DomainEvent(
            orbisync_protocol::v1::DomainEvent {
                event_id: id,
                event_type: "custom.chat.message".to_owned(),
                instance_revision: u64::MAX,
                data: Some(prost_types::Struct {
                    fields: [
                        (
                            "text".to_owned(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    "こんにちは".to_owned(),
                                )),
                            },
                        ),
                        (
                            "sender_user_id".to_owned(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    "forged".to_owned(),
                                )),
                            },
                        ),
                    ]
                    .into_iter()
                    .collect(),
                }),
            },
        )),
    }
}

#[tokio::test]
async fn custom_event_reaches_both_room_members_in_order_without_cross_room_leak() {
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let other = make_instance(world.id(), 5);
    let iid = instance.id();
    let other_id = other.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    server.store.insert_instance(other);
    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    let mut c = ws_connect(server.addr).await;
    for socket in [&mut a, &mut b, &mut c] {
        handshake(socket, TICKET).await;
    }
    let (accepted, _) = join_instance(&mut a, iid).await;
    let Some(envelope::Payload::JoinAccepted(accepted)) = accepted.payload else {
        panic!("join");
    };
    join_instance(&mut b, iid).await;
    join_instance(&mut c, other_id).await;
    let mut previous_revision = 0;
    for seq in 3..6 {
        let request = custom_event(iid, seq);
        a.send(Message::Binary(encode_envelope(&request).into()))
            .await
            .unwrap();
        let sent = recv_envelope(&mut a).await.unwrap();
        let received = recv_envelope(&mut b).await.unwrap();
        assert_eq!(received.message_id, request.message_id);
        assert_eq!(received.instance_id, iid.to_string());
        assert_eq!(received.payload, sent.payload);
        let Some(envelope::Payload::DomainEvent(event)) = received.payload else {
            panic!("event");
        };
        assert!(event.instance_revision > previous_revision && event.instance_revision < u64::MAX);
        previous_revision = event.instance_revision;
        let data = event.data.unwrap();
        assert_eq!(
            data.fields["sender_user_id"].kind,
            Some(prost_types::value::Kind::StringValue(
                accepted.user_id.clone()
            ))
        );
        assert_eq!(
            data.fields["sender_presence_id"].kind,
            Some(prost_types::value::Kind::StringValue(
                accepted.presence_id.clone()
            ))
        );
        assert_eq!(
            data.fields["text"].kind,
            Some(prost_types::value::Kind::StringValue(
                "こんにちは".to_owned()
            ))
        );
    }
    assert!(
        !try_recv_envelope(&mut c).await,
        "other room must not receive the event"
    );
    // A busy room must not stall delivery in an independent room.
    let guard = server.registry.lifecycle_guard(iid).await;
    a.send(Message::Binary(
        encode_envelope(&custom_event(iid, 6)).into(),
    ))
    .await
    .unwrap();
    c.send(Message::Binary(
        encode_envelope(&custom_event(other_id, 3)).into(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        recv_envelope(&mut c).await.unwrap().payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    drop(guard);
    assert!(matches!(
        recv_envelope(&mut a).await.unwrap().payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    assert!(matches!(
        recv_envelope(&mut b).await.unwrap().payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    for socket in [&mut a, &mut b, &mut c] {
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn custom_event_invalid_requests_and_unsupported_payloads_are_explicitly_rejected() {
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let iid = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, iid).await;
    join_instance(&mut b, iid).await;
    for (index, expected) in [
        "INVALID_ARGUMENT",
        "INVALID_ARGUMENT",
        "INSTANCE_MISMATCH",
        "INVALID_MESSAGE",
        "INVALID_MESSAGE",
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = custom_event(iid, index as u64 + 3);
        match index {
            0 => {
                if let Some(envelope::Payload::DomainEvent(event)) = &mut request.payload {
                    event.event_type = "role.changed".to_owned();
                }
            }
            1 => {
                if let Some(envelope::Payload::DomainEvent(event)) = &mut request.payload {
                    event.event_id = "invalid-id".to_owned();
                }
            }
            2 => request.instance_id = InstanceId::generate().to_string(),
            3 => {
                request.payload = Some(envelope::Payload::StateDelta(
                    orbisync_protocol::v1::StateDelta::default(),
                ))
            }
            _ => request.payload = None,
        }
        a.send(Message::Binary(encode_envelope(&request).into()))
            .await
            .unwrap();
        let response = recv_envelope(&mut a).await.unwrap();
        let Some(envelope::Payload::Error(error)) = response.payload else {
            panic!("explicit error required");
        };
        assert_eq!(error.code, expected);
        assert_eq!(error.request_message_id, request.message_id);
    }
    assert!(
        !try_recv_envelope(&mut b).await,
        "rejected events must never be broadcast"
    );
    // Rejection must not consume the connection's ability to send valid input.
    a.send(Message::Binary(
        encode_envelope(&custom_event(iid, 8)).into(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        recv_envelope(&mut b).await.unwrap().payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    assert!(matches!(
        recv_envelope(&mut a).await.unwrap().payload,
        Some(envelope::Payload::DomainEvent(_))
    ));
    // An undecodable protobuf frame must also receive an explicit error.
    a.send(Message::Binary(vec![0x80].into())).await.unwrap();
    let Some(envelope::Payload::Error(error)) = recv_envelope(&mut a).await.unwrap().payload else {
        panic!("malformed protobuf must not be silently ignored");
    };
    assert_eq!(error.code, "INVALID_MESSAGE");
    assert!(!try_recv_envelope(&mut b).await);
    b.close(None).await.unwrap();
}

#[tokio::test]
async fn custom_event_is_rejected_after_membership_is_removed() {
    let world = make_world();
    let instance = make_instance(world.id(), 5);
    let iid = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    let (accepted, _) = join_instance(&mut a, iid).await;
    let Some(envelope::Payload::JoinAccepted(accepted)) = accepted.payload else {
        panic!("join");
    };
    server
        .registry
        .submit(
            iid,
            orbisync_world_runtime::command::InstanceCommand::Leave {
                presence_id: orbisync_domain::PresenceId::parse(&accepted.presence_id).unwrap(),
            },
        )
        .await
        .unwrap();
    a.send(Message::Binary(
        encode_envelope(&custom_event(iid, 3)).into(),
    ))
    .await
    .unwrap();
    let Some(envelope::Payload::Error(error)) = recv_envelope(&mut a).await.unwrap().payload else {
        panic!("error");
    };
    assert_eq!(error.code, "NOT_JOINED");
    a.close(None).await.unwrap();
}
