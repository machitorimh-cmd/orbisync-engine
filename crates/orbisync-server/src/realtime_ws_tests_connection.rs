    #[test]
    fn realtime_state_default_actor_capacity_matches_outbound_queue_capacity() {
        let config = orbisync_config::Config::default().realtime;
        let expected = config.outbound_queue_capacity as usize;
        let state = RealtimeState::new(
            config,
            Arc::new(RuntimeRegistry::new()),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        );

        assert_eq!(state.history_capacity, expected);
    }

    #[tokio::test]
    async fn activation_fails_closed_without_checkpoint_store() {
        let instance_id = InstanceId::generate();
        let registry = Arc::new(RuntimeRegistry::new());
        let state = RealtimeState::new(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        );

        let error = state
            .ensure_instance_activated(instance_id)
            .await
            .expect_err("activation must require a configured checkpoint store");

        assert!(error.to_string().contains("not configured"));
        assert!(!registry.contains(instance_id));
    }

    #[derive(Clone)]
    struct ActivationCheckpointStore {
        checkpoint: Option<orbisync_application::AppCheckpoint>,
        loads: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CheckpointStore for ActivationCheckpointStore {
        async fn save_checkpoint(
            &self,
            _checkpoint: orbisync_application::AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, orbisync_application::ApplicationError> {
            Ok(Vec::new())
        }

        async fn load_latest(
            &self,
            instance_id: InstanceId,
        ) -> Result<
            Option<orbisync_application::AppCheckpoint>,
            orbisync_application::ApplicationError,
        > {
            self.loads.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(self
                .checkpoint
                .as_ref()
                .filter(|checkpoint| checkpoint.instance_id == instance_id)
                .cloned())
        }
    }

    #[tokio::test]
    async fn concurrent_first_activation_restores_checkpoint_once() {
        use std::collections::HashMap;

        use orbisync_domain::{
            Entity, EntityId, EntityKind, Revision, Timestamp, VisibilityPolicy,
        };

        let instance_id = InstanceId::generate();
        let timestamp = Timestamp::from_unix_millis(10_000).expect("timestamp");
        let mut components = HashMap::new();
        components.insert("example.state".to_owned(), vec![1, 2, 3]);
        let entity = Entity::from_persisted(
            EntityId::generate(),
            instance_id,
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            Revision::from_u64(3),
            timestamp,
            timestamp,
            components,
        )
        .expect("entity");
        let runtime_checkpoint = orbisync_world_runtime::Checkpoint::new(
            instance_id,
            Revision::from_u64(9),
            vec![entity.clone()],
            timestamp,
        );
        let durable = orbisync_application::AppCheckpoint::new(
            instance_id,
            runtime_checkpoint.revision,
            runtime_checkpoint.to_json_bytes().expect("serialize"),
            timestamp,
        );
        let loads = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn CheckpointStore> = Arc::new(ActivationCheckpointStore {
            checkpoint: Some(durable),
            loads: Arc::clone(&loads),
        });
        let registry = Arc::new(RuntimeRegistry::new());
        let state = Arc::new(
            RealtimeState::builder(
                orbisync_config::Config::default().realtime,
                Arc::clone(&registry),
                Arc::new(DeliveryRegistry::new()),
                Arc::new(orbisync_domain::SystemClock::new()),
            )
            .with_checkpoint_store(store)
            .build(),
        );

        let mut joins = Vec::new();
        for _ in 0..8 {
            let state = Arc::clone(&state);
            joins.push(tokio::spawn(async move {
                state.ensure_instance_activated(instance_id).await
            }));
        }
        for join in joins {
            join.await.expect("activation task").expect("activation");
        }

        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let snapshot = registry
            .read_snapshot(instance_id)
            .await
            .expect("restored snapshot");
        assert_eq!(snapshot.revision, Revision::from_u64(9));
        assert_eq!(snapshot.entities, vec![entity]);
        assert_eq!(snapshot.members.len(), 0);
    }

    /// ADR-024 (`world.speed_acceleration_check_enabled`). Unlike the
    /// helper-level tests in `orbisync_world_runtime::validation`, this test
    /// exercises the real generation path: `RealtimeStateBuilder::build`
    /// without overriding the flag (so its own `unwrap_or(true)` default is
    /// what runs), `ensure_instance_activated`, and the task-owned actor's
    /// `InstanceCommand` handling. It fails if the default flips, if the
    /// builder-to-actor wiring is broken, or if the actor stops rejecting
    /// excess speed — not only if the low-level helper regresses.
    #[tokio::test]
    async fn speed_acceleration_check_defaults_to_enabled_through_the_real_activation_path() {
        use orbisync_domain::{Quaternion, Revision, Timestamp};

        let instance_id = InstanceId::generate();
        let store: Arc<dyn CheckpointStore> = Arc::new(ActivationCheckpointStore {
            checkpoint: None,
            loads: Arc::new(AtomicUsize::new(0)),
        });
        let registry = Arc::new(RuntimeRegistry::new());
        let state = RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_checkpoint_store(store)
        .build();
        // Deliberately no `.with_speed_acceleration_check(...)` call: this
        // exercises the builder's own default, not a value chosen here.

        state
            .ensure_instance_activated(instance_id)
            .await
            .expect("activation");

        let user = UserId::generate();
        let entity_id = EntityId::generate();
        let now = Timestamp::from_unix_millis(1_000).expect("valid timestamp");
        let permissions = WorldPermissions {
            entity_spawn: true,
            entity_update_own: true,
            entity_update_any: false,
        };

        let spawn = InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::identity(),
            expected_revision: Revision::from_u64(1),
            user_id: user,
            now,
            permissions,
        };
        let spawned = registry
            .submit(instance_id, spawn)
            .await
            .expect("mailbox accepts the first transform");
        let CommandOutcome::Applied {
            entity_revision: Some(spawned_revision),
            ..
        } = spawned
        else {
            panic!("first transform must auto-create the entity: {spawned:?}");
        };

        // Same `now` as the spawn: delta_seconds clamps to
        // `MIN_DELTA_SECONDS`, so 1 unit of movement is ~1000 m/s — far over
        // the default 50 m/s ceiling but under the 10 m per-tick teleport
        // limit, isolating the speed/acceleration check (mirrors
        // `validation::tests::speed_check_binds_at_min_delta`).
        let fast = InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::new(
                Vec3::new(1.0, 0.0, 0.0).expect("finite"),
                Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("finite"),
                Vec3::new(1.0, 1.0, 1.0).expect("finite"),
            )
            .expect("valid transform"),
            expected_revision: spawned_revision,
            user_id: user,
            now,
            permissions,
        };
        let outcome = registry
            .submit(instance_id, fast)
            .await
            .expect("mailbox accepts the second transform");
        assert!(
            matches!(
                outcome,
                CommandOutcome::Rejected {
                    code: "INVALID_TRANSFORM",
                    ..
                }
            ),
            "default speed_acceleration_check_enabled=true must reject excess speed, got {outcome:?}"
        );
    }

    /// Counterpart to the default-enabled test above: with the flag
    /// explicitly disabled through the same real activation path, the same
    /// excess-speed update is accepted, while the per-tick teleport distance
    /// check and ownership check — which the flag must never touch — still
    /// reject (ADR-024).
    #[tokio::test]
    async fn speed_acceleration_check_disabled_accepts_excess_speed_but_keeps_other_invariants() {
        use orbisync_domain::{Quaternion, Revision, Timestamp};

        let instance_id = InstanceId::generate();
        let store: Arc<dyn CheckpointStore> = Arc::new(ActivationCheckpointStore {
            checkpoint: None,
            loads: Arc::new(AtomicUsize::new(0)),
        });
        let registry = Arc::new(RuntimeRegistry::new());
        let state = RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_checkpoint_store(store)
        .with_speed_acceleration_check(false)
        .build();

        state
            .ensure_instance_activated(instance_id)
            .await
            .expect("activation");

        let owner = UserId::generate();
        let entity_id = EntityId::generate();
        let now = Timestamp::from_unix_millis(1_000).expect("valid timestamp");
        let permissions = WorldPermissions {
            entity_spawn: true,
            entity_update_own: true,
            entity_update_any: false,
        };

        let spawn = InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::identity(),
            expected_revision: Revision::from_u64(1),
            user_id: owner,
            now,
            permissions,
        };
        let spawned = registry
            .submit(instance_id, spawn)
            .await
            .expect("mailbox accepts the first transform");
        let CommandOutcome::Applied {
            entity_revision: Some(spawned_revision),
            ..
        } = spawned
        else {
            panic!("first transform must auto-create the entity: {spawned:?}");
        };

        let fast = InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::new(
                Vec3::new(1.0, 0.0, 0.0).expect("finite"),
                Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("finite"),
                Vec3::new(1.0, 1.0, 1.0).expect("finite"),
            )
            .expect("valid transform"),
            expected_revision: spawned_revision,
            user_id: owner,
            now,
            permissions,
        };
        let fast_outcome = registry
            .submit(instance_id, fast)
            .await
            .expect("mailbox accepts the second transform");
        let CommandOutcome::Applied {
            entity_revision: Some(after_fast_revision),
            ..
        } = fast_outcome
        else {
            panic!(
                "speed_acceleration_check_enabled=false must accept the excess-speed update, got {fast_outcome:?}"
            );
        };

        // Teleport distance is not gated by this flag: 100 units exceeds the
        // 10-unit per-tick limit regardless of the flag's value.
        let teleport = InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::new(
                Vec3::new(100.0, 0.0, 0.0).expect("finite"),
                Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("finite"),
                Vec3::new(1.0, 1.0, 1.0).expect("finite"),
            )
            .expect("valid transform"),
            expected_revision: after_fast_revision,
            user_id: owner,
            now,
            permissions,
        };
        let teleport_outcome = registry
            .submit(instance_id, teleport)
            .await
            .expect("mailbox accepts the teleport attempt");
        assert!(
            matches!(
                teleport_outcome,
                CommandOutcome::Rejected {
                    code: "INVALID_TRANSFORM",
                    ..
                }
            ),
            "disabling speed_acceleration_check_enabled must not weaken the per-tick teleport distance check, got {teleport_outcome:?}"
        );

        // Ownership is not gated by this flag: a non-owner without
        // entity_update_any is still rejected.
        let intruder = UserId::generate();
        let intruder_update = InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::identity(),
            expected_revision: after_fast_revision,
            user_id: intruder,
            now,
            permissions: WorldPermissions {
                entity_spawn: false,
                entity_update_own: true,
                entity_update_any: false,
            },
        };
        let intruder_outcome = registry
            .submit(instance_id, intruder_update)
            .await
            .expect("mailbox accepts the intruder attempt");
        assert!(
            matches!(
                intruder_outcome,
                CommandOutcome::Rejected {
                    code: "NOT_OWNER",
                    ..
                }
            ),
            "disabling speed_acceleration_check_enabled must not weaken ownership enforcement, got {intruder_outcome:?}"
        );
    }

    /// HIGH-001 (round 3): the process must bound how many instances restore
    /// from durable checkpoints at the same time, because each restore holds
    /// a payload-sized working set. Three concurrent activations must produce
    /// at most `MAX_CONCURRENT_INSTANCE_ACTIVATIONS` in-flight loads, and the
    /// blocked activation must proceed once a permit is released.
    #[tokio::test]
    async fn concurrent_activations_are_bounded_process_wide() {
        struct BlockingCheckpointStore {
            started: Arc<AtomicUsize>,
            gate: Arc<tokio::sync::Semaphore>,
        }

        #[async_trait::async_trait]
        impl CheckpointStore for BlockingCheckpointStore {
            async fn save_checkpoint(
                &self,
                _checkpoint: orbisync_application::AppCheckpoint,
            ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, orbisync_application::ApplicationError>
            {
                Ok(Vec::new())
            }

            async fn load_latest(
                &self,
                _instance_id: InstanceId,
            ) -> Result<
                Option<orbisync_application::AppCheckpoint>,
                orbisync_application::ApplicationError,
            > {
                self.started.fetch_add(1, Ordering::SeqCst);
                // Block until the test releases one permit.
                let _permit = self.gate.acquire().await.expect("gate open");
                Ok(None)
            }
        }

        let limit = MAX_CONCURRENT_INSTANCE_ACTIVATIONS;
        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let store: Arc<dyn CheckpointStore> = Arc::new(BlockingCheckpointStore {
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        });
        let registry = Arc::new(RuntimeRegistry::new());
        let state = Arc::new(
            RealtimeState::builder(
                orbisync_config::Config::default().realtime,
                Arc::clone(&registry),
                Arc::new(DeliveryRegistry::new()),
                Arc::new(orbisync_domain::SystemClock::new()),
            )
            .with_checkpoint_store(store)
            .build(),
        );

        let instances = [
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
        ];
        let mut tasks = Vec::new();
        for instance_id in instances {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                state.ensure_instance_activated(instance_id).await
            }));
        }

        async fn wait_for(counter: &AtomicUsize, target: usize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while counter.load(Ordering::SeqCst) < target {
                assert!(
                    std::time::Instant::now() < deadline,
                    "activation never reached {target} concurrent loads"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        wait_for(&started, limit).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            limit,
            "activations beyond the bound must not reach the checkpoint store"
        );

        gate.add_permits(1);
        wait_for(&started, limit + 1).await;
        for task in tasks {
            task.await
                .expect("activation task")
                .expect("activation completes");
        }
        for instance_id in instances {
            assert!(registry.contains(instance_id));
        }
    }

    /// HIGH-001 (round 4): the activation permit wait must be bounded. When
    /// both permits are held by in-flight restores, a third activation must
    /// fail closed with a clear timeout error instead of stalling forever.
    #[tokio::test]
    async fn activation_permit_wait_times_out_and_fails_closed() {
        struct BlockingCheckpointStore {
            started: Arc<AtomicUsize>,
            gate: Arc<tokio::sync::Semaphore>,
        }

        #[async_trait::async_trait]
        impl CheckpointStore for BlockingCheckpointStore {
            async fn save_checkpoint(
                &self,
                _checkpoint: orbisync_application::AppCheckpoint,
            ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, orbisync_application::ApplicationError>
            {
                Ok(Vec::new())
            }

            async fn load_latest(
                &self,
                _instance_id: InstanceId,
            ) -> Result<
                Option<orbisync_application::AppCheckpoint>,
                orbisync_application::ApplicationError,
            > {
                self.started.fetch_add(1, Ordering::SeqCst);
                let _permit = self.gate.acquire().await.expect("gate open");
                Ok(None)
            }
        }

        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let store: Arc<dyn CheckpointStore> = Arc::new(BlockingCheckpointStore {
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        });
        let registry = Arc::new(RuntimeRegistry::new());
        let mut state = RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_checkpoint_store(store)
        .build();
        // Shorten only the permit wait so the test fails fast on regression.
        state.activation_permit_timeout = std::time::Duration::from_millis(150);
        let state = Arc::new(state);

        let held_a = InstanceId::generate();
        let held_b = InstanceId::generate();
        let blocked_c = InstanceId::generate();
        let mut restores = Vec::new();
        for instance_id in [held_a, held_b] {
            let state = Arc::clone(&state);
            restores.push(tokio::spawn(async move {
                state.ensure_instance_activated(instance_id).await
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while started.load(Ordering::SeqCst) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the two permit holders never reached the checkpoint store"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let blocked = {
            let state = Arc::clone(&state);
            tokio::spawn(async move { state.ensure_instance_activated(blocked_c).await })
        };
        let error = blocked
            .await
            .expect("activation task")
            .expect_err("permit wait must time out");
        assert!(
            error.to_string().contains("timed out"),
            "unexpected error: {error}"
        );
        assert!(!registry.contains(blocked_c));

        gate.add_permits(2);
        for restore in restores {
            restore
                .await
                .expect("activation task")
                .expect("permit holders complete");
        }
        assert!(registry.contains(held_a));
        assert!(registry.contains(held_b));
        assert!(!registry.contains(blocked_c));
    }

    #[tokio::test]
    async fn corrupt_checkpoint_fails_closed_without_publishing_actor() {
        use orbisync_domain::{Revision, Timestamp};

        let instance_id = InstanceId::generate();
        let timestamp = Timestamp::from_unix_millis(10_000).expect("timestamp");
        let durable = orbisync_application::AppCheckpoint::new(
            instance_id,
            Revision::from_u64(9),
            br#"{"format_version":2}"#.to_vec(),
            timestamp,
        );
        let store: Arc<dyn CheckpointStore> = Arc::new(ActivationCheckpointStore {
            checkpoint: Some(durable),
            loads: Arc::new(AtomicUsize::new(0)),
        });
        let registry = Arc::new(RuntimeRegistry::new());
        let state = RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_checkpoint_store(store)
        .build();

        let error = state
            .ensure_instance_activated(instance_id)
            .await
            .expect_err("corrupt checkpoint must block activation");

        assert!(error.to_string().contains("not valid JSON"));
        assert!(!registry.contains(instance_id));
    }

    #[test]
    fn active_connection_guard_records_current_and_disconnect() {
        let metrics = Arc::new(PrometheusMetrics::new());
        let active = Arc::new(AtomicUsize::new(1));
        {
            let _guard = ActiveConnectionGuard {
                active: Arc::clone(&active),
                metrics: Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
            };
        }
        let text = metrics.render();
        assert!(text.contains("websocket_connections_current 0"));
        assert!(text.contains("websocket_disconnects_total{reason=\"transport\"} 1"));
    }

    #[test]
    fn member_gauge_is_refreshed_when_connection_guard_drops_after_leave() {
        use orbisync_domain::{PresenceId, UserId};
        use orbisync_world_runtime::{InstanceRuntimeDescriptor, RuntimeState};

        let registry = Arc::new(RuntimeRegistry::new());
        let instance_id = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id,
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        let metrics = Arc::new(PrometheusMetrics::new());
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard.insert(instance_id, actor);
        }
        let presence = PresenceId::generate();
        let user = UserId::generate();
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard
                .get_mut(&instance_id)
                .expect("actor")
                .submit(InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id,
                    capacity: 10,
                })
                .expect("join");
        }
        record_total_members(&registry, metrics.as_ref());
        assert!(metrics.render().contains("instance_members_current 1"));
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard
                .get_mut(&instance_id)
                .expect("actor")
                .submit(InstanceCommand::Leave {
                    presence_id: presence,
                })
                .expect("leave");
        }
        let active = Arc::new(AtomicUsize::new(1));
        {
            let _guard = ActiveConnectionGuard {
                active,
                metrics: Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
            };
        }
        // Aggregate member gauges are refreshed outside the connection guard,
        // after any registry lock held by the termination path is released.
        record_total_members(&registry, metrics.as_ref());
        assert!(metrics.render().contains("instance_members_current 0"));
    }

    #[test]
    fn member_gauge_is_not_refreshed_on_transport_connection_drop() {
        use orbisync_domain::{PresenceId, UserId};
        use orbisync_realtime::ResumeSessionStore;
        use orbisync_world_runtime::{InstanceRuntimeDescriptor, RuntimeState};

        let registry = Arc::new(RuntimeRegistry::new());
        let instance_id = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id,
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        let metrics = Arc::new(PrometheusMetrics::new());
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard.insert(instance_id, actor);
        }
        let presence = PresenceId::generate();
        let user = UserId::generate();
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard
                .get_mut(&instance_id)
                .expect("actor")
                .submit(InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id,
                    capacity: 10,
                })
                .expect("join");
        }
        let resume_store = Arc::new(ResumeSessionStore::new(
            Arc::new(orbisync_domain::SystemClock::new()),
            60,
        ));
        let _token = resume_store.issue(
            user,
            instance_id,
            presence,
            0,
            orbisync_domain::Revision::INITIAL,
        );
        // Deliberately make the gauge stale, as it was after the observed
        // transport disconnects. The two guards below mirror on_upgrade:
        // PresenceGuard is dropped before the common active-connection guard.
        metrics.set(Gauge::InstanceMembersCurrent, 99);
        let active = Arc::new(AtomicUsize::new(1));
        {
            let _active_guard = ActiveConnectionGuard {
                active,
                metrics: Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
            };
            let _presence_guard = PresenceGuard {
                delivery: Arc::new(DeliveryRegistry::new()),
                instance_id,
                presence,
                resume_store,
            };
        }
        // D-26 deliberately keeps the presence during the resume grace
        // window. The periodic prune path refreshes this gauge after the
        // registry lock is released; the connection guard must not do it.
        assert!(metrics.render().contains("instance_members_current 99"));
    }

    #[test]
    fn pruning_expired_resume_binding_releases_member_and_refreshes_gauge() {
        use orbisync_domain::{Clock, PresenceId, Timestamp, UserId};
        use orbisync_realtime::ResumeSessionStore;
        use orbisync_world_runtime::{InstanceRuntimeDescriptor, RuntimeState};

        struct TestClock(std::sync::Mutex<Timestamp>);
        impl Clock for TestClock {
            fn now(&self) -> Timestamp {
                *self.0.lock().expect("clock lock")
            }
        }

        let clock = Arc::new(TestClock(std::sync::Mutex::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("timestamp"),
        )));
        let registry = Arc::new(RuntimeRegistry::new());
        let delivery = Arc::new(DeliveryRegistry::new());
        let instance_id = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id,
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        });
        actor.start();
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard.insert(instance_id, actor);
        }
        let presence = PresenceId::generate();
        let user = UserId::generate();
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            guard
                .get_mut(&instance_id)
                .expect("actor")
                .submit(InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id,
                    capacity: 10,
                })
                .expect("join");
        }
        let resume_store = Arc::new(ResumeSessionStore::new(
            Arc::clone(&clock) as Arc<dyn Clock>,
            60,
        ));
        let _token = resume_store.issue(
            user,
            instance_id,
            presence,
            0,
            orbisync_domain::Revision::INITIAL,
        );
        resume_store.mark_disconnected(presence);
        let metrics = Arc::new(PrometheusMetrics::new());
        let state = RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            delivery,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .with_resume_store(resume_store)
        .with_metrics_recorder(Arc::clone(&metrics) as Arc<dyn MetricsRecorder>)
        .build();
        record_total_members(&registry, metrics.as_ref());
        assert!(metrics.render().contains("instance_members_current 1"));
        *clock.0.lock().expect("clock lock") =
            Timestamp::from_unix_millis(1_700_000_060_001).expect("timestamp");

        assert_eq!(state.prune_expired_resume_sessions(), 1);
        assert!(metrics.render().contains("instance_members_current 0"));
        let inner = registry.inner();
        let guard = inner.lock().expect("lock");
        assert_eq!(guard.get(&instance_id).expect("actor").member_count(), 0);
    }

    #[test]
    fn snapshot_bytes_count_the_logical_payload_once() {
        let metrics = PrometheusMetrics::new();
        record_snapshot_bytes(&metrics, 32_768);
        let text = metrics.render();
        assert!(text.contains("snapshot_bytes_total 32768"));
    }

    #[test]
    fn h1_join_drop_110_reclaims_capacity_and_delivery() {
        use orbisync_domain::{PresenceId, UserId};
        use orbisync_world_runtime::{
            InstanceRuntimeDescriptor, RuntimeState, command::InstanceCommand,
        };

        let registry = Arc::new(RuntimeRegistry::new());
        let delivery = Arc::new(DeliveryRegistry::new());
        let instance_id = InstanceId::generate();
        // create actor once
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            let descriptor = InstanceRuntimeDescriptor {
                instance_id,
                state: RuntimeState::Running,
                revision: orbisync_domain::Revision::INITIAL,
            };
            let mut actor = InstanceActor::new(descriptor);
            actor.start();
            guard.insert(instance_id, actor);
        }
        let capacity = 100u32;
        // One store for the whole test: `PresenceGuard::drop` marks the binding
        // disconnected here, and the eviction below is what the live join path
        // performs when the instance is full (W-18, D-26).
        let resume_store = Arc::new(ResumeSessionStore::new(
            Arc::new(orbisync_domain::SystemClock::new()),
            60,
        ));
        // join -> drop 110 times sequentially via guard
        for _ in 0..110 {
            let presence = PresenceId::generate();
            let user = UserId::generate();
            let outcome = {
                let inner = registry.inner();
                let mut g = inner.lock().expect("lock");
                g.get_mut(&instance_id)
                    .expect("actor")
                    .handle(InstanceCommand::Join {
                        presence_id: presence,
                        user_id: user,
                        instance_id,
                        capacity,
                    })
            };
            assert!(
                matches!(outcome, CommandOutcome::Applied { .. }),
                "join should succeed"
            );
            // The live join path issues a resume binding here, so the test must
            // too; without it there would be nothing for the guard to mark
            // disconnected and nothing to evict.
            let _token = resume_store.issue(
                user,
                instance_id,
                presence,
                0,
                orbisync_domain::Revision::INITIAL,
            );

            // simulate ws delivery registration
            let rx = delivery.register(instance_id);
            assert_eq!(delivery.sender_count(instance_id), 1);

            // guard lives for this iteration; drop order rx -> guard ensures cleanup_closed removes closed sender
            {
                let _guard = PresenceGuard {
                    delivery: Arc::clone(&delivery),
                    instance_id,
                    presence,
                    resume_store: Arc::clone(&resume_store),
                };
                {
                    let inner = registry.inner();
                    let g = inner.lock().expect("lock");
                    assert_eq!(g.get(&instance_id).expect("actor").member_count(), 1);
                }
                drop(rx);
                // sender is now closed but still in map until guard cleans
                assert_eq!(delivery.sender_count(instance_id), 1);
            }
            // After the guard drops, the presence deliberately stays for the
            // resume grace window (D-26), so capacity is NOT reclaimed yet.
            {
                let inner = registry.inner();
                let g = inner.lock().expect("lock");
                assert_eq!(
                    g.get(&instance_id).expect("actor").member_count(),
                    1,
                    "presence is held for the grace window so resume can rebind it"
                );
            }
            // Reclaiming it is the join path's job: evict the disconnected
            // binding and release its membership. Doing it here keeps the H-1
            // guarantee under test — 110 cycles must not exhaust capacity.
            let evicted = resume_store
                .evict_oldest_for_instance(instance_id)
                .expect("the dropped guard must have marked its binding disconnected");
            {
                let inner = registry.inner();
                let mut g = inner.lock().expect("lock");
                let _ = g
                    .get_mut(&instance_id)
                    .expect("actor")
                    .handle(InstanceCommand::Leave {
                        presence_id: evicted.presence_id,
                    });
            }
            {
                let inner = registry.inner();
                let g = inner.lock().expect("lock");
                assert_eq!(
                    g.get(&instance_id).expect("actor").member_count(),
                    0,
                    "member should be reclaimed once the grace binding is evicted"
                );
            }
            assert_eq!(
                delivery.sender_count(instance_id),
                0,
                "sender should be 0 after cleanup_closed"
            );
            assert_eq!(delivery.total_sender_count(), 0);
        }
        // 111th join (after 110 cycles) must still succeed – proves no leak to INSTANCE_FULL
        let presence = PresenceId::generate();
        let user = UserId::generate();
        let outcome = {
            let inner = registry.inner();
            let mut g = inner.lock().expect("lock");
            g.get_mut(&instance_id)
                .expect("actor")
                .handle(InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id,
                    capacity,
                })
        };
        assert!(
            matches!(outcome, CommandOutcome::Applied { .. }),
            "101st/111th join after 110 cycles should still succeed"
        );
        // cleanup
        {
            let inner = registry.inner();
            let mut g = inner.lock().expect("lock");
            let _ = g
                .get_mut(&instance_id)
                .expect("actor")
                .handle(InstanceCommand::Leave {
                    presence_id: presence,
                });
        }
        assert_eq!(delivery.sender_count(instance_id), 0);
    }

    #[test]
    fn h1_presence_guard_reclaims_on_panic() {
        use orbisync_domain::{PresenceId, UserId};
        use orbisync_world_runtime::{
            InstanceRuntimeDescriptor, RuntimeState, command::InstanceCommand,
        };

        let registry = Arc::new(RuntimeRegistry::new());
        let delivery = Arc::new(DeliveryRegistry::new());
        let instance_id = InstanceId::generate();
        {
            let inner = registry.inner();
            let mut guard = inner.lock().expect("lock");
            let descriptor = InstanceRuntimeDescriptor {
                instance_id,
                state: RuntimeState::Running,
                revision: orbisync_domain::Revision::INITIAL,
            };
            let mut actor = InstanceActor::new(descriptor);
            actor.start();
            guard.insert(instance_id, actor);
        }
        let capacity = 100u32;
        let presence = PresenceId::generate();
        let user = UserId::generate();
        {
            let inner = registry.inner();
            let mut g = inner.lock().expect("lock");
            let _ = g
                .get_mut(&instance_id)
                .expect("actor")
                .handle(InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id,
                    capacity,
                });
        }
        let rx = delivery.register(instance_id);
        let resume_store = Arc::new(ResumeSessionStore::new(
            Arc::new(orbisync_domain::SystemClock::new()),
            60,
        ));
        let _token = resume_store.issue(
            user,
            instance_id,
            presence,
            0,
            orbisync_domain::Revision::INITIAL,
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PresenceGuard {
                delivery: Arc::clone(&delivery),
                instance_id,
                presence,
                resume_store: Arc::clone(&resume_store),
            };
            drop(rx);
            panic!("simulated panic after join");
        }));
        assert!(result.is_err());
        // The sender is cleaned on unwind, and the binding is marked so the
        // slot can be reclaimed — a panic must not skip either (H-1). The
        // membership itself is held for the grace window (D-26), exactly as on
        // an ordinary disconnect.
        assert_eq!(delivery.sender_count(instance_id), 0);
        let evicted = resume_store
            .evict_oldest_for_instance(instance_id)
            .expect("Drop must run on unwind and mark the binding disconnected");
        {
            let inner = registry.inner();
            let mut g = inner.lock().expect("lock");
            let _ = g
                .get_mut(&instance_id)
                .expect("actor")
                .handle(InstanceCommand::Leave {
                    presence_id: evicted.presence_id,
                });
        }
        {
            let inner = registry.inner();
            let g = inner.lock().expect("lock");
            assert_eq!(g.get(&instance_id).expect("actor").member_count(), 0);
        }
        // Subsequent join must succeed
        let presence2 = PresenceId::generate();
        let user2 = UserId::generate();
        let outcome = {
            let inner = registry.inner();
            let mut g = inner.lock().expect("lock");
            g.get_mut(&instance_id)
                .expect("actor")
                .handle(InstanceCommand::Join {
                    presence_id: presence2,
                    user_id: user2,
                    instance_id,
                    capacity,
                })
        };
        assert!(matches!(outcome, CommandOutcome::Applied { .. }));
    }
