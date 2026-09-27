    #[test]
    fn sync_actor_outcomes_keep_entity_and_instance_boundaries_through_recreation() {
        use orbisync_domain::{EntityKind, Revision, Timestamp, VisibilityPolicy};
        let (mut actor, _, _, user, _) = snapshot_fixture();
        let id = EntityId::generate();
        let spawn = || InstanceCommand::SpawnEntity {
            command_id: None, entity_id: id, kind: EntityKind::Object, owner: Some(user),
            transform: None, visibility: VisibilityPolicy::Global, requester: user,
            permissions: WorldPermissions::all(),
        };
        let CommandOutcome::Applied { revision: first_boundary, entity_revision, .. } = actor.handle(spawn()) else {
            panic!("spawn failed");
        };
        assert_eq!(entity_revision, Some(Revision::from_u64(1)));
        assert!(first_boundary > Revision::from_u64(1));
        assert!(actor.entities_snapshot().iter().find(|entity| entity.id() == id).expect("spawned").components().is_empty());
        let CommandOutcome::Applied { revision: update_boundary, entity_revision: update_revision, .. } = actor.handle(
            InstanceCommand::UpdateEntityComponent {
                command_id: None, entity_id: id, component_key: "test.note".to_owned(),
                payload_bytes: br#"{"text":"persisted","color":"mint"}"#.to_vec(),
                expected_revision: Revision::from_u64(1), now: Timestamp::from_unix_millis(2_000).expect("time"),
                requester: user, permissions: WorldPermissions::all(),
            },
        ) else { panic!("update failed"); };
        assert_eq!(update_revision, Some(Revision::from_u64(2)));
        assert!(update_boundary > first_boundary);
        let CommandOutcome::Applied { revision: deleted_boundary, entity_revision, .. } = actor.handle(
            InstanceCommand::DeleteEntity {
                command_id: None, entity_id: id, expected_revision: Revision::from_u64(2),
                requester: user, permissions: WorldPermissions::all(),
            },
        ) else { panic!("delete failed"); };
        assert_eq!(entity_revision, None);
        let CommandOutcome::Applied { revision: recreated_boundary, entity_revision, .. } = actor.handle(spawn()) else {
            panic!("recreation failed");
        };
        assert_eq!(entity_revision, Some(Revision::from_u64(1)));
        assert!(recreated_boundary > deleted_boundary);
        // A later read no longer has the update's revision; the retained outcome does.
        assert_eq!(actor.entities_snapshot().iter().find(|entity| entity.id() == id).expect("recreated").revision(), Revision::from_u64(1));
        assert_eq!(update_revision, Some(Revision::from_u64(2)));
    }

    #[tokio::test]
    async fn sync_snapshot_header_and_json_share_a_read_despite_later_actor_changes() {
        let (actor, instance, world, user, _) = snapshot_fixture();
        let registry = RuntimeRegistry::new();
        registry.insert(instance.id(), actor).expect("insert");
        let read = registry.read_snapshot(instance.id()).await.expect("read");
        let initial_revision = read.revision;
        registry.submit(instance.id(), InstanceCommand::Join {
            presence_id: PresenceId::generate(), user_id: UserId::generate(),
            instance_id: instance.id(), capacity: 100,
        }).await.expect("advance actor");
        let (boundary, data) = prepare_snapshot_for_viewer(
            &read, Some(Vec3::new(0.0, 0.0, 0.0).expect("position")), Some(user),
            &orbisync_interest::UniformGrid::default(), &mut std::collections::HashMap::new(), None,
            SnapshotContext { instance: &instance, world: &world, metrics: &NoopMetrics,
                permissions: WorldPermissions::all(), server_time_unix_ms: 1000 },
        );
        let json: serde_json::Value = serde_json::from_slice(&data).expect("json");
        assert_eq!(boundary, initial_revision.as_u64());
        assert_eq!(json["revision"].as_u64(), Some(boundary));
        assert!(registry.read_snapshot(instance.id()).await.expect("later").revision > initial_revision);
        assert!(snapshot_payloads("read", &data, boundary).iter().all(|chunk| chunk.instance_revision == boundary));
    }

    fn sync_sink(capacity: u32) -> (Arc<RealtimeState>, Arc<RealtimeDeliverySink>, InstanceId) {
        let id = InstanceId::generate();
        let mut config = orbisync_config::Config::default().realtime;
        config.outbound_queue_capacity = capacity;
        let registry = Arc::new(RuntimeRegistry::new());
        let mut actor = InstanceActor::new(orbisync_world_runtime::InstanceRuntimeDescriptor {
            instance_id: id,
            state: orbisync_world_runtime::RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        registry.insert(id, actor).expect("sync fixture actor");
        let state = Arc::new(RealtimeState::new(config, registry,
            Arc::new(DeliveryRegistry::new()), Arc::new(orbisync_domain::SystemClock::new())));
        let sink = Arc::new(RealtimeDeliverySink::new(Arc::clone(&state), id,
            Vec3::new(0.0, 0.0, 0.0).expect("position"), UserId::generate(), None,
            std::collections::HashMap::new()));
        sink.begin_snapshot();
        (state, sink, id)
    }

    fn sync_command_frame(instance: InstanceId, entity: EntityId, operation: &str, boundary: u64, revision: u64) -> Vec<u8> {
        Envelope {
            protocol_major: 1, instance_id: instance.to_string(), message_id: uuid::Uuid::now_v7().to_string(),
            payload: Some(envelope::Payload::EntityCommand(orbisync_protocol::v1::EntityCommand {
                command_id: uuid::Uuid::now_v7().to_string(), entity_id: entity.to_string(),
                expected_revision: revision, operation: operation.to_owned(), instance_revision: Some(boundary),
                arguments: Some(prost_types::Struct { fields: std::collections::BTreeMap::from([
                    ("visibility".to_owned(), prost_types::Value { kind: Some(prost_types::value::Kind::StringValue("global".to_owned())) }),
                ]) }),
            })), ..Envelope::default()
        }.encode_to_vec()
    }

    #[test]
    fn sync_handoff_uses_one_registered_queue_and_filters_pre_snapshot_commands() {
        let (state, sink, instance) = sync_sink(8);
        let registration = state.delivery.register_sink(instance, Arc::clone(&sink) as Arc<dyn DeliverySink>);
        let entity = EntityId::generate();
        let frames = [sync_command_frame(instance, entity, "spawn", 10, 1),
            sync_command_frame(instance, entity, "delete", 11, 11),
            sync_command_frame(instance, entity, "spawn", 12, 1)];
        for frame in &frames {
            assert_eq!(sync_test_broadcast(&state, instance, frame.clone(), Reliability::Reliable).delivered, 1);
        }
        sink.install_snapshot(10, std::collections::HashMap::new());
        sink.finish_snapshot(); // Must remain in FIFO handoff mode while data is queued.
        let items = sink.queue().lock().expect("queue").drain_batch(8);
        let delivered: Vec<_> = items.iter().filter_map(|item| sink.prepare_delivery(item)).collect();
        assert_eq!(delivered.len(), 2);
        for (env, boundary) in delivered.iter().zip([11, 12]) {
            let Some(envelope::Payload::EntityCommand(command)) = env.payload.as_ref() else { panic!("command"); };
            assert_eq!(command.instance_revision, Some(boundary));
        }
        sink.finish_snapshot();
        // Delayed publication of a commit already covered by the snapshot stays discarded.
        assert!(sink.prepare_delivery(&frames[0]).is_none());
        assert!(sink.filter.lock().expect("filter").raw_snapshot_messages.is_empty());
        assert!(!sink.filter.lock().expect("filter").snapshot_pending);
        drop(registration);
        assert_eq!(sync_test_broadcast(&state, instance, frames[2].clone(), Reliability::Reliable).delivered, 0);
    }

    #[test]
    fn sync_handoff_filters_private_raw_messages_before_any_delivery() {
        let (state, sink, instance) = sync_sink(8);
        let _registration = state.delivery.register_sink(instance, Arc::clone(&sink) as Arc<dyn DeliverySink>);
        let frame = sync_command_frame(instance, EntityId::generate(), "spawn", 11, 1);
        let mut env = decode_envelope(&frame).expect("decode");
        let Some(envelope::Payload::EntityCommand(command)) = env.payload.as_mut() else { panic!("command"); };
        command.arguments.as_mut().expect("args").fields.insert("visibility".to_owned(), prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue("owner_only".to_owned())),
        });
        assert_eq!(sync_test_broadcast(&state, instance, env.encode_to_vec(), Reliability::Reliable).delivered, 1);
        sink.install_snapshot(10, std::collections::HashMap::new());
        let queued = sink.queue().lock().expect("queue").drain_batch(8);
        assert_eq!(queued.len(), 1);
        assert!(sink.prepare_delivery(&queued[0]).is_none());
    }

    #[test]
    fn sync_handoff_retains_latest_lane_messages_reliably_until_snapshot_drain() {
        let (state, sink, instance) = sync_sink(2);
        let _registration = state.delivery.register_sink(instance, Arc::clone(&sink) as Arc<dyn DeliverySink>);
        let frame = Envelope {
            instance_id: instance.to_string(), message_id: "delta-1".to_owned(),
            payload: Some(envelope::Payload::StateDelta(orbisync_protocol::v1::StateDelta {
                from_revision: 10, to_revision: 11,
                entities: vec![orbisync_protocol::v1::EntityState { entity_id: EntityId::generate().to_string(), revision: 2, ..Default::default() }],
            })), ..Default::default()
        };
        assert_eq!(sync_test_broadcast(&state, instance, frame.encode_to_vec(), Reliability::LatestWins).delivered, 1);
        let mut second = frame.clone();
        second.message_id = "delta-2".to_owned();
        assert_eq!(sync_test_broadcast(&state, instance, second.encode_to_vec(), Reliability::LatestWins).delivered, 1);
        let depth = sink.queue().lock().expect("queue").depth();
        assert_eq!(depth.reliable, 2);
        assert_eq!(depth.latest, 0);
        // Even latest-wins overflow must invalidate initial synchronization.
        assert_eq!(sync_test_broadcast(&state, instance, frame.encode_to_vec(), Reliability::LatestWins).disconnected_slow_consumers, 1);
        assert!(sink.is_closed());
    }

    #[test]
    fn sync_handoff_overflow_disconnects_instead_of_silently_losing_state() {
        let (state, sink, instance) = sync_sink(1);
        let _registration = state.delivery.register_sink(instance, Arc::clone(&sink) as Arc<dyn DeliverySink>);
        let frame = sync_command_frame(instance, EntityId::generate(), "spawn", 11, 1);
        assert_eq!(sync_test_broadcast(&state, instance, frame.clone(), Reliability::Reliable).delivered, 1);
        assert_eq!(sync_test_broadcast(&state, instance, frame, Reliability::Reliable).disconnected_slow_consumers, 1);
        assert!(sink.is_closed());
        assert_eq!(sink.queue().lock().expect("queue").len(), 1);
    }

    #[test]
    fn sync_feature_is_opt_in_and_additive_revision_preserves_legacy_meanings() {
        let mut response = orbisync_protocol::v1::ServerHello::default();
        negotiate_state_sync(&orbisync_protocol::v1::ClientHello::default(), &mut response);
        assert!(response.enabled_features.is_empty());
        let request = orbisync_protocol::v1::ClientHello {
            supported_features: vec![STATE_SYNC_FEATURE.to_owned()], ..Default::default()
        };
        negotiate_state_sync(&request, &mut response);
        assert_eq!(response.enabled_features, vec![STATE_SYNC_FEATURE]);
        let frame = sync_command_frame(InstanceId::generate(), EntityId::generate(), "spawn", u64::MAX, 1);
        let Some(envelope::Payload::EntityCommand(mut command)) = decode_envelope(&frame).expect("decode").payload else { panic!("command"); };
        assert_eq!(command.expected_revision, 1);
        assert_eq!(command.instance_revision, Some(u64::MAX));
        command.instance_revision = None;
        let old_bytes = command.encode_to_vec();
        let old = orbisync_protocol::v1::EntityCommand::decode(old_bytes.as_slice()).expect("old wire");
        assert_eq!(old.instance_revision, None); // No false revision zero for old servers.
        assert_eq!(old.expected_revision, 1);
    }
    #[test]
    fn sync_delta_keeps_commit_payload_after_later_component_update() {
        use orbisync_domain::{Revision, Timestamp, Transform};
        let (mut actor, _, _, user, _) = snapshot_fixture();
        let id = spawn_snapshot_entity(&mut actor, user, 0.0);
        let now = Timestamp::from_unix_millis(5_000).unwrap();
        let CommandOutcome::Applied { revision, entity_revision, committed_entity: Some(committed), .. } = actor.handle(
            InstanceCommand::UpdateTransform { entity_id: id, transform: Transform::identity(),
                expected_revision: Revision::from_u64(1), user_id: user, now, permissions: WorldPermissions::all() }
        ) else { panic!("transform outcome"); };
        assert_eq!(Some(committed.revision()), entity_revision);
        assert!(matches!(actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None, entity_id: id, component_key: "test.note".into(),
            payload_bytes: br#"{"text":"future"}"#.to_vec(), expected_revision: committed.revision(),
            now, requester: user, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { revision: later, .. } if later > revision));
        let encoded = committed_delta_entity(&committed);
        assert_eq!(encoded.revision, committed.revision().as_u64());
        assert!(encoded.properties.unwrap().fields.is_empty());
        assert!(public_component_properties(actor.entities_snapshot().iter().find(|e| e.id() == id).unwrap()).unwrap().fields.contains_key("test.note"));
    }

    #[test]
    fn sync_latest_splits_batched_entities_and_fields_without_losing_other_updates() {
        use orbisync_protocol::v1::{EntityState, StateDelta};
        let mut queue = OutboundQueue::new(8);
        let frame = |revision, entities| Envelope {
            payload: Some(envelope::Payload::StateDelta(StateDelta { from_revision: revision - 1, to_revision: revision, entities })),
            ..Default::default()
        };
        let entity = |id: &str, revision| EntityState {
            entity_id: id.into(), revision, transform: Some(Default::default()), ..Default::default()
        };
        enqueue_committed_delta(&mut queue, frame(102, vec![entity("a", 3)]), 65536).unwrap();
        let mut old_a = entity("a", 2);
        old_a.properties = Some(Default::default());
        enqueue_committed_delta(&mut queue, frame(101, vec![old_a, entity("b", 7)]), 65536).unwrap();
        let entries: Vec<_> = queue.drain_all().iter().map(|bytes| {
            let Some(envelope::Payload::StateDelta(delta)) = decode_envelope(bytes).unwrap().payload else { panic!("delta"); };
            (delta.to_revision, delta.entities[0].clone())
        }).collect();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|(r, e)| *r == 102 && e.entity_id == "a" && e.transform.is_some()));
        assert!(entries.iter().any(|(r, e)| *r == 101 && e.entity_id == "a" && e.properties.is_some()));
        assert!(entries.iter().any(|(r, e)| *r == 101 && e.entity_id == "b"));
        assert_eq!(queue.depth().bytes, 0);
    }

    #[test]
    fn sync_custom_unsafe_integers_remain_original_opaque_bytes() {
        use orbisync_domain::{Revision, Timestamp};
        let (mut actor, _, _, user, _) = snapshot_fixture();
        let id = spawn_snapshot_entity(&mut actor, user, 0.0);
        let original = br#"{"nested":[18446744073709551615,9007199254740993]}"#;
        assert!(matches!(actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None, entity_id: id, component_key: "test.precise".into(),
            payload_bytes: original.to_vec(), expected_revision: Revision::from_u64(1),
            now: Timestamp::from_unix_millis(5_000).unwrap(), requester: user, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        let entities = actor.entities_snapshot();
        let properties = public_component_properties(entities.iter().find(|e| e.id() == id).unwrap()).unwrap();
        let Some(prost_types::value::Kind::StructValue(opaque)) = &properties.fields["test.precise"].kind else { panic!("opaque"); };
        let Some(prost_types::value::Kind::StringValue(data)) = &opaque.fields["value"].kind else { panic!("value"); };
        assert_eq!(base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).unwrap(), original);
        assert!(json_fits_struct(&serde_json::json!({"number": 1.25, "integer": 9007199254740991_u64})));
        assert!(!json_fits_struct(&serde_json::json!({"integer": 9007199254740992_u64})));
    }

    #[test]
    fn independent04_private_delta_queued_during_snapshot_must_not_leak_after_delete() {
        use orbisync_protocol::v1::{EntityState, StateDelta};
        use orbisync_world_runtime::actor::EntityInterestView;
        let (_, sink, instance) = sync_sink(8);
        let id = EntityId::generate();
        let private_view = EntityInterestView { id, owner: Some(UserId::generate()),
            position: Some(Vec3::new(0.0, 0.0, 0.0).unwrap()),
            visibility: orbisync_domain::VisibilityPolicy::OwnerOnly };
        let frame = Envelope { instance_id: instance.to_string(), message_id: "private-delta".into(),
            payload: Some(envelope::Payload::StateDelta(StateDelta { from_revision: 100, to_revision: 101,
                entities: vec![EntityState { entity_id: id.to_string(), revision: 2,
                    transform: Some(orbisync_protocol::v1::Transform { rotation_w: 1.0, ..Default::default() }),
                    properties: Some(prost_types::Struct { fields: std::collections::BTreeMap::from([
                        ("note.secret".into(), prost_types::Value { kind: Some(prost_types::value::Kind::StringValue("private-value".into())) })
                    ]) }), ..Default::default() }] })), ..Default::default() }.encode_to_vec();
        // Owner-only is rejected while the authoritative view exists.
        assert!(filter_payload_for_viewer_with_roles(&frame, Some(Vec3::new(0.0,0.0,0.0).unwrap()),
            Some(sink.filter.lock().unwrap().user), &sink.state.interest_grid,
            std::slice::from_ref(&private_view), None, &mut std::collections::HashMap::new()).is_none());
        // During handoff enqueue deliberately retains raw bytes, including invisible updates.
        let views = std::iter::once(private_view).collect();
        assert_eq!(sink.enqueue(&frame, Reliability::LatestWins, Some(&views)).delivered, 1);
        sink.install_snapshot(100, std::collections::HashMap::new());
        // Entity has since been deleted: current registry has no interest view.
        let queued = sink.queue().lock().unwrap().drain_all();
        assert_eq!(queued.len(), 1);
        assert!(sink.prepare_delivery(&queued[0]).is_none(), "owner-only Delta must not fall back to spatial visibility after deletion");
    }

    #[tokio::test]
    async fn independent04_actual_actor_delete_preserves_owner_only_handoff_privacy() {
        use orbisync_domain::{EntityKind, Revision, Timestamp, Transform, VisibilityPolicy};
        use orbisync_world_runtime::actor::EntityInterestView;
        let (mut actor, instance, _, owner, _) = snapshot_fixture();
        let id = EntityId::generate();
        assert!(matches!(actor.handle(InstanceCommand::SpawnEntity {
            command_id: None, entity_id: id, kind: EntityKind::Object, owner: Some(owner),
            transform: Some(Transform::identity()), visibility: VisibilityPolicy::OwnerOnly,
            requester: owner, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        assert!(matches!(actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None, entity_id: id, component_key: "note.secret".into(),
            payload_bytes: br#"{"text":"private"}"#.to_vec(), expected_revision: Revision::from_u64(1),
            now: Timestamp::from_unix_millis(5000).unwrap(), requester: owner, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        let CommandOutcome::Applied { revision, committed_entity: Some(entity), .. } = actor.handle(InstanceCommand::UpdateTransform {
            entity_id: id, transform: Transform::identity(), expected_revision: Revision::from_u64(2),
            user_id: owner, now: Timestamp::from_unix_millis(6000).unwrap(), permissions: WorldPermissions::all(),
        }) else { panic!("transform"); };
        let registry = Arc::new(RuntimeRegistry::new());
        registry.insert(instance.id(), actor).unwrap();
        let state = Arc::new(RealtimeState::new(orbisync_config::Config::default().realtime, registry,
            Arc::new(DeliveryRegistry::new()), Arc::new(orbisync_domain::SystemClock::new())));
        let sink = RealtimeDeliverySink::new(Arc::clone(&state), instance.id(),
            Vec3::new(0.0,0.0,0.0).unwrap(), UserId::generate(), None, std::collections::HashMap::new());
        sink.begin_snapshot();
        sink.install_snapshot(revision.as_u64()-1, std::collections::HashMap::new());
        let frame = Envelope { instance_id: instance.id().to_string(), message_id: "actor-private".into(),
            payload: Some(envelope::Payload::StateDelta(orbisync_protocol::v1::StateDelta {
                from_revision: revision.as_u64()-1, to_revision: revision.as_u64(),
                entities: vec![committed_delta_entity(&entity)],
            })), ..Default::default() }.encode_to_vec();
        let views = std::iter::once(EntityInterestView::from_entity(&entity)).collect();
        assert_eq!(sink.enqueue(&frame, Reliability::LatestWins, Some(&views)).delivered,1);
        assert!(matches!(state.registry.submit(instance.id(), InstanceCommand::DeleteEntity {
            command_id: None, entity_id:id, expected_revision:entity.revision(), requester:owner, permissions:WorldPermissions::all(),
        }).await.unwrap(), CommandOutcome::Applied { .. }));
        assert!(!state.registry.interest_views(instance.id()).unwrap().iter().any(|view|view.id==id));
        let queued=sink.queue().lock().unwrap().drain_all();
        assert!(sink.prepare_delivery(&queued[0]).is_none(),"actual deleted owner-only entity leaked its committed properties");
    }


    // Test publishers must supply the same immutable view as production publishers.
    fn sync_test_broadcast(state: &RealtimeState, instance: InstanceId, bytes: Vec<u8>, reliability: Reliability) -> crate::delivery::BroadcastOutcome {
        let env = decode_envelope(&bytes).unwrap();
        let (ids, private) = match &env.payload {
            Some(envelope::Payload::EntityCommand(c)) => (vec![c.entity_id.clone()],
                c.arguments.as_ref().and_then(|a| extract_string_field(a, "visibility")).as_deref() == Some("owner_only")),
            Some(envelope::Payload::StateDelta(d)) => (d.entities.iter().map(|e|e.entity_id.clone()).collect(), false),
            _ => (Vec::new(), false),
        };
        let views = ids.into_iter().map(|id| orbisync_world_runtime::actor::EntityInterestView {
            id: id.parse().unwrap(), owner: None, position: None,
            visibility: if private { VisibilityPolicy::OwnerOnly } else { VisibilityPolicy::Global },
        }).collect();
        state.delivery.broadcast_with_views(instance, bytes, reliability, Arc::new(views), state.interest_grid)
    }

    #[test]
    fn sync_publication_visibility_preserves_owner_public_and_delete_progress() {
        for private in [false, true] {
            for owner in [false, true] {
                for operation in ["spawn", "update", "delete", "delta"] {
                    let (_, sink, instance) = sync_sink(8);
                    let id = EntityId::generate();
                    let user = sink.filter.lock().unwrap().user;
                    let view = orbisync_world_runtime::actor::EntityInterestView {
                        id, owner: Some(if owner { user } else { UserId::generate() }), position: None,
                        visibility: if private { VisibilityPolicy::OwnerOnly } else { VisibilityPolicy::Global },
                    };
                    let bytes = if operation == "delta" {
                        Envelope { message_id: "delta".into(), payload: Some(envelope::Payload::StateDelta(orbisync_protocol::v1::StateDelta {
                            from_revision: 10, to_revision: 11, entities: vec![orbisync_protocol::v1::EntityState {
                                entity_id: id.to_string(), revision: 2, properties: Some(Default::default()), ..Default::default()
                            }],
                        })), ..Default::default() }.encode_to_vec()
                    } else { sync_command_frame(instance, id, operation, 11, 2) };
                    let views = std::iter::once(view).collect();
                    assert_eq!(sink.enqueue(&bytes, Reliability::Reliable, Some(&views)).delivered, 1);
                    sink.install_snapshot(10, Default::default());
                    let queued = sink.queue().lock().unwrap().drain_all();
                    assert_eq!(sink.prepare_delivery(&queued[0]).is_some(), !private || owner, "{operation}");
                    assert!(!sink.is_closed());
                    assert!(sink.filter.lock().unwrap().raw_snapshot_messages.is_empty());
                }
            }
        }
    }

    #[test]
    fn sync_missing_publication_visibility_disconnects_for_recovery() {
        for handoff in [false, true] {
            let (_, sink, instance) = sync_sink(8);
            if !handoff { sink.finish_snapshot(); }
            let bytes = sync_command_frame(instance, EntityId::generate(), "spawn", 11, 1);
            assert_eq!(sink.enqueue(&bytes, Reliability::Reliable, None).disconnected_slow_consumer, 1);
            assert!(sink.is_closed());
            assert!(sink.prepare_delivery(&bytes).is_none());
        }
    }

    #[tokio::test]
    async fn sync_actual_actor_recreation_cannot_publicize_old_private_delta() {
        use orbisync_domain::{EntityKind, Revision, Timestamp, Transform, VisibilityPolicy};
        use orbisync_world_runtime::actor::EntityInterestView;
        let (mut actor, instance, _, owner, _) = snapshot_fixture();
        let id = EntityId::generate();
        assert!(matches!(actor.handle(InstanceCommand::SpawnEntity {
            command_id: None, entity_id: id, kind: EntityKind::Object, owner: Some(owner),
            transform: Some(Transform::identity()), visibility: VisibilityPolicy::OwnerOnly,
            requester: owner, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        assert!(matches!(actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None, entity_id: id, component_key: "note.secret".into(),
            payload_bytes: br#"{"text":"private"}"#.to_vec(), expected_revision: Revision::from_u64(1),
            now: Timestamp::from_unix_millis(5000).unwrap(), requester: owner, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        let CommandOutcome::Applied { revision, committed_entity: Some(entity), .. } = actor.handle(InstanceCommand::UpdateTransform {
            entity_id: id, transform: Transform::identity(), expected_revision: Revision::from_u64(2),
            user_id: owner, now: Timestamp::from_unix_millis(6000).unwrap(), permissions: WorldPermissions::all(),
        }) else { panic!("transform"); };
        let registry = Arc::new(RuntimeRegistry::new());
        registry.insert(instance.id(), actor).unwrap();
        let state = Arc::new(RealtimeState::new(orbisync_config::Config::default().realtime, registry,
            Arc::new(DeliveryRegistry::new()), Arc::new(orbisync_domain::SystemClock::new())));
        let sink = RealtimeDeliverySink::new(Arc::clone(&state), instance.id(),
            Vec3::new(0.0,0.0,0.0).unwrap(), UserId::generate(), None, std::collections::HashMap::new());
        sink.begin_snapshot();
        sink.install_snapshot(revision.as_u64()-1, std::collections::HashMap::new());
        let frame = Envelope { instance_id: instance.id().to_string(), message_id: "actor-private".into(),
            payload: Some(envelope::Payload::StateDelta(orbisync_protocol::v1::StateDelta {
                from_revision: revision.as_u64()-1, to_revision: revision.as_u64(),
                entities: vec![committed_delta_entity(&entity)],
            })), ..Default::default() }.encode_to_vec();
        let views = std::iter::once(EntityInterestView::from_entity(&entity)).collect();
        assert_eq!(sink.enqueue(&frame, Reliability::LatestWins, Some(&views)).delivered,1);
        assert!(matches!(state.registry.submit(instance.id(), InstanceCommand::DeleteEntity {
            command_id: None, entity_id:id, expected_revision:entity.revision(), requester:owner, permissions:WorldPermissions::all(),
        }).await.unwrap(), CommandOutcome::Applied { .. }));
        assert!(!state.registry.interest_views(instance.id()).unwrap().iter().any(|view|view.id==id));
        assert!(matches!(state.registry.submit(instance.id(), InstanceCommand::SpawnEntity {
            command_id: None, entity_id: id, kind: EntityKind::Object, owner: Some(owner),
            transform: Some(Transform::identity()), visibility: VisibilityPolicy::Global,
            requester: owner, permissions: WorldPermissions::all(),
        }).await.unwrap(), CommandOutcome::Applied { .. }));
        assert!(matches!(state.registry.interest_views(instance.id()).unwrap().iter().find(|v|v.id==id).unwrap().visibility, VisibilityPolicy::Global));
        let queued=sink.queue().lock().unwrap().drain_all();
        assert!(sink.prepare_delivery(&queued[0]).is_none(),"actual deleted owner-only entity leaked its committed properties");
    }

    #[tokio::test]
    async fn sync_recovered_publications_preserve_reliable_boundaries_when_reversed() {
        let (state, sink, instance) = sync_sink(8);
        sink.finish_snapshot();
        let _registration = state.delivery.register_sink(instance, sink.clone() as Arc<dyn DeliverySink>);
        let id = EntityId::generate();
        for boundary in [12, 11] {
            let frame = decode_envelope(&sync_command_frame(instance, id, "update", boundary, boundary-9)).unwrap();
            let Some(envelope::Payload::EntityCommand(command)) = frame.payload else { panic!("command"); };
            CommandPublication { negotiated_minor: 0, revision: boundary, views: Arc::new(std::iter::once(orbisync_world_runtime::actor::EntityInterestView {
                id, owner: None, position: None, visibility: VisibilityPolicy::Global,
            }).collect()) }.publish(&state, instance, &crate::command_dedup::CommandDedupResult::Applied { command, message_id: frame.message_id }).await;
        }
        let frames = sink.queue().lock().unwrap().drain_all();
        let revisions: Vec<_> = frames.iter().map(|bytes| {
            let Some(envelope::Payload::EntityCommand(command)) = sink.prepare_delivery(bytes).unwrap().payload else { panic!("command"); };
            command.instance_revision.unwrap()
        }).collect();
        assert_eq!(revisions, vec![12,11], "reliable lifecycle commands must not coalesce or rewrite older commit boundaries");
    }
