#![allow(clippy::unwrap_used, clippy::expect_used)]
// Regression oracles derived from preserved independent probes.
use super::*;
use orbisync_application::checkpoint_admission::{
    GenerationStore, SelectedGeneration, WriterToken,
};
use orbisync_domain::{Entity, EntityId, EntityKind, Revision, VisibilityPolicy};
use orbisync_world_runtime::{RuntimeRegistry, actor::InstanceActor};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "postwait_tests.rs"]
mod postwait;

fn wire_command() -> orbisync_protocol::v1::EntityCommand {
    orbisync_protocol::v1::EntityCommand {
        command_id: orbisync_domain::CommandId::generate().to_string(),
        entity_id: EntityId::generate().to_string(),
        operation: "spawn".into(),
        expected_revision: 0,
        arguments: None,
        instance_revision: None,
    }
}
async fn submit_wire(
    state: &crate::realtime_ws::RealtimeState,
    store: &Store,
    command: &orbisync_protocol::v1::EntityCommand,
) -> orbisync_world_runtime::actor::AdmissionResult {
    let owner = orbisync_domain::UserId::generate();
    crate::checkpoint_admission::submit_generation_command(
        &state.registry,
        store.instance(),
        orbisync_world_runtime::command::InstanceCommand::SpawnEntity {
            command_id: Some(command.command_id.parse().unwrap()),
            entity_id: command.entity_id.parse().unwrap(),
            kind: EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: orbisync_world_runtime::command::WorldPermissions::all(),
        },
        command.clone(),
        crate::command_dedup::CommandDedupStore::fingerprint(command),
        uuid::Uuid::now_v7(),
        state.clock.now().to_unix_millis().unwrap(),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn fix74_actual_dispatch_recovers_same_attempt_replays_and_conflicts() {
    use crate::checkpoint_admission::{GenerationDispatch, dispatch_generation_receipt};
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let command = wire_command();
    submit_wire(&state, &store, &command).await;
    let handle = state.registry.handle(store.instance()).unwrap();
    store.fail.store(true, Ordering::Release);
    assert!(
        service
            .persist(handle.clone(), state.clock.now())
            .await
            .is_err()
    );
    let pinned = store.publications.lock().unwrap()[0].clone();
    assert!(matches!(
        dispatch_generation_receipt(
            &state,
            store.instance(),
            &wire_command(),
            tokio::time::Instant::now() + Duration::from_secs(2)
        )
        .await
        .unwrap(),
        GenerationDispatch::Refused { .. }
    ));
    store.fail.store(false, Ordering::Release);
    assert!(matches!(
        dispatch_generation_receipt(
            &state,
            store.instance(),
            &command,
            tokio::time::Instant::now() + Duration::from_secs(2)
        )
        .await
        .unwrap(),
        GenerationDispatch::Reply(_)
    ));
    assert_eq!(
        store.publications.lock().unwrap().as_slice(),
        &[pinned.clone(), pinned]
    );
    let revision = handle.read_snapshot().await.unwrap().revision;
    // Entity already exists; replay dispatch never consults mutable entity/hook
    // validation or creates another generation.
    assert!(matches!(
        dispatch_generation_receipt(
            &state,
            store.instance(),
            &command,
            tokio::time::Instant::now() + Duration::from_secs(2)
        )
        .await
        .unwrap(),
        GenerationDispatch::Reply(_)
    ));
    let mut changed = command.clone();
    changed.entity_id = EntityId::generate().to_string();
    assert!(
        matches!(dispatch_generation_receipt(&state, store.instance(), &changed, tokio::time::Instant::now()+Duration::from_secs(2)).await.unwrap(), GenerationDispatch::Refused { code, .. } if code == "COMMAND_ID_CONFLICT")
    );
    assert_eq!(handle.read_snapshot().await.unwrap().revision, revision);
    store.writer.invalidate();
    assert!(matches!(
        dispatch_generation_receipt(
            &state,
            store.instance(),
            &command,
            tokio::time::Instant::now() + Duration::from_secs(2)
        )
        .await
        .unwrap(),
        GenerationDispatch::Refused { .. }
    ));
    service.drain().await;
}
#[tokio::test]
async fn fix74_actual_invalid_dispatch_persists_rejection_and_conflict() {
    use crate::checkpoint_admission::{
        GenerationDispatch, dispatch_generation_receipt, reject_generation_command,
    };
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let mut command = wire_command();
    command.entity_id = "invalid".into();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    reject_generation_command(
        &state,
        store.instance(),
        &command,
        "invalid entity".into(),
        deadline,
    )
    .await
    .unwrap();
    assert!(
        matches!(dispatch_generation_receipt(&state, store.instance(), &command, deadline).await.unwrap(), GenerationDispatch::Reply(crate::command_dedup::CommandDedupResult::Rejected { code, .. }) if code == "INVALID_ARGUMENT")
    );
    command.entity_id = EntityId::generate().to_string();
    assert!(
        matches!(dispatch_generation_receipt(&state, store.instance(), &command, deadline).await.unwrap(), GenerationDispatch::Refused { code, .. } if code == "COMMAND_ID_CONFLICT")
    );
    assert!(
        state
            .registry
            .read_snapshot(store.instance())
            .await
            .unwrap()
            .entities
            .is_empty()
    );
    assert_eq!(store.publications.lock().unwrap().len(), 1);
    service.drain().await;
}
#[tokio::test]
async fn fix74_projection_pages_advance_past_one_hundred_failures_and_wrap() {
    let store = Store::new();
    let service = store.service();
    let mut ids: Vec<_> = (0..101).map(|_| InstanceId::generate()).collect();
    ids.sort();
    *store.backlog.lock().unwrap() = ids.clone();
    let first = service.projection_page().await.unwrap();
    assert_eq!(first, ids[..100]);
    store.fail.store(true, Ordering::Release);
    for id in first {
        assert!(service.project(id).await.is_err());
    }
    assert_eq!(service.projection_page().await.unwrap(), ids[100..]);
    // Simulate healthy final item's selected head, with the prefix still pending.
    store.checkpoint.lock().unwrap().instance_id = ids[100];
    store.fail.store(false, Ordering::Release);
    service.project(ids[100]).await.unwrap();
    assert_eq!(store.applied.load(Ordering::Acquire), 1);
    assert!(service.projection_page().await.unwrap().is_empty());
    assert_eq!(service.projection_page().await.unwrap(), ids[..100]);
    service.drain().await;
    let restarted = store.service();
    assert_eq!(restarted.projection_page().await.unwrap(), ids[..100]);
    assert_eq!(restarted.projection_page().await.unwrap(), ids[100..]);
    restarted.drain().await;
}

struct OperatorFixture {
    calls: AtomicUsize,
    change_source: Option<Arc<Store>>,
}
#[async_trait::async_trait]
impl crate::checkpoint_operator::Approval for OperatorFixture {
    async fn approve(
        &self,
        _: InstanceId,
        _: &orbisync_application::checkpoint_admission::ReconciledAuthority,
        _: orbisync_storage_postgres::ReconciliationDecision,
    ) -> Result<(), ApplicationError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(store) = &self.change_source {
            store.rows.lock().unwrap().push(store.row());
        }
        Ok(())
    }
}
fn review_digest(artifact: &serde_json::Value) -> String {
    crate::checkpoint_operator::digest(&serde_json::to_vec(artifact).unwrap())
}
#[tokio::test]
async fn fix74_operator_review_approval_conversion_and_same_attempt_retry() {
    let store = Store::new();
    let service = store.service();
    let mut artifact = crate::checkpoint_operator::inspect(&service, store.instance())
        .await
        .unwrap();
    assert!(artifact["decision"].is_null());
    artifact["chosen_checkpoint"] =
        serde_json::from_slice(&store.checkpoint.lock().unwrap().to_json_bytes().unwrap()).unwrap();
    artifact["decision"] = "new_empty".into();
    artifact["never_admitted"] = true.into();
    artifact["evidence"] = "reviewed creation records; never admitted".into();
    let operator = Arc::new(OperatorFixture {
        calls: AtomicUsize::new(0),
        change_source: None,
    });
    store.fail.store(true, Ordering::Release);
    assert!(
        crate::checkpoint_operator::apply(
            &service,
            operator.clone(),
            artifact.clone(),
            &review_digest(&artifact),
            1000
        )
        .await
        .is_err()
    );
    assert_eq!(operator.calls.load(Ordering::Acquire), 1);
    let attempt = service.pending_conversion_identity().unwrap();
    store.fail.store(false, Ordering::Release);
    service.retry_conversion().await.unwrap();
    let attempts = store.conversions.lock().unwrap().clone();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    assert_eq!(attempt, format!("{:?}", attempts[1]));
    assert!(service.pending_conversion_identity().is_none());
    assert_eq!(operator.calls.load(Ordering::Acquire), 1);
    service.drain().await;
}
#[tokio::test]
async fn fix74_operator_ambiguous_unapproved_and_changed_sources_refuse() {
    let store = Store::new();
    let service = store.service();
    let operator = Arc::new(OperatorFixture {
        calls: AtomicUsize::new(0),
        change_source: None,
    });
    let mut artifact = crate::checkpoint_operator::inspect(&service, store.instance())
        .await
        .unwrap();
    artifact["chosen_checkpoint"] =
        serde_json::from_slice(&store.checkpoint.lock().unwrap().to_json_bytes().unwrap()).unwrap();
    for decision in [
        serde_json::Value::Null,
        "trusted_history".into(),
        "new_empty".into(),
        "baseline".into(),
    ] {
        artifact["decision"] = decision;
        assert!(
            crate::checkpoint_operator::apply(
                &service,
                operator.clone(),
                artifact.clone(),
                &review_digest(&artifact),
                1000
            )
            .await
            .is_err()
        );
    }
    artifact["decision"] = "new_empty".into();
    artifact["never_admitted"] = true.into();
    artifact["evidence"] = "reviewed creation proof".into();
    assert!(
        crate::checkpoint_operator::apply(
            &service,
            operator.clone(),
            artifact.clone(),
            "unapproved",
            1000
        )
        .await
        .is_err()
    );
    // Fixed-selection drift, including checkpoint-only and row-only cases.
    store.inventory_checkpoint.store(true, Ordering::Release);
    assert!(
        crate::checkpoint_operator::apply(
            &service,
            operator.clone(),
            artifact.clone(),
            &review_digest(&artifact),
            1000
        )
        .await
        .is_err()
    );
    store.inventory_checkpoint.store(false, Ordering::Release);
    store.rows.lock().unwrap().push(store.row());
    assert!(
        crate::checkpoint_operator::apply(
            &service,
            operator.clone(),
            artifact.clone(),
            &review_digest(&artifact),
            1000
        )
        .await
        .is_err()
    );
    store.rows.lock().unwrap().clear();
    assert_eq!(operator.calls.load(Ordering::Acquire), 0);
    let operator = Arc::new(OperatorFixture {
        calls: AtomicUsize::new(0),
        change_source: Some(store.clone()),
    });
    assert!(
        crate::checkpoint_operator::apply(
            &service,
            operator.clone(),
            artifact.clone(),
            &review_digest(&artifact),
            1000
        )
        .await
        .is_err()
    );
    assert_eq!(operator.calls.load(Ordering::Acquire), 1);
    assert!(store.conversions.lock().unwrap().is_empty());
    assert!(service.pending_conversion_identity().is_none());
    service.drain().await;
}

async fn review71_submit(
    state: &crate::realtime_ws::RealtimeState,
    store: &Store,
    id: orbisync_domain::CommandId,
    entity: EntityId,
    owner: orbisync_domain::UserId,
) -> orbisync_world_runtime::actor::AdmissionResult {
    crate::checkpoint_admission::submit_generation_command(
        &state.registry,
        store.instance(),
        orbisync_world_runtime::command::InstanceCommand::SpawnEntity {
            command_id: Some(id),
            entity_id: entity,
            kind: EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: orbisync_world_runtime::command::WorldPermissions::all(),
        },
        orbisync_protocol::v1::EntityCommand {
            command_id: id.to_string(),
            entity_id: entity.to_string(),
            operation: "spawn".into(),
            expected_revision: 0,
            arguments: None,
            instance_revision: None,
        },
        [1; 32],
        uuid::Uuid::now_v7(),
        1000,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn fix74_uncertain_same_id_triggers_ws_retry() {
    use orbisync_application::checkpoint_admission::AdmissionPhase;
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let id = orbisync_domain::CommandId::generate();
    let entity = EntityId::generate();
    let owner = orbisync_domain::UserId::generate();
    assert_eq!(
        review71_submit(&state, &store, id, entity, owner)
            .await
            .phase,
        AdmissionPhase::Completed
    );
    store.fail.store(true, Ordering::Release);
    let handle = state.registry.handle(store.instance()).unwrap();
    assert!(
        service
            .persist(handle.clone(), Timestamp::from_unix_millis(1000).unwrap())
            .await
            .is_err()
    );
    store.fail.store(false, Ordering::Release);
    let replay = review71_submit(&state, &store, id, entity, owner).await;
    let ws_will_persist = replay.phase == AdmissionPhase::Completed
        || (matches!(
            &replay.outcome,
            orbisync_world_runtime::command::CommandOutcome::Rejected {
                code: "PERSISTENCE_UNAVAILABLE",
                ..
            }
        ) && replay.receipt.is_some());
    println!(
        "uncertain replay phase={:?}, receipt={}, WS persist predicate={ws_will_persist}",
        replay.phase,
        replay.receipt.is_some()
    );
    assert_eq!(replay.phase, AdmissionPhase::Replay);
    assert!(ws_will_persist);
    service
        .persist(handle, Timestamp::from_unix_millis(2000).unwrap())
        .await
        .unwrap();
    assert_eq!(
        review71_submit(&state, &store, id, entity, owner)
            .await
            .phase,
        AdmissionPhase::Replay
    );
    service.drain().await;
}

#[tokio::test]
async fn fix74_expired_commit_releases_publication_after_reap() {
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let id = orbisync_domain::CommandId::generate();
    review71_submit(
        &state,
        &store,
        id,
        EntityId::generate(),
        orbisync_domain::UserId::generate(),
    )
    .await;
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    service.retain_publication(store.instance(), id.to_string(), move || drop(owner));
    let handle = state.registry.handle(store.instance()).unwrap();
    store.fail.store(true, Ordering::Release);
    assert!(
        service
            .persist(handle.clone(), Timestamp::from_unix_millis(1000).unwrap())
            .await
            .is_err()
    );
    store.fail.store(false, Ordering::Release);
    assert!(
        service
            .persist(
                handle.clone(),
                Timestamp::from_unix_millis(86_402_000).unwrap()
            )
            .await
            .is_err()
    );
    service
        .persist(handle, Timestamp::from_unix_millis(86_403_000).unwrap())
        .await
        .unwrap();
    assert!(
        service
            .reap(
                &state.registry,
                store.instance(),
                Timestamp::from_unix_millis(86_404_000).unwrap()
            )
            .await
            .unwrap()
            .is_some()
    );
    service.drain().await;
    println!(
        "actor removed={}, publication still retained after expiry/reap/drain={}",
        !state.registry.contains(store.instance()),
        weak.upgrade().is_some()
    );
    assert!(!state.registry.contains(store.instance()));
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn fix74_expired_callback_is_discarded_and_reused_id_gets_new_owner() {
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let id = orbisync_domain::CommandId::generate();
    let owner = orbisync_domain::UserId::generate();
    review71_submit(&state, &store, id, EntityId::generate(), owner).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let old_calls = calls.clone();
    service.retain_publication(store.instance(), id.to_string(), move || {
        old_calls.fetch_add(100, Ordering::AcqRel);
    });
    let handle = state.registry.handle(store.instance()).unwrap();
    store.fail.store(true, Ordering::Release);
    assert!(
        service
            .persist(handle.clone(), Timestamp::from_unix_millis(1000).unwrap())
            .await
            .is_err()
    );
    store.fail.store(false, Ordering::Release);
    let error = service
        .persist(
            handle.clone(),
            Timestamp::from_unix_millis(86_402_000).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        orbisync_application::ApplicationErrorKind::CommittedButExpired
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(service.publications.lock().unwrap().is_empty());
    assert!(service.retained.lock().unwrap().is_empty());
    service
        .persist(
            handle.clone(),
            Timestamp::from_unix_millis(86_403_000).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        review71_submit(&state, &store, id, EntityId::generate(), owner)
            .await
            .phase,
        orbisync_application::checkpoint_admission::AdmissionPhase::Completed
    );
    let new_calls = calls.clone();
    service.retain_publication(store.instance(), id.to_string(), move || {
        new_calls.fetch_add(1, Ordering::AcqRel);
    });
    service
        .persist(handle, Timestamp::from_unix_millis(86_404_000).unwrap())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    service.drain().await;
}

#[test]
fn fix74_ws_dispatch_precedes_fresh_limiter_hooks_and_invalid_legacy_helper() {
    let source = include_str!("../realtime_ws_connection_runtime.rs");
    let dispatch = source.find("dispatch_generation_receipt(&state").unwrap();
    let scope = source[..dispatch]
        .rfind("ephemeral_participation_allowed(")
        .unwrap();
    let session = source[..dispatch]
        .rfind("is_session_still_active(")
        .unwrap();
    let limiter = source.find("rate_limiter.check(now, category)").unwrap();
    let hooks = source
        .find("gate.validate(&registration, &request)")
        .unwrap();
    assert!(scope < dispatch && session < dispatch && dispatch < limiter && limiter < hooks);
    let rejection = source.find("reject_generation_command(&state").unwrap();
    assert!(
        rejection
            < source[rejection..]
                .find("persist_command_outcome(")
                .unwrap()
                + rejection
    );
}

#[derive(Debug)]
struct PublicationDelay {
    started: tokio::sync::Notify,
    release: Semaphore,
    committed: Mutex<Option<GenerationAttempt>>,
    mutations: AtomicUsize,
}
#[derive(Debug)]
struct Store {
    limits: CheckpointLimits,
    writer: WriterPermit,
    checkpoint: Mutex<Checkpoint>,
    head: AtomicUsize,
    rows: Mutex<Vec<Entity>>,
    applied: AtomicUsize,
    fail: AtomicBool,
    selects: AtomicUsize,
    pause: Semaphore,
    block: AtomicBool,
    conversions: Mutex<Vec<GenerationAttempt>>,
    declared_total: AtomicUsize,
    publications: Mutex<Vec<GenerationAttempt>>,
    backlog: Mutex<Vec<InstanceId>>,
    inventory_checkpoint: AtomicBool,
    publication_delay: Mutex<Option<Arc<PublicationDelay>>>,
}
impl Store {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            limits: CheckpointLimits::default(),
            writer: WriterPermit::new(WriterToken {
                epoch: 1,
                boot: uuid::Uuid::now_v7(),
            })
            .unwrap(),
            checkpoint: Mutex::new(Checkpoint::new(
                InstanceId::generate(),
                Revision::INITIAL,
                vec![],
                Timestamp::from_unix_millis(1000).unwrap(),
            )),
            head: AtomicUsize::new(1),
            rows: Mutex::new(vec![]),
            applied: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            selects: AtomicUsize::new(0),
            pause: Semaphore::new(0),
            block: AtomicBool::new(false),
            conversions: Mutex::new(Vec::new()),
            declared_total: AtomicUsize::new(0),
            publications: Mutex::new(Vec::new()),
            backlog: Mutex::new(Vec::new()),
            inventory_checkpoint: AtomicBool::new(false),
            publication_delay: Mutex::new(None),
        })
    }
    fn service(self: &Arc<Self>) -> Arc<GenerationServices> {
        GenerationServices::new(self.limits, self.writer.clone(), self.clone()).unwrap()
    }
    fn instance(&self) -> InstanceId {
        self.checkpoint.lock().unwrap().instance_id
    }
    fn row(&self) -> Entity {
        Entity::new(
            EntityId::generate(),
            self.instance(),
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            Timestamp::from_unix_millis(1000).unwrap(),
        )
    }
}
#[async_trait::async_trait]
impl GenerationStore for Store {
    fn limits(&self) -> CheckpointLimits {
        self.limits
    }
    async fn publish(
        &self,
        attempt: &GenerationAttempt,
        source: &mut dyn GenerationSource,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.publications.lock().unwrap().push(attempt.clone());
        let delay = self.publication_delay.lock().unwrap().clone();
        if let Some(delay) = &delay
            && delay.committed.lock().unwrap().as_ref() == Some(attempt)
        {
            return Ok(GenerationResolution::Committed {
                publish_seq: attempt.expected_head + 1,
            });
        }
        while source.next_chunk().await?.is_some() {}
        if let Some(delay) = delay {
            *delay.committed.lock().unwrap() = Some(attempt.clone());
            delay.mutations.fetch_add(1, Ordering::AcqRel);
            self.head
                .store((attempt.expected_head + 1) as usize, Ordering::Release);
            delay.started.notify_one();
            delay.release.acquire().await.unwrap().forget();
        }
        if self.fail.load(Ordering::Acquire) {
            return Ok(GenerationResolution::Uncertain);
        }
        self.head
            .store((attempt.expected_head + 1) as usize, Ordering::Release);
        Ok(GenerationResolution::Committed {
            publish_seq: attempt.expected_head + 1,
        })
    }
    async fn resolve(
        &self,
        _: &GenerationAttempt,
    ) -> Result<GenerationResolution, ApplicationError> {
        Ok(GenerationResolution::Uncertain)
    }
}
#[async_trait::async_trait]
impl GenerationRecovery for Store {
    async fn inventory(
        &self,
        instance: InstanceId,
    ) -> Result<orbisync_application::checkpoint_admission::LegacyInventory, ApplicationError> {
        let checkpoint = self.checkpoint.lock().unwrap();
        let selected = self.inventory_checkpoint.load(Ordering::Acquire);
        Ok(
            orbisync_application::checkpoint_admission::LegacyInventory {
                source_id: selected.then_some(instance.as_uuid()),
                source_digest: selected.then_some([1; 32]),
                checkpoint: if selected {
                    Some(orbisync_application::AppCheckpoint {
                        instance_id: instance,
                        revision: checkpoint.revision,
                        payload: checkpoint.to_json_bytes().unwrap(),
                        created_at: checkpoint.timestamp,
                    })
                } else {
                    None
                },
                rows: self.rows.lock().unwrap().clone(),
            },
        )
    }
    async fn pending_projections(
        &self,
        after: Option<InstanceId>,
    ) -> Result<Vec<InstanceId>, ApplicationError> {
        Ok(self
            .backlog
            .lock()
            .unwrap()
            .iter()
            .filter(|id| after.is_none_or(|after| **id > after))
            .take(100)
            .copied()
            .collect())
    }

    async fn select(&self, _: InstanceId) -> Result<SelectedGeneration, ApplicationError> {
        self.selects.fetch_add(1, Ordering::Release);
        if self.block.load(Ordering::Acquire) {
            self.pause.acquire().await.unwrap().forget();
        }
        let checkpoint = Arc::new(self.checkpoint.lock().unwrap().clone());
        let mut manifest = checkpoint.stream_manifest(self.limits, &AtomicBool::new(false))?;
        let declared = self.declared_total.load(Ordering::Acquire);
        if declared != 0 {
            manifest.serialized_bytes = declared as u64;
            manifest.chunk_count = (declared as u64).div_ceil(manifest.chunk_bytes);
        }
        let attempt = GenerationAttempt::from_manifest(
            checkpoint.instance_id,
            self.head.load(Ordering::Acquire) as i64 - 1,
            self.writer.token(),
            1000,
            orbisync_application::checkpoint_record::canonical::VERSION,
            self.limits,
            &manifest,
        )?;
        Ok(SelectedGeneration {
            attempt,
            source: Box::new(EncodedCheckpoint::new(checkpoint, self.limits, manifest)?),
            cutoff_millis: None,
        })
    }
    async fn convert(
        &self,
        attempt: &GenerationAttempt,
        source: &mut dyn GenerationSource,
        _: &orbisync_application::checkpoint_admission::ReconciledAuthority,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.conversions.lock().unwrap().push(attempt.clone());
        self.publish(attempt, source).await
    }
    async fn project(
        &self,
        selected: &GenerationAttempt,
        entities: &[Entity],
    ) -> Result<(), ApplicationError> {
        if self.fail.load(Ordering::Acquire)
            || selected.expected_head + 1 != self.head.load(Ordering::Acquire) as i64
        {
            return Err(unavailable());
        }
        *self.rows.lock().unwrap() = entities.to_vec();
        self.applied
            .store((selected.expected_head + 1) as usize, Ordering::Release);
        Ok(())
    }
}
fn state(service: Arc<GenerationServices>) -> crate::realtime_ws::RealtimeState {
    crate::realtime_ws::RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::new(RuntimeRegistry::new()),
        Arc::new(crate::delivery::DeliveryRegistry::new()),
        Arc::new(orbisync_domain::SystemClock::new()),
    )
    .with_generation_services(service)
    .with_checkpoint_limits(CheckpointLimits::new(16384, 2097152).unwrap())
    .build()
}

#[tokio::test]
async fn phase4_activation_builder_identity_and_writer_loss() {
    let store = Store::new();
    let row = store.row();
    store.rows.lock().unwrap().push(row);
    let service = store.service();
    let state = state(service.clone());
    assert!(Arc::ptr_eq(state.generation.as_ref().unwrap(), &service));
    assert_eq!(state.checkpoint_limits, service.limits);
    let guard = state
        .ensure_instance_activated(store.instance())
        .await
        .unwrap();
    drop(guard);
    let snapshot = state
        .registry
        .handle(store.instance())
        .unwrap()
        .read_snapshot()
        .await
        .unwrap();
    assert!(
        snapshot.entities.is_empty(),
        "row-only entity must not resurrect"
    );
    store.writer.invalidate();
    assert!(
        state
            .ensure_instance_activated(store.instance())
            .await
            .is_err()
    );
    assert!(
        state
            .registry
            .handle(store.instance())
            .unwrap()
            .read_snapshot()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn phase4_projection_failure_delete_restart_and_lag() {
    let store = Store::new();
    store.rows.lock().unwrap().push(store.row());
    let service = store.service();
    store.fail.store(true, Ordering::Release);
    assert!(service.project(store.instance()).await.is_err());
    assert_eq!(store.rows.lock().unwrap().len(), 1);
    assert_eq!(store.applied.load(Ordering::Acquire), 0);
    store.fail.store(false, Ordering::Release);
    // A fresh service represents restart; durable target is still pending.
    let restarted = store.service();
    restarted.project(store.instance()).await.unwrap();
    restarted.project(store.instance()).await.unwrap();
    assert!(store.rows.lock().unwrap().is_empty());
    assert_eq!(store.applied.load(Ordering::Acquire), 1);
    let selected = store.select(store.instance()).await.unwrap();
    store.head.store(2, Ordering::Release);
    assert!(store.project(&selected.attempt, &[]).await.is_err());
}

#[tokio::test]
async fn phase4_cancelled_caller_keeps_global_permit_until_cleanup() {
    let store = Store::new();
    store.block.store(true, Ordering::Release);
    let service = store.service();
    let instance = store.instance();
    let first = {
        let service = service.clone();
        tokio::spawn(async move { service.restore(instance).await })
    };
    let second = {
        let service = service.clone();
        tokio::spawn(async move { service.restore(instance).await })
    };
    while store.selects.load(Ordering::Acquire) < 2 {
        tokio::task::yield_now().await;
    }
    first.abort();
    second.abort();
    let third = {
        let service = service.clone();
        tokio::spawn(async move { service.project(instance).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(store.selects.load(Ordering::Acquire), 2);
    assert_eq!(service.jobs.available_permits(), 0);
    store.block.store(false, Ordering::Release);
    store.pause.add_permits(2);
    third.await.unwrap().unwrap();
    service.drain().await;
    assert_eq!(service.jobs.available_permits(), 2);
}

#[tokio::test]
async fn phase4_failed_reap_retains_same_actor_and_generation() {
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    store.fail.store(true, Ordering::Release);
    assert!(
        service
            .reap(
                &state.registry,
                store.instance(),
                Timestamp::from_unix_millis(2000).unwrap()
            )
            .await
            .is_err()
    );
    let handle = state.registry.handle(store.instance()).unwrap();
    let first = handle
        .capture_generation(Timestamp::from_unix_millis(3000).unwrap())
        .await
        .unwrap()
        .0;
    let second = handle
        .capture_generation(Timestamp::from_unix_millis(4000).unwrap())
        .await
        .unwrap()
        .0;
    assert_eq!(first, second);
    store.fail.store(false, Ordering::Release);
    assert!(
        service
            .reap(
                &state.registry,
                store.instance(),
                Timestamp::from_unix_millis(5000).unwrap()
            )
            .await
            .unwrap()
            .is_some()
    );
    assert!(!state.registry.contains(store.instance()));
}

#[test]
fn phase4_operator_classifications_preserve_ambiguity_and_boundaries() {
    use crate::checkpoint_reconciliation::{
        BaselineBoundary, LegacyObservation as O, classify_legacy,
    };
    let store = Store::new();
    let row = store.row();
    let checkpoint = Checkpoint::new(
        store.instance(),
        Revision::INITIAL,
        vec![row.clone()],
        Timestamp::from_unix_millis(1000).unwrap(),
    );
    assert_eq!(
        classify_legacy(None, &[], &[], false),
        vec![O::Empty, O::UnknownHistory]
    );
    assert_eq!(
        classify_legacy(Some(&checkpoint), std::slice::from_ref(&row), &[], false),
        vec![O::Matching, O::UnknownHistory]
    );
    assert!(
        classify_legacy(None, std::slice::from_ref(&row), &[], false)
            .contains(&O::RowOnly(row.id()))
    );
    let result = classify_legacy(Some(&checkpoint), &[], &[row.id()], false);
    assert!(
        result.contains(&O::CheckpointOnly(row.id()))
            && result.contains(&O::Delete(row.id()))
            && result.contains(&O::UnknownHistory)
    );
    let mut boundary = BaselineBoundary {
        last_old_commit: 0,
        cutoff: 86_399_999,
        now: 86_400_000,
        sessions_invalidated: true,
        report: "explicit chosen values and loss acknowledgement".into(),
    };
    assert!(boundary.validate().is_err());
    boundary.cutoff = 86_400_000;
    assert!(boundary.validate().is_ok());
    boundary.sessions_invalidated = false;
    assert!(boundary.validate().is_err());
    assert_eq!(checkpoint.entities, vec![row]);
    let row = &checkpoint.entities[0];
    let newer = Entity::from_persisted(
        row.id(),
        row.instance_id(),
        row.kind(),
        row.owner(),
        row.transform(),
        row.visibility().clone(),
        Revision::from_u64(row.revision().as_u64() + 1),
        row.created_at(),
        row.updated_at(),
        row.components().clone(),
    )
    .unwrap();
    assert!(
        classify_legacy(Some(&checkpoint), std::slice::from_ref(&newer), &[], false)
            .contains(&O::RowsAhead(row.id()))
    );
    let ahead = Checkpoint::new(
        store.instance(),
        newer.revision(),
        vec![newer],
        checkpoint.timestamp,
    );
    assert!(
        classify_legacy(Some(&ahead), std::slice::from_ref(row), &[], false)
            .contains(&O::CheckpointAhead(row.id()))
    );
}

#[test]
fn phase4_default_off_and_incomplete_opt_in_refused() {
    let config = orbisync_config::Config::default();
    assert!(!config.world.checkpoint_generation_enabled);
    let env = orbisync_config::MapEnv::from_pairs([(
        "ORBISYNC_WORLD_CHECKPOINT_GENERATION_ENABLED",
        "true",
    )]);
    assert!(orbisync_config::Config::load(None, &env, &[]).is_err());
    let _actor_type: Option<InstanceActor> = None;
}

#[tokio::test]
async fn phase4_conversion_uncertainty_pins_identity_and_blocks_new_factory() {
    use orbisync_application::checkpoint_admission::ReconciledAuthority;
    let store = Store::new();
    let service = store.service();
    let checkpoint = store.checkpoint.lock().unwrap().clone();
    let approval = ReconciledAuthority {
        source_id: None,
        source_digest: None,
        report: "operator proven new empty instance".into(),
    };
    store.fail.store(true, Ordering::Release);
    assert!(
        service
            .convert(approval.clone(), move || async move { Ok(checkpoint) })
            .await
            .is_err()
    );
    assert_eq!(service.snapshots.available_permits(), 1);
    let called = Arc::new(AtomicBool::new(false));
    let flag = called.clone();
    assert!(
        service
            .convert(approval, move || async move {
                flag.store(true, Ordering::Release);
                Err(unavailable())
            })
            .await
            .is_err()
    );
    assert!(!called.load(Ordering::Acquire));
    store.fail.store(false, Ordering::Release);
    service.retry_conversion().await.unwrap();
    let attempts = store.conversions.lock().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    assert_eq!(service.snapshots.available_permits(), 2);
}

#[tokio::test]
async fn phase4_lower_total_refuses_selected_head_without_fallback_or_discard() {
    let mut store = Store::new();
    Arc::get_mut(&mut store).unwrap().limits = CheckpointLimits::new(16384, 2097152).unwrap();
    let preserved = store.checkpoint.lock().unwrap().clone();
    store.declared_total.store(2097153, Ordering::Release);
    let state = state(store.service());
    assert!(
        state
            .ensure_instance_activated(store.instance())
            .await
            .is_err()
    );
    assert!(!state.registry.contains(store.instance()));
    assert_eq!(store.selects.load(Ordering::Acquire), 1);
    assert_eq!(*store.checkpoint.lock().unwrap(), preserved);
    assert_eq!(store.head.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn phase4_legacy_factories_are_serial_and_share_global_jobs() {
    let store = Store::new();
    let service = store.service();
    let entered = Arc::new(AtomicUsize::new(0));
    let pause = Arc::new(Semaphore::new(0));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let service = service.clone();
        let entered = entered.clone();
        let pause = pause.clone();
        tasks.push(tokio::spawn(async move {
            service
                .inspect_legacy(move || async move {
                    entered.fetch_add(1, Ordering::Release);
                    pause.acquire().await.unwrap().forget();
                    Ok(())
                })
                .await
        }));
    }
    while entered.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(entered.load(Ordering::Acquire), 1);
    assert_eq!(service.jobs.available_permits(), 0);
    pause.add_permits(2);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(entered.load(Ordering::Acquire), 2);
    service.drain().await;
    assert_eq!(service.jobs.available_permits(), 2);
}

#[tokio::test]
async fn phase4_publication_waits_for_exact_receipt_commit_and_runs_once() {
    use orbisync_domain::{CommandId, UserId};
    use orbisync_world_runtime::command::{InstanceCommand, WorldPermissions};
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let command_id = CommandId::generate();
    let entity = EntityId::generate();
    let owner = UserId::generate();
    let command = InstanceCommand::SpawnEntity {
        command_id: Some(command_id),
        entity_id: entity,
        kind: EntityKind::Object,
        owner: Some(owner),
        transform: None,
        visibility: VisibilityPolicy::Global,
        requester: owner,
        permissions: WorldPermissions::all(),
    };
    let template = orbisync_protocol::v1::EntityCommand {
        command_id: command_id.to_string(),
        entity_id: entity.to_string(),
        operation: "spawn".into(),
        expected_revision: 0,
        arguments: None,
        instance_revision: None,
    };
    let result = crate::checkpoint_admission::submit_generation_command(
        &state.registry,
        store.instance(),
        command,
        template,
        [1; 32],
        uuid::Uuid::now_v7(),
        1000,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        result.phase,
        orbisync_application::checkpoint_admission::AdmissionPhase::Completed
    );
    let published = Arc::new(AtomicUsize::new(0));
    let count = published.clone();
    service.retain_publication(store.instance(), command_id.to_string(), move || {
        count.fetch_add(1, Ordering::Release);
    });
    store.fail.store(true, Ordering::Release);
    let handle = state.registry.handle(store.instance()).unwrap();
    assert!(
        service
            .persist(handle.clone(), Timestamp::from_unix_millis(1000).unwrap())
            .await
            .is_err()
    );
    assert_eq!(published.load(Ordering::Acquire), 0);
    store.fail.store(false, Ordering::Release);
    service
        .persist(handle.clone(), Timestamp::from_unix_millis(2000).unwrap())
        .await
        .unwrap();
    service
        .persist(handle.clone(), Timestamp::from_unix_millis(3000).unwrap())
        .await
        .unwrap();
    assert_eq!(published.load(Ordering::Acquire), 1);
    service
        .persist(
            handle.clone(),
            Timestamp::from_unix_millis(86_402_000).unwrap(),
        )
        .await
        .unwrap();
    let (_, snapshot) = handle
        .capture_generation(Timestamp::from_unix_millis(86_402_001).unwrap())
        .await
        .unwrap();
    assert!(
        snapshot.dedup.is_empty(),
        "periodic persistence retires only expired durable receipts"
    );
    assert_eq!(snapshot.entities.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn deadline78_owned_waits_and_cleanup() {
    let store = Store::new();
    let service = store.service();
    let cleanup = Arc::new(Semaphore::new(0));
    let witness = Arc::new(());
    let weak = Arc::downgrade(&witness);
    let mut callers = Vec::new();
    for _ in 0..2 {
        let service = service.clone();
        let cleanup = cleanup.clone();
        let witness = witness.clone();
        callers.push(tokio::spawn(async move {
            service
                .owned(async move {
                    let _witness = witness;
                    cleanup.acquire().await.unwrap().forget();
                    Ok(())
                })
                .await
        }));
    }
    drop(witness);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    for caller in callers {
        assert!(caller.await.unwrap().is_err());
    }
    assert!(weak.upgrade().is_some());
    assert_eq!(service.jobs.available_permits(), 0);
    assert!(service.owned(async { Ok(()) }).await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), service.drain())
            .await
            .is_err()
    );
    assert!(weak.upgrade().is_some());
    assert!(service.owned(async { Ok(()) }).await.is_err());
    cleanup.add_permits(2);
    service.drain().await;
    service.drain().await;
    assert!(weak.upgrade().is_none());
    assert_eq!(service.jobs.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn deadline78_expired_conversion_wait_never_calls_factory() {
    let store = Store::new();
    let service = store.service();
    let permit = service.conversions.acquire().await.unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    assert!(
        service
            .inspect_legacy(move || async move {
                observed.store(true, Ordering::Release);
                Ok(())
            })
            .await
            .is_err()
    );
    drop(permit);
    service.drain().await;
    assert!(!called.load(Ordering::Acquire));
}

#[tokio::test(start_paused = true)]
async fn deadline78_inventory_operator_budget() {
    struct DelayedApproval {
        entered: tokio::sync::Notify,
        release: Semaphore,
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl crate::checkpoint_operator::Approval for DelayedApproval {
        async fn approve(
            &self,
            _: InstanceId,
            _: &orbisync_application::checkpoint_admission::ReconciledAuthority,
            _: orbisync_storage_postgres::ReconciliationDecision,
        ) -> Result<(), ApplicationError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            Ok(())
        }
    }
    let store = Store::new();
    let service = store.service();
    let mut artifact = crate::checkpoint_operator::inspect(&service, store.instance())
        .await
        .unwrap();
    artifact["chosen_checkpoint"] =
        serde_json::from_slice(&store.checkpoint.lock().unwrap().to_json_bytes().unwrap()).unwrap();
    artifact["decision"] = "new_empty".into();
    artifact["never_admitted"] = true.into();
    artifact["evidence"] = "reviewed creation records".into();
    let operator = Arc::new(DelayedApproval {
        entered: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    let caller = tokio::spawn({
        let service = service.clone();
        let operator = operator.clone();
        async move {
            let digest = review_digest(&artifact);
            crate::checkpoint_operator::apply(&service, operator, artifact, &digest, 1000).await
        }
    });
    operator.entered.notified().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(caller.await.unwrap().is_err());
    assert_eq!(service.operator_status(), Some("approval_pending"));
    assert_eq!(service.jobs.available_permits(), 1);
    assert!(store.conversions.lock().unwrap().is_empty());
    operator.release.add_permits(1);
    service.drain().await;
    assert_eq!(operator.calls.load(Ordering::Acquire), 1);
    assert!(store.conversions.lock().unwrap().is_empty());
    assert_eq!(
        service.operator_status(),
        Some("approved_conversion_not_started")
    );
}

#[tokio::test(start_paused = true)]
async fn deadline78_project_cleanup_budget() {
    let store = Store::new();
    let service = store.service();
    store.block.store(true, Ordering::Release);
    let caller = tokio::spawn({
        let service = service.clone();
        let instance = store.instance();
        async move { service.project(instance).await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(caller.await.unwrap().is_err());
    assert_eq!(service.jobs.available_permits(), 1);
    store.pause.add_permits(1);
    service.drain().await;
    assert_eq!(store.applied.load(Ordering::Acquire), 0);
    assert_eq!(service.jobs.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn deadline78_commit_ack_retains_same_actor_attempt() {
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let handle = state.registry.handle(store.instance()).unwrap();
    let delay = Arc::new(PublicationDelay {
        started: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
        committed: Mutex::new(None),
        mutations: AtomicUsize::new(0),
    });
    *store.publication_delay.lock().unwrap() = Some(delay.clone());
    store.fail.store(true, Ordering::Release);
    let caller = tokio::spawn({
        let service = service.clone();
        let handle = handle.clone();
        async move {
            service
                .persist(handle, Timestamp::from_unix_millis(2000).unwrap())
                .await
        }
    });
    delay.started.notified().await;
    let original = store.publications.lock().unwrap()[0].clone();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(caller.await.unwrap().is_err());
    assert_eq!(service.jobs.available_permits(), 1);
    assert_eq!(service.snapshots.available_permits(), 1);
    let pinned = handle
        .capture_generation(Timestamp::from_unix_millis(9000).unwrap())
        .await
        .unwrap()
        .0;
    assert_eq!(pinned, original);
    delay.release.add_permits(1);
    while service.jobs.available_permits() != 2 {
        tokio::task::yield_now().await;
    }
    store.fail.store(false, Ordering::Release);
    service
        .persist(handle, Timestamp::from_unix_millis(10000).unwrap())
        .await
        .unwrap();
    assert_eq!(
        store.publications.lock().unwrap().as_slice(),
        &[original.clone(), original]
    );
    assert_eq!(delay.mutations.load(Ordering::Acquire), 1);
    assert_eq!(service.snapshots.available_permits(), 2);
    service.drain().await;
}

#[tokio::test(start_paused = true)]
async fn deadline78_expired_actor_request_does_not_pin_capture() {
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let handle = state.registry.handle(store.instance()).unwrap();
    assert!(
        handle
            .capture_generation_before(
                Timestamp::from_unix_millis(2000).unwrap(),
                Some(tokio::time::Instant::now()),
                None,
            )
            .await
            .is_err()
    );
    let attempt = handle
        .capture_generation(Timestamp::from_unix_millis(3000).unwrap())
        .await
        .unwrap()
        .0;
    assert_eq!(attempt.completed_at_millis, 3000);
    service.drain().await;
}

#[tokio::test(start_paused = true)]
async fn deadline78_blocking_worker_retains_permits_after_observation() {
    let service = Store::new().service();
    let witness = Arc::new(());
    let weak = Arc::downgrade(&witness);
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let caller = tokio::spawn({
        let service = service.clone();
        async move {
            service
                .inspect_legacy(move || async move {
                    tokio::task::spawn_blocking(move || {
                        let _witness = witness;
                        entered.send(()).unwrap();
                        blocked.recv().unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap()
                })
                .await
        }
    });
    started.await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(caller.await.unwrap().is_err());
    assert!(weak.upgrade().is_some());
    assert_eq!(service.jobs.available_permits(), 1);
    assert_eq!(service.conversions.available_permits(), 0);
    release.send(()).unwrap();
    service.drain().await;
    assert!(weak.upgrade().is_none());
    assert_eq!(service.jobs.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn deadline78_cursor_and_instance_waits_cannot_start_late_work() {
    let store = Store::new();
    let service = store.service();
    let state = state(service.clone());
    drop(
        state
            .ensure_instance_activated(store.instance())
            .await
            .unwrap(),
    );
    let handle = state.registry.handle(store.instance()).unwrap();
    let cursor = service.projection_cursor.lock().await;
    let instance = service.instance_guard(store.instance()).await;
    let started = tokio::time::Instant::now();
    let (page, persist) = tokio::join!(
        service.projection_page(),
        service.persist(handle, Timestamp::from_unix_millis(2000).unwrap())
    );
    assert!(page.is_err() && persist.is_err());
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    drop(cursor);
    drop(instance);
    service.drain().await;
    assert!(store.publications.lock().unwrap().is_empty());
    assert_eq!(service.snapshots.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn deadline78_shutdown_refuses_execution_of_queued_registration() {
    let service = Store::new().service();
    let permits = service.jobs.acquire_many(2).await.unwrap();
    let invoked = Arc::new(AtomicBool::new(false));
    let caller = tokio::spawn({
        let service = service.clone();
        let invoked = invoked.clone();
        async move {
            service
                .owned(async move {
                    invoked.store(true, Ordering::Release);
                    Ok(())
                })
                .await
        }
    });
    tokio::task::yield_now().await;
    service.close_admission();
    service.cancel_execution();
    drop(permits);
    assert!(caller.await.unwrap().is_err());
    service.drain().await;
    assert!(!invoked.load(Ordering::Acquire));
    assert_eq!(service.jobs.available_permits(), 2);
}
