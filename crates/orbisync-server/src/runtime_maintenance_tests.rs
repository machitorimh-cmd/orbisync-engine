// Included in main's test module to reuse the persistence port fixtures.

#[derive(Default)]
struct TickCountingMetrics(AtomicUsize);

impl MetricsRecorder for TickCountingMetrics {
    fn incr(&self, _: Counter) {}
    fn add(&self, _: Counter, _: u64) {}
    fn set(&self, _: Gauge, _: i64) {}
    fn observe(&self, histogram: MetricsHistogram, _: f64) {
        if histogram == MetricsHistogram::TickDuration {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct GatedEntityStore {
    recorded: RecordingPersistentEntityStore,
    blocked: InstanceId,
    entered: Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait]
impl PersistentEntityStore for GatedEntityStore {
    async fn spawn(&self, entity: orbisync_domain::Entity) -> Result<(), ApplicationError> {
        if entity.instance_id() == self.blocked {
            self.entered.notify_one();
            self.release
                .acquire()
                .await
                .expect("release storage")
                .forget();
        }
        self.recorded.spawn(entity).await
    }
    async fn update(&self, entity: orbisync_domain::Entity) -> Result<(), ApplicationError> {
        self.recorded.update(entity).await
    }
    async fn transfer_ownership(
        &self,
        entity: orbisync_domain::Entity,
        audit: EntityOwnershipTransferAudit,
    ) -> Result<(), ApplicationError> {
        self.recorded.transfer_ownership(entity, audit).await
    }
    async fn upsert_component(
        &self,
        id: EntityId,
        instance: InstanceId,
        revision: Revision,
        now: Timestamp,
        key: String,
        payload: Vec<u8>,
    ) -> Result<(), ApplicationError> {
        self.recorded
            .upsert_component(id, instance, revision, now, key, payload)
            .await
    }
    async fn delete_component(
        &self,
        id: EntityId,
        instance: InstanceId,
        revision: Revision,
        now: Timestamp,
        key: String,
    ) -> Result<(), ApplicationError> {
        self.recorded
            .delete_component(id, instance, revision, now, key)
            .await
    }
    async fn delete(&self, id: EntityId, instance: InstanceId) -> Result<(), ApplicationError> {
        self.recorded.delete(id, instance).await
    }
    async fn list_by_instance(
        &self,
        instance: InstanceId,
    ) -> Result<Vec<orbisync_domain::Entity>, ApplicationError> {
        self.recorded.list_by_instance(instance).await
    }
}

async fn active_worker_instance(
    registry: &RuntimeRegistry,
    instance: InstanceId,
    user: UserId,
) -> orbisync_world_runtime::InstanceHandle {
    let handle = registry.ensure_instance(make_actor(instance));
    assert!(matches!(
        handle
            .submit(InstanceCommand::Join {
                presence_id: PresenceId::generate(),
                user_id: user,
                instance_id: instance,
                capacity: 10,
            })
            .await
            .unwrap(),
        CommandOutcome::Applied { .. }
    ));
    handle
}

fn worker_services(
    registry: Arc<RuntimeRegistry>,
    entities: Arc<dyn PersistentEntityStore>,
    extensions: Arc<dyn ExtensionOutboxStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    metrics: Arc<dyn MetricsRecorder>,
) -> super::runtime_maintenance::MaintenanceServices {
    super::runtime_maintenance::MaintenanceServices {
        registry,
        entities,
        extensions,
        checkpoints,
        metrics,
        delivery: Arc::new(DeliveryRegistry::new()),
        dedup: Arc::new(orbisync_server::command_dedup::CommandDedupStore::default()),
        checkpoint_permits: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CHECKPOINT_SAVES)),
    }
}

async fn wait_for_ticks(metrics: &TickCountingMetrics, target: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while metrics.0.load(Ordering::SeqCst) < target {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("ticks must continue while storage is blocked");
}

#[tokio::test]
async fn storage_wait_preserves_tick_cadence_order_and_shutdown_drain() {
    let registry = Arc::new(RuntimeRegistry::new());
    let instance = InstanceId::generate();
    let owner = UserId::generate();
    let handle = active_worker_instance(&registry, instance, owner).await;
    let healthy = active_worker_instance(&registry, InstanceId::generate(), owner).await;
    let entity = EntityId::generate();
    let other_entity = EntityId::generate();
    for (handle, entity_id) in [(&handle, entity), (&healthy, other_entity)] {
        assert!(matches!(
            handle
                .submit(InstanceCommand::SpawnEntity {
                    command_id: None,
                    entity_id,
                    kind: EntityKind::Object,
                    owner: Some(owner),
                    transform: None,
                    visibility: VisibilityPolicy::Global,
                    requester: owner,
                    permissions: WorldPermissions::all(),
                })
                .await
                .unwrap(),
            CommandOutcome::Applied { .. }
        ));
    }
    let store = Arc::new(GatedEntityStore {
        recorded: RecordingPersistentEntityStore::default(),
        blocked: instance,
        entered: Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let metrics = Arc::new(TickCountingMetrics::default());
    let extensions = Arc::new(RecordingExtensionStore::default());
    let workers = super::runtime_maintenance::RuntimeWorkers::spawn(
        worker_services(
            registry.clone(),
            store.clone(),
            extensions.clone(),
            Arc::new(cross_instance_store(InstanceId::generate())),
            metrics.clone(),
        ),
        100,
        Duration::from_secs(300),
        30_000,
    );
    tokio::time::timeout(Duration::from_secs(1), store.entered.notified())
        .await
        .unwrap();
    let tick_count = metrics.0.load(Ordering::SeqCst);
    wait_for_ticks(&metrics, tick_count + 8).await;
    assert!(
        store
            .recorded
            .calls
            .lock()
            .unwrap()
            .contains(&format!("spawn:{other_entity}")),
        "another instance's writes must progress"
    );
    assert!(matches!(
        handle
            .submit(InstanceCommand::UpdateEntityComponent {
                command_id: None,
                entity_id: entity,
                component_key: "com.test.ordered".into(),
                payload_bytes: b"{}".to_vec(),
                expected_revision: Revision::from_u64(1),
                now: Timestamp::from_unix_millis(1_000).unwrap(),
                requester: owner,
                permissions: WorldPermissions::all(),
            })
            .await
            .unwrap(),
        CommandOutcome::Applied { .. }
    ));
    assert!(matches!(
        handle
            .submit(InstanceCommand::DeleteEntity {
                command_id: None,
                entity_id: entity,
                expected_revision: Revision::from_u64(2),
                requester: owner,
                permissions: WorldPermissions::all(),
            })
            .await
            .unwrap(),
        CommandOutcome::Applied { .. }
    ));
    let shutdown = orbisync_server::shutdown::ShutdownState::new();
    shutdown.begin();
    super::mark_instances_draining(&shutdown, &registry).await;
    let mut drain = tokio::spawn(workers.drain(Duration::from_secs(2)));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut drain)
            .await
            .is_err(),
        "shutdown must wait for accepted writes"
    );
    store.release.add_permits(1);
    drain.await.expect("worker drain");
    let calls = store.recorded.calls.lock().unwrap();
    let ordered: Vec<_> = calls
        .iter()
        .filter(|call| call.contains(&entity.to_string()))
        .cloned()
        .collect();
    assert_eq!(
        ordered,
        vec![
            format!("spawn:{entity}"),
            format!("upsert_component:{entity}:com.test.ordered"),
            format!("delete:{entity}")
        ]
    );
    let facts = extensions.events.lock().unwrap();
    let relevant: Vec<_> = facts
        .iter()
        .filter(|fact| fact.instance_id() == Some(instance))
        .map(ExtensionEvent::kind)
        .collect();
    assert_eq!(
        relevant,
        vec![
            "member.joined",
            "entity.spawned",
            "entity.updated",
            "entity.deleted"
        ]
    );
}

#[tokio::test]
async fn checkpoint_wait_does_not_stop_periodic_ticks() {
    let registry = Arc::new(RuntimeRegistry::new());
    let instance = InstanceId::generate();
    active_worker_instance(&registry, instance, UserId::generate()).await;
    let store = Arc::new(cross_instance_store(instance));
    let metrics = Arc::new(TickCountingMetrics::default());
    let workers = super::runtime_maintenance::RuntimeWorkers::spawn(
        worker_services(
            registry.clone(),
            Arc::new(RecordingPersistentEntityStore::default()),
            Arc::new(RecordingExtensionStore::default()),
            store.clone(),
            metrics.clone(),
        ),
        100,
        Duration::from_secs(300),
        1,
    );
    tokio::time::timeout(Duration::from_secs(1), store.blocked_started.notified())
        .await
        .unwrap();
    wait_for_ticks(&metrics, metrics.0.load(Ordering::SeqCst) + 4).await;
    let shutdown = orbisync_server::shutdown::ShutdownState::new();
    shutdown.begin();
    super::mark_instances_draining(&shutdown, &registry).await;
    workers.drain(Duration::from_secs(3)).await;
}
