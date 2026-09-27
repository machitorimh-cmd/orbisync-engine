    fn snapshot_fixture() -> (
        InstanceActor,
        orbisync_domain::WorldInstance,
        orbisync_domain::World,
        UserId,
        PresenceId,
    ) {
        use orbisync_domain::{InstanceId, Timestamp, Transform, World, WorldInstance};
        use orbisync_world_runtime::{
            InstanceRuntimeDescriptor, RuntimeState, command::InstanceCommand,
        };

        let instance_id = InstanceId::generate();
        let world_id = orbisync_domain::WorldId::generate();
        let now = Timestamp::from_unix_millis(1_000).expect("timestamp");
        let world = World::new(
            world_id,
            "public-world",
            None,
            Transform::identity(),
            100,
            now,
        )
        .expect("world");
        let instance = WorldInstance::new(instance_id, world_id, 100, now).expect("instance");
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id,
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        let user = UserId::generate();
        let presence = PresenceId::generate();
        assert!(matches!(
            actor.handle(InstanceCommand::Join {
                presence_id: presence,
                user_id: user,
                instance_id,
                capacity: 100,
            }),
            CommandOutcome::Applied { .. }
        ));
        (actor, instance, world, user, presence)
    }

    fn snapshot_json(
        actor: &InstanceActor,
        instance: &orbisync_domain::WorldInstance,
        world: &orbisync_domain::World,
        user: UserId,
        viewer_pos: Vec3,
        grid: orbisync_interest::UniformGrid,
        permissions: WorldPermissions,
    ) -> serde_json::Value {
        snapshot_json_with_metrics(
            actor,
            instance,
            world,
            user,
            viewer_pos,
            grid,
            permissions,
            &NoopMetrics,
        )
    }

    fn snapshot_json_with_metrics(
        actor: &InstanceActor,
        instance: &orbisync_domain::WorldInstance,
        world: &orbisync_domain::World,
        user: UserId,
        viewer_pos: Vec3,
        grid: orbisync_interest::UniformGrid,
        permissions: WorldPermissions,
        metrics: &dyn MetricsRecorder,
    ) -> serde_json::Value {
        let mut subscribed = std::collections::HashMap::new();
        let data = filtered_snapshot_for_viewer(
            actor,
            Some(viewer_pos),
            Some(user),
            &grid,
            &mut subscribed,
            SnapshotContext {
                instance,
                world,
                metrics,
                permissions,
                server_time_unix_ms: 2_000,
            },
        );
        serde_json::from_slice(&data).expect("snapshot JSON")
    }

    fn spawn_snapshot_entity(actor: &mut InstanceActor, owner: UserId, x: f64) -> EntityId {
        use orbisync_domain::{EntityKind, Quaternion, Transform, VisibilityPolicy};
        use orbisync_world_runtime::command::InstanceCommand;

        let entity_id = EntityId::generate();
        let transform = Transform::new(
            Vec3::new(x, 0.0, 0.0).expect("position"),
            Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("rotation"),
            Vec3::new(1.0, 1.0, 1.0).expect("scale"),
        )
        .expect("transform");
        assert!(matches!(
            actor.handle(InstanceCommand::SpawnEntity {
                command_id: None,
                entity_id,
                kind: EntityKind::Object,
                owner: Some(owner),
                transform: Some(transform),
                visibility: VisibilityPolicy::spatial(100.0).expect("visibility"),
                requester: owner,
                permissions: WorldPermissions::all(),
            }),
            CommandOutcome::Applied { .. }
        ));
        entity_id
    }

    #[test]
    fn snapshot_data_contains_all_join_state_without_credentials() {
        let (actor, instance, world, user, presence) = snapshot_fixture();
        let value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions {
                entity_spawn: true,
                entity_update_own: false,
                entity_update_any: true,
            },
        );

        // (a) All seven required Snapshot sections are present and populated.
        for key in [
            "user",
            "instance",
            "permissions",
            "entities",
            "users",
            "revision",
            "server_time_unix_ms",
        ] {
            assert!(value.get(key).is_some(), "missing snapshot field {key}");
        }
        assert_eq!(value["user"]["user_id"], user.to_string());
        assert_eq!(value["users"][0]["presence_id"], presence.to_string());
        assert_eq!(value["instance"]["instance_id"], instance.id().to_string());
        assert_eq!(value["instance"]["world_id"], world.id().to_string());
        assert_eq!(value["permissions"]["entity_spawn"], true);
        assert_eq!(value["permissions"]["entity_update_own"], false);
        assert_eq!(value["permissions"]["entity_update_any"], true);
        assert_eq!(value["revision"], actor.revision().as_u64());
        assert_eq!(value["server_time_unix_ms"], 2_000);

        // (f) Snapshot bytes contain no credential-shaped data.
        let text = serde_json::to_string(&value)
            .expect("snapshot JSON text")
            .to_lowercase();
        for forbidden in ["password", "token", "hash", "login_id", "email"] {
            assert!(
                !text.contains(forbidden),
                "credential field leaked: {forbidden}"
            );
        }
    }

    #[test]
    fn snapshot_restores_public_whiteboard_component_properties() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        let entity_id = spawn_snapshot_entity(&mut actor, user, 10.0);
        let now = orbisync_domain::Timestamp::from_unix_millis(2_000).expect("timestamp");
        assert!(matches!(
            actor.handle(InstanceCommand::UpdateEntityComponent {
                command_id: None,
                entity_id,
                component_key: "com.orbisync.whiteboard.note".to_owned(),
                payload_bytes: br#"{"text":"hello","color":"mint","locked":false}"#.to_vec(),
                expected_revision: orbisync_domain::Revision::from_u64(1),
                now,
                requester: user,
                permissions: WorldPermissions::all(),
            }),
            CommandOutcome::Applied { .. }
        ));

        let value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
        );
        let entity = value["entities"]
            .as_array()
            .expect("entities")
            .iter()
            .find(|entity| entity["entity_id"] == entity_id.to_string())
            .expect("whiteboard entity");
        assert_eq!(
            entity["properties"]["com.orbisync.whiteboard.note"]["value"]["text"],
            "hello"
        );
        assert_eq!(
            entity["properties"]["com.orbisync.whiteboard.note"]["value"]["color"],
            "mint"
        );
        assert_eq!(
            entity["properties"]["com.orbisync.whiteboard.note"]["value"]["locked"],
            false
        );
    }

    #[test]
    fn snapshot_keeps_other_public_namespace_and_hides_internal_component() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        let entity_id = spawn_snapshot_entity(&mut actor, user, 10.0);
        let now = orbisync_domain::Timestamp::from_unix_millis(2_000).expect("timestamp");
        for (revision, key, payload) in [
            (1, "com.example.widget", br#"{"label":"other-app"}"#.to_vec()),
            (2, "org.school.example.board", br#"{"label":"other-app"}"#.to_vec()),
            (3, "org.school.example.opaque", vec![0, 255]),
        ] {
            assert!(matches!(
                actor.handle(InstanceCommand::UpdateEntityComponent {
                    command_id: None,
                    entity_id,
                    component_key: key.to_owned(),
                    payload_bytes: payload,
                    expected_revision: orbisync_domain::Revision::from_u64(revision),
                    now,
                    requester: user,
                    permissions: WorldPermissions::all(),
                }),
                CommandOutcome::Applied { .. }
            ));
        }
        let value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
        );
        let entity = value["entities"]
            .as_array()
            .expect("entities")
            .iter()
            .find(|entity| entity["entity_id"] == entity_id.to_string())
            .expect("entity");
        assert_eq!(
            entity["properties"]["com.example.widget"]["value"]["label"],
            "other-app"
        );
        assert_eq!(
            entity["properties"]["org.school.example.board"]["value"]["label"],
            "other-app"
        );
        assert_eq!(
            entity["properties"]["com.example.widget"]["encoding"],
            "json"
        );
        assert_eq!(
            entity["properties"]["org.school.example.opaque"]["encoding"],
            "base64"
        );
        assert_eq!(
            entity["properties"]["org.school.example.opaque"]["value"],
            "AP8="
        );
        assert!(entity["properties"].get("core.transform").is_none());
    }

    #[test]
    fn snapshot_includes_component_bytes_for_visible_entities_only() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        let near = spawn_snapshot_entity(&mut actor, user, 1.0);
        let far = spawn_snapshot_entity(&mut actor, user, 100.0);
        for id in [near, far] {
            let result = actor.handle(InstanceCommand::UpdateEntityComponent {
                command_id: None, entity_id: id, component_key: "custom.chat".to_owned(),
                payload_bytes: vec![0, 128, 255], expected_revision: orbisync_domain::Revision::from_u64(1),
                now: orbisync_domain::Timestamp::from_unix_millis(1000).unwrap(),
                requester: user, permissions: WorldPermissions::all(),
            });
            assert!(matches!(result, CommandOutcome::Applied { .. }), "{result:?}");
        }
        let data = snapshot_json(&actor, &instance, &world, user,
            Vec3::new(0.0, 0.0, 0.0).unwrap(), orbisync_interest::UniformGrid::default(), WorldPermissions::all());
        let entities = data["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["entity_id"], near.to_string());
        assert_eq!(entities[0]["components"]["custom.chat"], serde_json::json!([0, 128, 255]));
        assert_eq!(entities[0]["owner_user_id"], user.to_string());
    }

    #[test]
    fn snapshot_excludes_entities_outside_joining_subject_interest() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        let near_id = spawn_snapshot_entity(&mut actor, UserId::generate(), 10.0);
        let far_id = spawn_snapshot_entity(&mut actor, UserId::generate(), 100.0);
        let value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
        );
        let ids = value["entities"]
            .as_array()
            .expect("entities array")
            .iter()
            .map(|entity| entity["entity_id"].as_str().expect("entity id"))
            .collect::<Vec<_>>();
        let near_id = near_id.to_string();
        let far_id = far_id.to_string();
        assert!(ids.contains(&near_id.as_str()));
        assert!(!ids.contains(&far_id.as_str()));
    }

    #[test]
    fn visible_set_size_metric_counts_near_entities() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        for x in [5.0, 10.0, 15.0] {
            spawn_snapshot_entity(&mut actor, UserId::generate(), x);
        }
        let metrics = PrometheusMetrics::new();
        let _value = snapshot_json_with_metrics(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
            &metrics,
        );
        let text = metrics.render();
        assert!(text.contains("interest_visible_set_size_bucket{le=\"5.0\"} 1"));
        assert!(text.contains("interest_visible_set_size_sum 3.0"));
        assert!(text.contains("interest_visible_set_size_count 1"));
    }

    #[test]
    fn visible_set_size_metric_records_zero_for_far_entities() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        spawn_snapshot_entity(&mut actor, UserId::generate(), 100.0);
        let metrics = PrometheusMetrics::new();
        let _value = snapshot_json_with_metrics(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
            &metrics,
        );
        let text = metrics.render();
        assert!(text.contains("interest_visible_set_size_bucket{le=\"0.0\"} 1"));
        assert!(text.contains("interest_visible_set_size_sum 0.0"));
        assert!(text.contains("interest_visible_set_size_count 1"));
    }

    #[test]
    fn snapshot_interest_near_radius_changes_visible_entities() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        let changing_id = spawn_snapshot_entity(&mut actor, UserId::generate(), 20.0);
        let wide = orbisync_interest::UniformGrid::default();
        let narrow = orbisync_interest::UniformGrid::new(
            orbisync_interest::InterestParameters::new(10.0, 15.0, 20.0)
                .expect("interest parameters"),
        );
        let wide_value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            wide,
            WorldPermissions::all(),
        );
        let narrow_value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            narrow,
            WorldPermissions::all(),
        );
        let wide_ids = wide_value["entities"].to_string();
        let narrow_ids = narrow_value["entities"].to_string();
        assert!(wide_ids.contains(&changing_id.to_string()));
        assert!(!narrow_ids.contains(&changing_id.to_string()));
        assert_ne!(wide_ids, narrow_ids);
    }

    #[test]
    fn snapshot_presence_list_contains_only_opaque_presence_ids() {
        let (mut actor, instance, world, user, presence) = snapshot_fixture();
        let other_user = UserId::generate();
        let other_presence = PresenceId::generate();
        assert!(matches!(
            actor.handle(InstanceCommand::Join {
                presence_id: other_presence,
                user_id: other_user,
                instance_id: instance.id(),
                capacity: instance.capacity(),
            }),
            CommandOutcome::Applied { .. }
        ));
        let value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
        );
        let users = value["users"].as_array().expect("users array");
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["presence_id"], presence.to_string());
        assert!(!users.iter().any(|entry| {
            entry["presence_id"] == other_presence.to_string()
        }));
        for entry in users {
            assert_eq!(entry.as_object().expect("presence object").len(), 1);
            assert!(entry.get("user_id").is_none());
            assert!(entry.get("display_name").is_none());
        }
    }

    #[test]
    fn join_accepted_state_filters_entities_and_keeps_presence_user_pairs() {
        let (mut actor, instance, _world, user, _) = snapshot_fixture();
        let other_user = UserId::generate();
        let other_presence = PresenceId::generate();
        assert!(matches!(
            actor.handle(InstanceCommand::Join {
                presence_id: other_presence,
                user_id: other_user,
                instance_id: instance.id(),
                capacity: instance.capacity(),
            }),
            CommandOutcome::Applied { .. }
        ));
        let hidden_user = UserId::generate();
        let hidden_presence = PresenceId::generate();
        assert!(matches!(
            actor.handle(InstanceCommand::Join {
                presence_id: hidden_presence,
                user_id: hidden_user,
                instance_id: instance.id(),
                capacity: instance.capacity(),
            }),
            CommandOutcome::Applied { .. }
        ));
        let near_id = spawn_snapshot_entity(&mut actor, other_user, 10.0);
        let far_id = spawn_snapshot_entity(&mut actor, UserId::generate(), 100.0);
        let read = orbisync_world_runtime::InstanceReadSnapshot {
            interest_views: actor.interest_views(),
            entities: actor.entities_snapshot(),
            members: actor.members_snapshot(),
            revision: actor.revision(),
            oldest_retained_revision: actor.oldest_retained_revision(),
            reliable_floor: actor.reliable_floor(),
            reliable_events: actor.reliable_events(),
        };
        let (entities, presence_ids, user_ids) = filtered_join_accepted_state(
            &read,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            user,
            &orbisync_interest::UniformGrid::default(),
            &mut std::collections::HashMap::new(),
            None,
        );
        let entity_ids = entities
            .iter()
            .map(|entity| entity.entity_id.as_str())
            .collect::<Vec<_>>();
        assert!(entity_ids.contains(&near_id.to_string().as_str()));
        assert!(!entity_ids.contains(&far_id.to_string().as_str()));
        assert_eq!(presence_ids.len(), user_ids.len());
        let pair_index = presence_ids
            .iter()
            .position(|presence| presence == &other_presence.to_string())
            .expect("other presence visible");
        assert_eq!(user_ids[pair_index], other_user.to_string());
        assert!(!presence_ids.contains(&hidden_presence.to_string()));
        assert!(!user_ids.contains(&hidden_user.to_string()));
    }

    #[test]
    fn snapshot_hides_presence_for_owned_entity_outside_interest() {
        let (mut actor, instance, world, user, _) = snapshot_fixture();
        let hidden_user = UserId::generate();
        let hidden_presence = PresenceId::generate();
        assert!(matches!(
            actor.handle(InstanceCommand::Join {
                presence_id: hidden_presence,
                user_id: hidden_user,
                instance_id: instance.id(),
                capacity: instance.capacity(),
            }),
            CommandOutcome::Applied { .. }
        ));

        let value = snapshot_json(
            &actor,
            &instance,
            &world,
            user,
            Vec3::new(0.0, 0.0, 0.0).expect("position"),
            orbisync_interest::UniformGrid::default(),
            WorldPermissions::all(),
        );
        let users = value["users"].as_array().expect("users array");
        let hidden_user_string = hidden_user.to_string();
        assert!(!users.iter().any(|entry| {
            entry["presence_id"] == hidden_presence.to_string()
                || entry
                    .get("user_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(hidden_user_string.as_str())
        }));
    }

    #[test]
    fn snapshot_chunks_have_shared_id_and_ordered_metadata() {
        let data = vec![b'x'; SNAPSHOT_CHUNK_BYTES * 2 + 7];
        let snapshots = snapshot_payloads("snapshot-test", &data, 42);
        assert_eq!(snapshots.len(), 3);
        assert!(
            snapshots
                .iter()
                .all(|snapshot| snapshot.data.len() <= SNAPSHOT_CHUNK_BYTES)
        );
        assert!(
            snapshots
                .iter()
                .all(|snapshot| snapshot.snapshot_id == "snapshot-test")
        );
        assert!(snapshots.iter().all(|snapshot| snapshot.chunk_count == 3));
        assert_eq!(
            snapshots
                .iter()
                .map(|snapshot| snapshot.chunk_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(
            snapshots
                .iter()
                .all(|snapshot| snapshot.instance_revision == 42)
        );
        assert_eq!(
            snapshots
                .iter()
                .flat_map(|snapshot| snapshot.data.clone())
                .collect::<Vec<_>>(),
            data
        );
        let empty = snapshot_payloads("empty", &[], 0);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].chunk_index, 0);
        assert_eq!(empty[0].chunk_count, 1);
    }

    // N-3 hysteresis wiring tests (W-6). These call the production helper
    // `filter_payload_for_viewer` with a real `subscribed` map; the tests go
    // red if the wiring is removed (passing None) or if `should_retain`
    // incorrectly uses 30 m for both branches. After N-5 the helper takes
    // `&[EntityInterestView]` instead of `&[Entity]` so we build lightweight
    // views (no components clone).
    fn n3_make_view(
        id: orbisync_domain::EntityId,
        x: f64,
    ) -> orbisync_world_runtime::actor::EntityInterestView {
        let pos = orbisync_domain::Vec3::new(x, 0.0, 0.0).expect("vec3");
        orbisync_world_runtime::actor::EntityInterestView {
            id,
            owner: None,
            position: Some(pos),
            visibility: orbisync_domain::VisibilityPolicy::spatial(100.0).expect("policy"),
        }
    }

    #[allow(dead_code)]
    fn n3_make_entity(
        id: orbisync_domain::EntityId,
        instance: orbisync_domain::InstanceId,
        x: f64,
    ) -> orbisync_domain::Entity {
        let pos = orbisync_domain::Vec3::new(x, 0.0, 0.0).expect("vec3");
        let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("quat");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("vec3");
        let t = orbisync_domain::Transform::new(pos, rot, scale).expect("transform");
        let vis = orbisync_domain::VisibilityPolicy::spatial(100.0).expect("policy");
        let ts = orbisync_domain::Timestamp::from_unix_millis(0).expect("ts");
        orbisync_domain::Entity::new(
            id,
            instance,
            orbisync_domain::EntityKind::Object,
            None,
            Some(t),
            vis,
            ts,
        )
    }

    fn n3_make_payload(entity_id: orbisync_domain::EntityId, revision: u64, x: f64) -> Vec<u8> {
        let transform = orbisync_protocol::v1::Transform {
            position_x: x as f32,
            position_y: 0.0,
            position_z: 0.0,
            rotation_x: 0.0,
            rotation_y: 0.0,
            rotation_z: 0.0,
            rotation_w: 1.0,
        };
        let es = orbisync_protocol::v1::EntityState {
            entity_id: entity_id.to_string(),
            revision,
            transform: Some(transform),
            properties: None,
            velocity: None,
            animation: None,
            presence: None,
        };
        let delta = orbisync_protocol::v1::StateDelta {
            from_revision: revision.saturating_sub(1),
            to_revision: revision,
            entities: vec![es],
        };
        let env = Envelope {
            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
            protocol_minor: 0,
            message_id: uuid::Uuid::now_v7().to_string(),
            sequence: 0,
            sent_at_unix_ms: 0,
            instance_id: String::new(),
            payload: Some(envelope::Payload::StateDelta(delta)),
        };
        let mut buf = Vec::new();
        env.encode(&mut buf).expect("encode");
        buf
    }

    #[test]
    fn n3_hysteresis_retains_at_31_after_29() {
        // Breaking `subscribed` wiring (filter_payload_for_viewer called with None or
        // not updating the map) makes the 31 m check red. Also breaking
        // `UniformGrid::should_retain` to always use near_radius makes 31 m culled.
        let grid = orbisync_interest::UniformGrid::default(); // 30 / 35
        let eid = orbisync_domain::EntityId::generate();
        let viewer = Some(orbisync_domain::Vec3::new(0.0, 0.0, 0.0).expect("vec"));
        let mut subscribed = std::collections::HashMap::new();

        // 29 m -> visible, subscribes
        let v29 = n3_make_view(eid, 29.0);
        let p29 = n3_make_payload(eid, 1, 29.0);
        let out29 = filter_payload_for_viewer(
            &p29,
            viewer,
            None,
            &grid,
            std::slice::from_ref(&v29),
            &mut subscribed,
        );
        assert!(
            out29.is_some(),
            "29 m must be visible and create subscription"
        );
        assert_eq!(subscribed.get(&eid), Some(&true));

        // 31 m -> still visible due to hysteresis (subscribed=true allows up to 35)
        // Without hysteresis (always false) this would be None.
        let v31 = n3_make_view(eid, 31.0);
        let p31 = n3_make_payload(eid, 2, 31.0);
        let out31 = filter_payload_for_viewer(
            &p31,
            viewer,
            None,
            &grid,
            std::slice::from_ref(&v31),
            &mut subscribed,
        );
        assert!(
            out31.is_some(),
            "31 m must remain visible due to hysteresis (was subscribed at 29)"
        );
        assert_eq!(subscribed.get(&eid), Some(&true));
    }

    #[test]
    fn n3_hysteresis_unsubscribes_at_36() {
        // Breaking `should_retain` unsubscribe threshold (e.g. 30 instead of 35)
        // or failing to set subscribed=false makes this red.
        let grid = orbisync_interest::UniformGrid::default();
        let eid = orbisync_domain::EntityId::generate();
        let viewer = Some(orbisync_domain::Vec3::new(0.0, 0.0, 0.0).expect("vec"));
        let mut subscribed = std::collections::HashMap::new();

        let v29 = n3_make_view(eid, 29.0);
        let p29 = n3_make_payload(eid, 1, 29.0);
        assert!(
            filter_payload_for_viewer(
                &p29,
                viewer,
                None,
                &grid,
                std::slice::from_ref(&v29),
                &mut subscribed
            )
            .is_some()
        );

        // 36 m -> beyond unsubscribe (35) -> culled and subscription cleared
        let v36 = n3_make_view(eid, 36.0);
        let p36 = n3_make_payload(eid, 2, 36.0);
        let out36 = filter_payload_for_viewer(
            &p36,
            viewer,
            None,
            &grid,
            std::slice::from_ref(&v36),
            &mut subscribed,
        );
        assert!(
            out36.is_none(),
            "36 m must be culled (beyond 35) and unsubscribe"
        );
        assert_eq!(subscribed.get(&eid), Some(&false));
    }

    #[test]
    fn n3_hysteresis_34_stays_culled_after_unsubscribe() {
        // After unsubscribe at 36, 34 m must stay culled (needs re-enter to 30).
        // If wiring forgets to persist subscribed=false, 34 would incorrectly become visible.
        let grid = orbisync_interest::UniformGrid::default();
        let eid = orbisync_domain::EntityId::generate();
        let viewer = Some(orbisync_domain::Vec3::new(0.0, 0.0, 0.0).expect("vec"));
        let mut subscribed = std::collections::HashMap::new();

        let v29 = n3_make_view(eid, 29.0);
        let p29 = n3_make_payload(eid, 1, 29.0);
        assert!(
            filter_payload_for_viewer(
                &p29,
                viewer,
                None,
                &grid,
                std::slice::from_ref(&v29),
                &mut subscribed
            )
            .is_some()
        );

        let v36 = n3_make_view(eid, 36.0);
        let p36 = n3_make_payload(eid, 2, 36.0);
        assert!(
            filter_payload_for_viewer(
                &p36,
                viewer,
                None,
                &grid,
                std::slice::from_ref(&v36),
                &mut subscribed
            )
            .is_none()
        );

        // 34 m -> still culled because we are unsubscribed (false) and 34 >30
        let v34 = n3_make_view(eid, 34.0);
        let p34 = n3_make_payload(eid, 3, 34.0);
        let out34 = filter_payload_for_viewer(
            &p34,
            viewer,
            None,
            &grid,
            std::slice::from_ref(&v34),
            &mut subscribed,
        );
        assert!(
            out34.is_none(),
            "34 m after 36 unsubscribe must stay culled (needs 30 to resubscribe)"
        );
        assert_eq!(subscribed.get(&eid), Some(&false));

        // Re-enter at 29 should resubscribe
        let v29b = n3_make_view(eid, 29.0);
        let p29b = n3_make_payload(eid, 4, 29.0);
        let out29b = filter_payload_for_viewer(
            &p29b,
            viewer,
            None,
            &grid,
            std::slice::from_ref(&v29b),
            &mut subscribed,
        );
        assert!(out29b.is_some(), "29 m re-enter must resubscribe");
        assert_eq!(subscribed.get(&eid), Some(&true));
    }

    #[test]
    fn n3_subscribed_prunes_stale_entries() {
        // Verifies W-6 step 4: entries for entities not in snapshot are purged.
        // Without the retain, the map grows unbounded for long-lived connections.
        // Breaking the retain (removing the line) keeps the stale entry and fails this test.
        // N-5: retain now uses HashSet (O(S) not O(S*N)).
        let grid = orbisync_interest::UniformGrid::default();
        let eid = orbisync_domain::EntityId::generate();
        let stale = orbisync_domain::EntityId::generate();
        let viewer = Some(orbisync_domain::Vec3::new(0.0, 0.0, 0.0).expect("vec"));
        let mut subscribed = std::collections::HashMap::new();
        subscribed.insert(stale, true);

        let v = n3_make_view(eid, 10.0);
        let p = n3_make_payload(eid, 1, 10.0);
        let _ = filter_payload_for_viewer(
            &p,
            viewer,
            None,
            &grid,
            std::slice::from_ref(&v),
            &mut subscribed,
        );
        assert!(
            !subscribed.contains_key(&stale),
            "stale entry not in snapshot must be pruned"
        );
        assert!(subscribed.contains_key(&eid));
    }

    #[test]
    fn n4_viewer_pinned_to_first_entity() {
        // Verifies W-7 step 4: viewer follows the first moved entity, not a second one.
        // If `should_follow_viewer` were `|_, _| true` (always update), the second
        // entity would incorrectly shift `viewer_pos` and this test goes red.
        // Production path: `handle_socket` calls `should_follow_viewer(viewer_entity, entity_id)`
        // before updating `viewer_pos`.
        let a = orbisync_domain::EntityId::generate();
        let b = orbisync_domain::EntityId::generate();
        assert!(
            should_follow_viewer(None, a),
            "first entity must be followed (no pin yet)"
        );
        let pinned = Some(a);
        assert!(
            should_follow_viewer(pinned, a),
            "same entity must continue to be followed"
        );
        assert!(
            !should_follow_viewer(pinned, b),
            "second entity must NOT be followed – pin stays on first"
        );
    }

    #[test]
    fn indexed_sink_keeps_unrelated_hysteresis_prunes_deletes_and_refreshes_owner_policy() {
        use crate::delivery::DeliverySink;
        let instance = InstanceId::generate();
        let owner = UserId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance, state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        let a = EntityId::generate();
        let b = EntityId::generate();
        let private = EntityId::generate();
        for (id, position, visibility) in [
            (a, Some(29.0), VisibilityPolicy::spatial(100.0).unwrap()),
            (b, Some(1.0), VisibilityPolicy::spatial(100.0).unwrap()),
            (private, None, VisibilityPolicy::OwnerOnly),
        ] {
            assert!(matches!(actor.handle(InstanceCommand::SpawnEntity {
                command_id: None, entity_id: id, kind: EntityKind::Object, owner: Some(owner),
                transform: position.and_then(|x| n3_make_entity(id, instance, x).transform()),
                visibility, requester: owner, permissions: WorldPermissions::all(),
            }), CommandOutcome::Applied { .. }));
        }
        let state = Arc::new(RealtimeState::builder(
            orbisync_config::Config::default().realtime, Arc::new(RuntimeRegistry::new()),
            Arc::new(DeliveryRegistry::new()), Arc::new(orbisync_domain::SystemClock::new()),
        ).build());
        let stale = EntityId::generate();
        let sink = RealtimeDeliverySink::new(state, instance, Vec3::new(0.0, 0.0, 0.0).unwrap(), owner, None,
            std::collections::HashMap::from([(b, true), (stale, true)]));
        let before = actor.interest_views();
        assert_eq!(sink.enqueue(&n3_make_payload(a, 1, 29.0), Reliability::LatestWins, Some(&before)).delivered, 1);
        assert!(!sink.filter.lock().unwrap().subscribed.contains_key(&stale));
        assert!(matches!(actor.handle(InstanceCommand::UpdateTransform {
            entity_id: a, transform: n3_make_entity(a, instance, 31.0).transform().unwrap(),
            expected_revision: orbisync_domain::Revision::from_u64(1), user_id: owner,
            now: orbisync_domain::Timestamp::from_unix_millis(1_000).unwrap(), permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        let moved = actor.interest_views();
        assert!(std::sync::Weak::ptr_eq(&before.membership_token(), &moved.membership_token()));
        assert_eq!(sink.enqueue(&n3_make_payload(a, 2, 31.0), Reliability::LatestWins, Some(&moved)).delivered, 1, "31m must retain subscription across a new snapshot");
        assert_eq!(sink.filter.lock().unwrap().subscribed.get(&b), Some(&true));
        assert!(matches!(actor.handle(InstanceCommand::DeleteEntity {
            command_id: None, entity_id: b, expected_revision: orbisync_domain::Revision::from_u64(1),
            requester: owner, permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        assert_eq!(sink.enqueue(&n3_make_payload(a, 3, 31.0), Reliability::LatestWins, Some(&actor.interest_views())).delivered, 1);
        assert!(!sink.filter.lock().unwrap().subscribed.contains_key(&b));
        let old_owner = actor.interest_views();
        assert_eq!(sink.enqueue(&n3_make_payload(private, 1, 0.0), Reliability::LatestWins, Some(&old_owner)).delivered, 1);
        let next_owner = UserId::generate();
        actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(), user_id: next_owner,
            instance_id: instance, capacity: 10,
        });
        assert!(matches!(actor.handle(InstanceCommand::TransferOwnership {
            command_id: None, entity_id: private, new_owner: Some(next_owner),
            expected_revision: orbisync_domain::Revision::from_u64(1),
            now: orbisync_domain::Timestamp::from_unix_millis(2_000).unwrap(), requester: owner,
            permissions: WorldPermissions::all(),
        }), CommandOutcome::Applied { .. }));
        let new_owner = actor.interest_views();
        assert!(std::sync::Weak::ptr_eq(&old_owner.membership_token(), &new_owner.membership_token()));
        assert_eq!(sink.enqueue(&n3_make_payload(private, 2, 0.0), Reliability::LatestWins, Some(&new_owner)).filtered, 1, "unchanged membership must not cache old authorization");
    }
