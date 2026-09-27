#[tokio::test]
async fn admission88_final_snapshot_waits_for_accepted_activation_and_join() {
    use std::future::Future;
    struct Store {
        entered: Notify,
        release: tokio::sync::Semaphore,
        saved: Mutex<Vec<AppCheckpoint>>,
    }
    #[async_trait]
    impl CheckpointStore for Store {
        async fn load_latest(
            &self,
            _: InstanceId,
        ) -> Result<Option<AppCheckpoint>, ApplicationError> {
            self.entered.notify_one();
            self.release.acquire().await.expect("restore gate").forget();
            Ok(None)
        }
        async fn save_checkpoint(
            &self,
            checkpoint: AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            self.saved.lock().unwrap().push(checkpoint);
            Ok(Vec::new())
        }
    }
    let store = Arc::new(Store {
        entered: Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        saved: Mutex::new(Vec::new()),
    });
    let state = Arc::new(
        orbisync_server::realtime_ws::RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::new(RuntimeRegistry::new()),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_checkpoint_store(store.clone())
        .build(),
    );
    let instance = InstanceId::generate();
    let activation = tokio::spawn({
        let state = state.clone();
        async move { state.ensure_instance_activated(instance).await }
    });
    store.entered.notified().await;
    state.shutdown.begin();
    let drain = super::mark_instances_draining(&state.shutdown, &state.registry);
    tokio::pin!(drain);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(
            Future::poll(drain.as_mut(), cx).is_pending()
        ))
        .await
    );
    assert!(
        state
            .ensure_instance_activated(InstanceId::generate())
            .await
            .is_err()
    );
    store.release.add_permits(1);
    let guard = activation
        .await
        .expect("activation task")
        .expect("accepted activation finishes");
    let user = UserId::generate();
    let joined = state
        .registry
        .submit(
            instance,
            InstanceCommand::Join {
                presence_id: PresenceId::generate(),
                user_id: user,
                instance_id: instance,
                capacity: 10,
            },
        )
        .await
        .expect("join mailbox");
    assert!(matches!(joined, CommandOutcome::Applied { .. }));
    let entity = EntityId::generate();
    assert!(matches!(
        state
            .registry
            .submit(
                instance,
                InstanceCommand::SpawnEntity {
                    command_id: None,
                    entity_id: entity,
                    kind: EntityKind::Object,
                    owner: Some(user),
                    transform: None,
                    visibility: VisibilityPolicy::Global,
                    requester: user,
                    permissions: WorldPermissions::all(),
                }
            )
            .await
            .expect("accepted work"),
        CommandOutcome::Applied { .. }
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(
            Future::poll(drain.as_mut(), cx).is_pending()
        ))
        .await
    );
    drop(guard);
    assert_eq!(
        drain.await,
        1,
        "late registered actor included in drain snapshot"
    );
    flush_shutdown_checkpoints(
        state.registry.clone(),
        store.clone(),
        state.command_dedup.clone(),
        state.metrics_recorder.clone(),
        Arc::new(tokio::sync::Semaphore::new(2)),
    )
    .await;
    let saved = store.saved.lock().unwrap();
    assert_eq!(saved.len(), 1);
    let checkpoint =
        orbisync_world_runtime::Checkpoint::from_json_bytes(&saved[0].payload).expect("checkpoint");
    assert_eq!(checkpoint.entities.len(), 1);
    assert_eq!(checkpoint.entities[0].id(), entity);
    assert!(checkpoint.timestamp <= state.clock.now());
}
