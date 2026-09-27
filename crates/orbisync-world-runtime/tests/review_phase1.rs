//! Independent review repros for exact 96ff6fb8ece70a15e25006e6cc3b913abeea072f.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use orbisync_application::{ApplicationError, checkpoint_admission::*};
use orbisync_domain::{
    CommandId, EntityId, EntityKind, InstanceId, Timestamp, UserId, VisibilityPolicy,
};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeRegistry,
    actor::{GenerationBinding, InstanceActor},
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
};
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Debug)]
struct Store;
#[async_trait::async_trait]
impl GenerationStore for Store {
    fn limits(&self) -> orbisync_application::CheckpointLimits {
        Default::default()
    }
    async fn publish(
        &self,
        _: &GenerationAttempt,
        _: &mut dyn GenerationSource,
    ) -> Result<GenerationResolution, ApplicationError> {
        Ok(GenerationResolution::Uncertain)
    }
    async fn resolve(
        &self,
        _: &GenerationAttempt,
    ) -> Result<GenerationResolution, ApplicationError> {
        Ok(GenerationResolution::Uncertain)
    }
}
#[derive(Debug)]
struct Encoder;
impl PreparedOutcomeEncoder for Encoder {
    fn encode(
        &self,
        _: ProspectiveOutcome<'_>,
        _: &AtomicBool,
    ) -> Result<Vec<u8>, ApplicationError> {
        Ok(vec![1])
    }
}
fn actor() -> InstanceActor {
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor::starting(InstanceId::generate()));
    actor.start();
    actor
        .enable_generation_admission(
            Some(GenerationBinding {
                store: Arc::new(Store),
                head: 0,
                writer: WriterPermit::new(WriterToken {
                    epoch: 1,
                    boot: CommandId::generate().as_uuid(),
                })
                .unwrap(),
            }),
            8 * 1024 * 1024,
            vec![],
        )
        .unwrap();
    actor
}
fn now() -> Timestamp {
    Timestamp::from_unix_millis(0).unwrap()
}
fn request() -> AdmissionRequest {
    AdmissionRequest {
        command_id: CommandId::generate(),
        fingerprint: [17; 32],
        message_id: CommandId::generate().as_uuid(),
        now_millis: 0,
        cancelled: Arc::new(AtomicBool::new(false)),
        encoder: Arc::new(Encoder),
    }
}
#[tokio::test]
async fn review_committed_task_effects_are_delivered_on_tick() {
    let registry = RuntimeRegistry::new();
    let handle = registry.ensure_instance(actor());
    let owner = UserId::generate();
    let result = handle
        .submit_prepared(
            InstanceCommand::SpawnEntity {
                command_id: None,
                entity_id: EntityId::generate(),
                kind: EntityKind::Avatar,
                owner: Some(owner),
                transform: None,
                visibility: VisibilityPolicy::Global,
                requester: owner,
                permissions: WorldPermissions::all(),
            },
            request(),
        )
        .await
        .unwrap();
    assert!(matches!(result.outcome, CommandOutcome::Applied { .. }));
    let (attempt, _) = handle.capture_generation(now()).await.unwrap();
    handle
        .resolve_generation(
            attempt,
            GenerationResolution::Committed { publish_seq: 1 },
            0,
        )
        .await
        .unwrap();
    let tick = handle.tick(now(), false).await.unwrap();
    assert!(
        !tick.events.is_empty(),
        "committed spawn extension event stays withheld after successful resolution"
    );
    assert!(!tick.persistence_events.is_empty());
}
#[test]
fn review_expired_commit_is_not_advertised_as_replayable() {
    let mut actor = actor();
    let mut request = request();
    let result = actor.submit_prepared(InstanceCommand::Tick, request.clone());
    assert!(matches!(result.outcome, CommandOutcome::Applied { .. }));
    let (attempt, _) = actor.capture_generation(now()).unwrap();
    assert!(
        actor
            .resolve_generation(
                &attempt,
                GenerationResolution::Committed { publish_seq: 1 },
                86_400_000
            )
            .is_err()
    );
    request.now_millis = 86_400_000;
    let replay = actor.submit_prepared(InstanceCommand::Tick, request);
    assert!(
        !matches!(
            replay.outcome,
            CommandOutcome::Rejected {
                code: "COMMAND_REPLAY",
                ..
            }
        ),
        "expired committed receipt is advertised as durable replay: {replay:?}"
    );
}

fn spawn(entity_id: EntityId) -> InstanceCommand {
    let owner = UserId::generate();
    InstanceCommand::SpawnEntity {
        command_id: None,
        entity_id,
        kind: EntityKind::Avatar,
        owner: Some(owner),
        transform: None,
        visibility: VisibilityPolicy::Global,
        requester: owner,
        permissions: WorldPermissions::all(),
    }
}

#[tokio::test]
async fn committed_prefix_survives_later_dirty_effects_and_repeated_ticks() {
    use orbisync_application::{EntityPersistenceEvent, ExtensionEvent};
    let registry = RuntimeRegistry::new();
    let handle = registry.ensure_instance(actor());
    let first = EntityId::generate();
    let second = EntityId::generate();
    assert!(matches!(
        handle
            .submit_prepared(spawn(first), request())
            .await
            .unwrap()
            .outcome,
        CommandOutcome::Applied { .. }
    ));
    let premature = handle.tick(now(), false).await.unwrap();
    assert!(premature.events.is_empty());
    assert!(premature.persistence_events.is_empty());
    let (attempt, _) = handle.capture_generation(now()).await.unwrap();
    handle
        .resolve_generation(
            attempt,
            GenerationResolution::Committed { publish_seq: 1 },
            0,
        )
        .await
        .unwrap();
    assert!(matches!(
        handle
            .submit_prepared(spawn(second), request())
            .await
            .unwrap()
            .outcome,
        CommandOutcome::Applied { .. }
    ));
    // Pin the next generation too: its unresolved effects must not block the
    // committed prefix or leak through that prefix's delivery.
    let (attempt, _) = handle.capture_generation(now()).await.unwrap();
    let tick = handle.tick(now(), false).await.unwrap();
    let spawned: Vec<_> = tick
        .events
        .iter()
        .filter_map(|event| match event {
            ExtensionEvent::EntitySpawned { entity_id, .. } => Some(*entity_id),
            _ => None,
        })
        .collect();
    assert_eq!(spawned, vec![first]);
    assert!(
        matches!(tick.persistence_events.as_slice(), [EntityPersistenceEvent::Spawned(entity)] if entity.id() == first)
    );
    for _ in 0..3 {
        let tick = handle.tick(now(), false).await.unwrap();
        assert!(tick.events.is_empty());
        assert!(tick.persistence_events.is_empty());
    }
    handle
        .resolve_generation(
            attempt,
            GenerationResolution::Committed { publish_seq: 2 },
            0,
        )
        .await
        .unwrap();
    let tick = handle.tick(now(), false).await.unwrap();
    assert!(
        matches!(tick.events.as_slice(), [ExtensionEvent::EntitySpawned { entity_id, .. }] if *entity_id == second)
    );
    assert!(
        matches!(tick.persistence_events.as_slice(), [EntityPersistenceEvent::Spawned(entity)] if entity.id() == second)
    );
    let tick = handle.tick(now(), false).await.unwrap();
    assert!(tick.events.is_empty());
    assert!(tick.persistence_events.is_empty());
}

#[tokio::test]
async fn committed_task_cycles_do_not_exhaust_outboxes() {
    let registry = RuntimeRegistry::new();
    let handle = registry.ensure_instance(actor());
    // More event-producing commits than the default bounded outbox capacity.
    for publish_seq in 1..=300 {
        assert!(matches!(
            handle
                .submit_prepared(spawn(EntityId::generate()), request())
                .await
                .unwrap()
                .outcome,
            CommandOutcome::Applied { .. }
        ));
        let (attempt, _) = handle.capture_generation(now()).await.unwrap();
        handle
            .resolve_generation(attempt, GenerationResolution::Committed { publish_seq }, 0)
            .await
            .unwrap();
        let tick = handle.tick(now(), false).await.unwrap();
        assert_eq!(tick.persistence_events.len(), 1);
        assert_eq!(tick.events.len(), 1);
    }
}

#[test]
fn durable_replay_expiry_boundary_is_checked_without_periodic_cleanup() {
    const EXPIRY: i64 = 86_400_000;
    for resolution_time in [0, EXPIRY - 1, EXPIRY, EXPIRY + 1] {
        let mut actor = actor();
        let mut request = request();
        let command = spawn(EntityId::generate());
        let result = actor.submit_prepared(command.clone(), request.clone());
        assert!(matches!(result.outcome, CommandOutcome::Applied { .. }));
        let (attempt, snapshot) = actor.capture_generation(now()).unwrap();
        assert_eq!(
            actor
                .resolve_generation(
                    &attempt,
                    GenerationResolution::Committed { publish_seq: 1 },
                    resolution_time
                )
                .is_ok(),
            resolution_time < EXPIRY
        );
        let revision = actor.descriptor().revision;
        let bytes = actor.checkpoint_capacity_bytes();
        if resolution_time < EXPIRY {
            actor.expire_generation_receipts(EXPIRY - 1);
            request.now_millis = EXPIRY - 1;
            let replay = actor.submit_prepared(command.clone(), request.clone());
            assert_eq!(replay.phase, AdmissionPhase::Replay);
            assert!(matches!(
                replay.outcome,
                CommandOutcome::Rejected {
                    code: "COMMAND_REPLAY",
                    ..
                }
            ));
            assert_eq!(replay.receipt.as_ref(), snapshot.dedup.first());
        }
        for duplicate_time in [EXPIRY, EXPIRY, EXPIRY + 1] {
            request.now_millis = duplicate_time;
            let expired = actor.submit_prepared(command.clone(), request.clone());
            assert_eq!(expired.phase, AdmissionPhase::Expired);
            assert!(matches!(
                expired.outcome,
                CommandOutcome::Rejected {
                    code: "REPLAY_WINDOW_EXPIRED",
                    ..
                }
            ));
            assert!(
                expired.receipt.is_none(),
                "expired result must not expose replay bytes"
            );
            assert_eq!(actor.descriptor().revision, revision);
            assert_eq!(actor.checkpoint_capacity_bytes(), bytes);
        }
        let (_, later) = actor
            .capture_generation(Timestamp::from_unix_millis(EXPIRY + 1).unwrap())
            .unwrap();
        assert_eq!(
            later.dedup, snapshot.dedup,
            "durable receipt times and result must not renew"
        );
    }
}
