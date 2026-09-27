#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use orbisync_domain::{CommandId, EntityKind, InstanceId, Transform};

// Explicit contract fixture. It never claims persistence or returns success.
#[derive(Debug)]
struct UnavailableGenerationStore;
#[async_trait::async_trait]
impl GenerationStore for UnavailableGenerationStore {
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
struct Encoder {
    bytes: usize,
    cancel: bool,
    revoke: Option<WriterPermit>,
}
impl PreparedOutcomeEncoder for Encoder {
    fn encode(
        &self,
        _: ProspectiveOutcome<'_>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, ApplicationError> {
        if self.cancel {
            cancelled.store(true, Ordering::Release);
        }
        if let Some(permit) = &self.revoke {
            permit.invalidate();
        }
        Ok(vec![255; self.bytes])
    }
}
fn request(bytes: usize) -> AdmissionRequest {
    AdmissionRequest {
        command_id: CommandId::generate(),
        fingerprint: [17; 32],
        message_id: CommandId::generate().as_uuid(),
        now_millis: 0,
        cancelled: Arc::new(AtomicBool::new(false)),
        encoder: Arc::new(Encoder {
            bytes,
            cancel: false,
            revoke: None,
        }),
    }
}
fn binding() -> GenerationBinding {
    GenerationBinding {
        store: Arc::new(UnavailableGenerationStore),
        head: 0,
        writer: WriterPermit::new(WriterToken {
            epoch: 1,
            boot: CommandId::generate().as_uuid(),
        })
        .unwrap(),
    }
}
fn actor(limit: usize) -> InstanceActor {
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor::starting(InstanceId::generate()));
    actor.start();
    actor
        .enable_generation_admission_inner(
            Some(binding()),
            orbisync_application::CheckpointLimits::default(),
            limit,
            Vec::new(),
        )
        .unwrap();
    actor
}
fn spawn(id: EntityId, owner: UserId) -> InstanceCommand {
    InstanceCommand::SpawnEntity {
        command_id: None,
        entity_id: id,
        kind: EntityKind::Avatar,
        owner: Some(owner),
        transform: Some(Transform::identity()),
        visibility: VisibilityPolicy::Global,
        requester: owner,
        permissions: WorldPermissions::all(),
    }
}
fn now() -> Timestamp {
    Timestamp::from_unix_millis(0).unwrap()
}
fn fixture() -> (InstanceActor, EntityId, UserId, PresenceId) {
    let mut actor = actor(8 * 1024 * 1024);
    let id = EntityId::generate();
    let owner = UserId::generate();
    let presence = PresenceId::generate();
    assert!(matches!(
        actor.handle(spawn(id, owner)),
        CommandOutcome::Applied { .. }
    ));
    assert!(matches!(
        actor.handle(InstanceCommand::Join {
            presence_id: presence,
            user_id: owner,
            instance_id: actor.descriptor.instance_id,
            capacity: 100
        }),
        CommandOutcome::Applied { .. }
    ));
    (actor, id, owner, presence)
}
fn footprint(a: &InstanceActor) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        a.state,
        a.descriptor,
        a.history,
        a.component_update_tokens,
        a.component_update_last_refill,
        a.outbox,
        a.persistence_outbox
    )
}
fn ingress(
    a: &InstanceActor,
    id: EntityId,
    owner: UserId,
    presence: PresenceId,
) -> Vec<InstanceCommand> {
    let rev = a.state.get_entity(id).unwrap().revision();
    let update = InstanceCommand::UpdateEntityComponent {
        command_id: None,
        entity_id: id,
        component_key: "example.data".into(),
        payload_bytes: vec![255; 4096],
        expected_revision: rev,
        now: now(),
        requester: owner,
        permissions: WorldPermissions::all(),
    };
    vec![
        spawn(EntityId::generate(), owner),
        update.clone(),
        InstanceCommand::DeleteEntity {
            command_id: None,
            entity_id: id,
            expected_revision: rev,
            requester: owner,
            permissions: WorldPermissions::all(),
        },
        InstanceCommand::TransferOwnership {
            command_id: None,
            entity_id: id,
            new_owner: None,
            expected_revision: rev,
            now: now(),
            requester: owner,
            permissions: WorldPermissions::all(),
        },
        InstanceCommand::UpdateTransform {
            entity_id: id,
            transform: Transform::identity(),
            expected_revision: rev,
            user_id: owner,
            now: now(),
            permissions: WorldPermissions::all(),
        },
        InstanceCommand::UpdateTransform {
            entity_id: EntityId::generate(),
            transform: Transform::identity(),
            expected_revision: Revision::INITIAL,
            user_id: owner,
            now: now(),
            permissions: WorldPermissions::all(),
        },
        InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: owner,
            instance_id: a.descriptor.instance_id,
            capacity: 100,
        },
        InstanceCommand::Leave {
            presence_id: presence,
        },
        InstanceCommand::Tick,
        InstanceCommand::Shutdown,
        InstanceCommand::WithExpectedEntityState {
            entity_id: id,
            expected: a.entity_snapshot(id).map(Box::new),
            command: Box::new(update),
        },
    ]
}

#[test]
fn missing_adapter_is_unavailable_without_switching_legacy() {
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor::starting(InstanceId::generate()));
    actor.start();
    assert!(
        actor
            .enable_generation_admission(None, 8 * 1024 * 1024, Vec::new())
            .is_err()
    );
    assert!(!actor.uses_generation_admission());
    let before = footprint(&actor);
    assert_eq!(
        actor
            .submit_prepared(InstanceCommand::Tick, request(1))
            .phase,
        AdmissionPhase::NotAdmitted
    );
    assert_eq!(before, footprint(&actor));
    assert!(matches!(
        actor.handle(InstanceCommand::Tick),
        CommandOutcome::Applied { .. }
    ));
}

#[test]
fn phase4_baseline_cutoff_rejects_old_ids_before_any_effect() {
    let mut actor = actor(8 * 1024 * 1024);
    let request = request(1);
    let issued = request
        .command_id
        .as_uuid()
        .get_timestamp()
        .unwrap()
        .to_unix();
    let issued = (issued.0 * 1000 + u64::from(issued.1 / 1_000_000)) as i64;
    actor.set_generation_cutoff(Some(issued + 1)).unwrap();
    let before = footprint(&actor);
    let command = spawn(EntityId::generate(), UserId::generate());
    let result = actor.submit_prepared(command.clone(), request.clone());
    assert_eq!(result.phase, AdmissionPhase::NotAdmitted);
    assert!(matches!(
        result.outcome,
        CommandOutcome::Rejected {
            code: "COMMAND_BEFORE_BASELINE",
            ..
        }
    ));
    assert_eq!(footprint(&actor), before);
    actor.set_generation_cutoff(Some(issued)).unwrap();
    assert_eq!(
        actor.submit_prepared(command, request).phase,
        AdmissionPhase::Completed
    );
}

#[test]
fn every_ingress_refuses_oversized_response_without_mutation_effects() {
    let (mut actor, id, owner, presence) = fixture();
    for command in ingress(&actor, id, owner, presence) {
        let before = footprint(&actor);
        let result = actor.submit_prepared(command, request(RESPONSE_BYTES + 1));
        assert!(
            matches!(
                result.outcome,
                CommandOutcome::Rejected {
                    code: CAPACITY_CODE,
                    ..
                }
            ),
            "{result:?}"
        );
        assert_eq!(result.phase, AdmissionPhase::Completed);
        assert_eq!(before, footprint(&actor));
        assert!(
            matches!(result.receipt.unwrap().result, CheckpointDedupResult::Rejected { code, detail }
            if code == CAPACITY_CODE && detail == CAPACITY_DETAIL)
        );
    }
    assert_eq!(actor.admission.as_ref().unwrap().receipts.len(), 11);
}

#[test]
fn exact_ledger_boundary_and_one_byte_less_include_transform_autocreate() {
    let owner = UserId::generate();
    let id = EntityId::generate();
    let mut reference = actor(8 * 1024 * 1024);
    let command = spawn(id, owner);
    assert!(matches!(
        reference.handle(command.clone()),
        CommandOutcome::Applied { .. }
    ));
    let bound = reference.checkpoint_capacity_bytes().unwrap();
    let mut fits = actor(bound);
    assert!(matches!(
        fits.handle(command.clone()),
        CommandOutcome::Applied { .. }
    ));
    assert_eq!(fits.checkpoint_capacity_bytes(), Some(bound));
    assert!(fits.build_checkpoint(now()).to_json_bytes().unwrap().len() <= bound);
    let mut short = actor(bound - 1);
    let before = footprint(&short);
    assert!(matches!(
        short.submit(command).unwrap(),
        CommandOutcome::Rejected {
            code: CAPACITY_CODE,
            ..
        }
    ));
    assert_eq!(before, footprint(&short));
    let mut empty = actor(HEADER);
    let before = footprint(&empty);
    let transform = InstanceCommand::UpdateTransform {
        entity_id: id,
        transform: Transform::identity(),
        expected_revision: Revision::INITIAL,
        user_id: owner,
        now: now(),
        permissions: WorldPermissions::all(),
    };
    assert!(matches!(
        empty.handle(transform),
        CommandOutcome::Rejected {
            code: CAPACITY_CODE,
            ..
        }
    ));
    assert_eq!(before, footprint(&empty));
}

#[test]
fn reservation_precedes_deletion_credit_and_releases_on_precommit_cancel() {
    let (mut actor, id, owner, _) = fixture();
    let rev = actor.entity_snapshot(id).unwrap().revision();
    let command = InstanceCommand::DeleteEntity {
        command_id: None,
        entity_id: id,
        expected_revision: rev,
        requester: owner,
        permissions: WorldPermissions::all(),
    };
    let bytes = actor.checkpoint_capacity_bytes().unwrap();
    actor.admission.as_mut().unwrap().limit = bytes;
    let before = footprint(&actor);
    let req = request(1);
    let result = actor.submit_prepared(command.clone(), req.clone());
    assert_eq!(result.phase, AdmissionPhase::NotAdmitted);
    assert!(result.receipt.is_none());
    assert_eq!(before, footprint(&actor));
    assert_eq!(actor.checkpoint_capacity_bytes(), Some(bytes));
    actor.admission.as_mut().unwrap().limit = 8 * 1024 * 1024;
    let mut cancelled = req.clone();
    cancelled.encoder = Arc::new(Encoder {
        bytes: 1,
        cancel: true,
        revoke: None,
    });
    assert_eq!(
        actor.submit_prepared(command.clone(), cancelled).phase,
        AdmissionPhase::NotAdmitted
    );
    assert_eq!(before, footprint(&actor));
    assert_eq!(actor.checkpoint_capacity_bytes(), Some(bytes));
    req.cancelled.store(false, Ordering::Release);
    assert!(matches!(
        actor.submit_prepared(command, req).outcome,
        CommandOutcome::Applied { .. }
    ));
    assert!(actor.entity_snapshot(id).is_none());
}

#[test]
fn dirty_cancel_uncertainty_absence_retirement_and_expiry_keep_identity() {
    let mut actor = actor(8 * 1024 * 1024);
    let req = request(8);
    let result =
        actor.submit_prepared(spawn(EntityId::generate(), UserId::generate()), req.clone());
    assert_eq!(result.phase, AdmissionPhase::Completed);
    req.cancelled.store(true, Ordering::Release); // postcommit caller loss
    actor.expire_generation_receipts(TTL + 1);
    assert_eq!(actor.admission.as_ref().unwrap().receipts.len(), 1);
    let (attempt, snapshot) = actor.capture_generation(now()).unwrap();
    assert!(actor.drain_outbox().is_empty());
    assert!(actor.drain_persistence_batch().is_empty());
    assert!(
        actor
            .resolve_generation(&attempt, GenerationResolution::Uncertain, 1)
            .is_err()
    );
    assert!(actor.retire_absent_generation(&attempt).is_err());
    let before = footprint(&actor);
    assert!(matches!(
        actor.handle(InstanceCommand::Tick),
        CommandOutcome::Rejected {
            code: "PERSISTENCE_UNAVAILABLE",
            ..
        }
    ));
    assert_eq!(before, footprint(&actor));
    let (retry, retry_snapshot) = actor
        .capture_generation(Timestamp::from_unix_millis(TTL + 1).unwrap())
        .unwrap();
    assert_eq!(retry, attempt);
    assert!(Arc::ptr_eq(&snapshot, &retry_snapshot));
    actor
        .resolve_generation(&attempt, GenerationResolution::ProvenAbsent, TTL + 1)
        .unwrap();
    actor.retire_absent_generation(&attempt).unwrap();
    let (second, second_snapshot) = actor
        .capture_generation(Timestamp::from_unix_millis(TTL + 1).unwrap())
        .unwrap();
    assert_ne!(second.generation_id, attempt.generation_id);
    assert_eq!(snapshot.dedup[0].result, second_snapshot.dedup[0].result);
    actor
        .resolve_generation(
            &second,
            GenerationResolution::Committed { publish_seq: 1 },
            TTL + 2,
        )
        .unwrap();
    assert!(!actor.drain_outbox().is_empty());
    actor.expire_generation_receipts(2 * TTL + 1);
    assert!(actor.admission.as_ref().unwrap().receipts.is_empty());
}

#[test]
fn completed_capacity_rejection_roundtrips_without_reapplication_contract_only() {
    let mut actor = actor(8 * 1024 * 1024);
    let req = request(RESPONSE_BYTES + 1);
    let command = spawn(EntityId::generate(), UserId::generate());
    let result = actor.submit_prepared(command.clone(), req.clone());
    let (attempt, checkpoint) = actor.capture_generation(now()).unwrap();
    actor
        .resolve_generation(
            &attempt,
            GenerationResolution::Committed { publish_seq: 1 },
            0,
        )
        .unwrap();
    let decoded = Checkpoint::from_json_bytes(&checkpoint.to_json_bytes().unwrap()).unwrap();
    let receipts = decoded.dedup.clone();
    let mut restored = InstanceActor::from_checkpoint_with_mailbox_config(
        decoded,
        30.0,
        256,
        50.0,
        10.0,
        MailboxConfig::default(),
        Arc::new(NoopMetrics),
    )
    .unwrap();
    let mut binding = binding();
    binding.head = 1;
    restored
        .enable_generation_admission(Some(binding), 8 * 1024 * 1024, receipts)
        .unwrap();
    restored.start();
    let before = footprint(&restored);
    let replay = restored.submit_prepared(command, req);
    assert_eq!(replay.phase, AdmissionPhase::Replay);
    assert_eq!(result.receipt, replay.receipt);
    assert_eq!(before, footprint(&restored));
}

#[test]
fn writer_recheck_and_bigint_overflow_preserve_all_ingress_effects() {
    let (mut actor, id, owner, presence) = fixture();
    let mut req = request(1);
    req.encoder = Arc::new(Encoder {
        bytes: 1,
        cancel: false,
        revoke: Some(actor.admission.as_ref().unwrap().binding.writer.clone()),
    });
    let before = footprint(&actor);
    assert_eq!(
        actor.submit_prepared(InstanceCommand::Tick, req).phase,
        AdmissionPhase::NotAdmitted
    );
    for command in ingress(&actor, id, owner, presence) {
        assert!(matches!(
            actor.submit(command).unwrap(),
            CommandOutcome::Rejected {
                code: "PERSISTENCE_UNAVAILABLE",
                ..
            }
        ));
        assert_eq!(before, footprint(&actor));
    }
    let (mut actor, id, owner, presence) = fixture();
    actor
        .state
        .install_revision(Revision::from_u64(i64::MAX as u64));
    actor.descriptor.revision = actor.state.revision();
    let before = footprint(&actor);
    for command in ingress(&actor, id, owner, presence) {
        assert!(matches!(
            actor.handle(command),
            CommandOutcome::Rejected {
                code: "REVISION_OVERFLOW",
                ..
            }
        ));
        assert_eq!(before, footprint(&actor));
    }
}

#[tokio::test]
async fn registry_task_and_direct_adapters_share_admission_and_block_legacy_save_reap() {
    for task in [false, true] {
        let actor = actor(HEADER);
        let id = actor.descriptor.instance_id;
        let registry = crate::RuntimeRegistry::new();
        if task {
            registry.ensure_instance(actor);
        } else {
            registry.insert(id, actor).unwrap();
        }
        let before = registry.read_snapshot(id).await.unwrap();
        let result = registry
            .submit_prepared(
                id,
                spawn(EntityId::generate(), UserId::generate()),
                request(1),
            )
            .await
            .unwrap();
        assert_eq!(result.phase, AdmissionPhase::NotAdmitted);
        assert!(registry.build_checkpoint(id, now()).await.is_none());
        assert!(matches!(
            registry
                .submit(id, spawn(EntityId::generate(), UserId::generate()))
                .await
                .unwrap(),
            CommandOutcome::Rejected {
                code: CAPACITY_CODE,
                ..
            }
        ));
        let after = registry.read_snapshot(id).await.unwrap();
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.entities, after.entities);
        if let Some(handle) = registry.handle(id) {
            assert!(handle.reap_if_idle(now()).await.is_none());
            let (attempt, _) = handle.capture_generation(now()).await.unwrap();
            assert!(handle.tick(now(), true).await.unwrap().checkpoint.is_none());
            assert!(
                handle
                    .resolve_generation(attempt, GenerationResolution::Uncertain, 0)
                    .await
                    .is_err()
            );
        }
    }
}

#[test]
fn actual_encoding_stays_below_ledger_with_binary_expansion_and_numeric_growth() {
    let (mut actor, id, owner, _) = fixture();
    actor.component_updates_per_sec = 1000;
    for index in 0..16 {
        let rev = actor.entity_snapshot(id).unwrap().revision();
        let result = actor.submit_prepared(
            InstanceCommand::UpdateEntityComponent {
                command_id: None,
                entity_id: id,
                component_key: format!("example.data{index}"),
                payload_bytes: vec![255; 4096],
                expected_revision: rev,
                now: now(),
                requester: owner,
                permissions: WorldPermissions::all(),
            },
            request(256),
        );
        assert!(matches!(result.outcome, CommandOutcome::Applied { .. }));
        assert!(
            actor.build_checkpoint(now()).to_json_bytes().unwrap().len()
                <= actor.checkpoint_capacity_bytes().unwrap()
        );
    }
}

fn retained_entry(response_bytes: usize) -> CheckpointDedupEntry {
    CheckpointDedupEntry {
        command_id: CommandId::generate().to_string(),
        fingerprint: vec![255; 32],
        message_id: CommandId::generate().to_string(),
        created_at_millis: 0,
        expires_at_millis: TTL,
        result: if response_bytes == 0 {
            CheckpointDedupResult::Rejected {
                code: CAPACITY_CODE.into(),
                detail: CAPACITY_DETAIL.into(),
            }
        } else {
            CheckpointDedupResult::Applied {
                response_payload: vec![255; response_bytes],
            }
        },
    }
}
#[test]
fn record_count_and_logical_aggregate_saturation_do_not_evict_unexpired_receipts() {
    for receipts in [
        (0..4096).map(|_| retained_entry(0)).collect::<Vec<_>>(),
        (0..31)
            .map(|_| retained_entry(RESPONSE_BYTES))
            .collect::<Vec<_>>(),
    ] {
        let count = receipts.len();
        let mut actor =
            InstanceActor::new(InstanceRuntimeDescriptor::starting(InstanceId::generate()));
        actor.start();
        actor
            .enable_generation_admission_inner(
                Some(binding()),
                orbisync_application::CheckpointLimits::new(262_144, 64 * 1024 * 1024).unwrap(),
                64 * 1024 * 1024,
                receipts,
            )
            .unwrap();
        let before = footprint(&actor);
        let bound = actor.checkpoint_capacity_bytes();
        let result = actor.submit_prepared(InstanceCommand::Tick, request(1));
        assert_eq!(result.phase, AdmissionPhase::NotAdmitted);
        assert!(matches!(
            result.outcome,
            CommandOutcome::Rejected {
                code: CAPACITY_CODE,
                ..
            }
        ));
        assert_eq!(actor.admission.as_ref().unwrap().receipts.len(), count);
        assert_eq!(before, footprint(&actor));
        assert_eq!(bound, actor.checkpoint_capacity_bytes());
    }
}

#[test]
fn fallback_reservation_covers_maximum_response_and_escaped_rejection_metadata() {
    let cancelled = AtomicBool::new(false);
    let mut entry = retained_entry(RESPONSE_BYTES);
    entry.created_at_millis = i64::MIN;
    entry.expires_at_millis = i64::MAX;
    assert!(receipt_charge(&entry, &cancelled).unwrap() < OUTCOME_RESERVATION);
    entry.result = CheckpointDedupResult::Rejected {
        code: "\u{0}".repeat(128),
        detail: "\u{0}".repeat(8192),
    };
    assert!(receipt_charge(&entry, &cancelled).unwrap() < OUTCOME_RESERVATION);
    let checkpoint = Checkpoint::new(
        InstanceId::generate(),
        Revision::from_u64(i64::MAX as u64),
        Vec::new(),
        Timestamp::from_unix_millis(253_402_300_799_999).unwrap(),
    );
    assert!(checkpoint.to_json_bytes().unwrap().len() < HEADER);
}

#[test]
fn admitted_state_capacity_rejection_and_internal_deletion_use_correct_ledger_delta() {
    let mut actor = actor(HEADER + OUTCOME_RESERVATION);
    let before = footprint(&actor);
    let request = request(1);
    let result = actor.submit_prepared(
        spawn(EntityId::generate(), UserId::generate()),
        request.clone(),
    );
    assert_eq!(result.phase, AdmissionPhase::Completed);
    assert!(matches!(
        result.outcome,
        CommandOutcome::Rejected {
            code: CAPACITY_CODE,
            ..
        }
    ));
    assert_eq!(before, footprint(&actor));
    let mut conflict = request;
    conflict.fingerprint = [18; 32];
    assert!(matches!(
        actor
            .submit_prepared(InstanceCommand::Tick, conflict)
            .outcome,
        CommandOutcome::Rejected {
            code: "COMMAND_ID_CONFLICT",
            ..
        }
    ));
    let (mut actor, id, owner, _) = fixture();
    actor.admission.as_mut().unwrap().limit = actor.checkpoint_capacity_bytes().unwrap();
    let rev = actor.entity_snapshot(id).unwrap().revision();
    assert!(matches!(
        actor.handle(InstanceCommand::DeleteEntity {
            command_id: None,
            entity_id: id,
            expected_revision: rev,
            requester: owner,
            permissions: WorldPermissions::all()
        }),
        CommandOutcome::Applied { .. }
    ));
    assert_eq!(actor.checkpoint_capacity_bytes(), Some(HEADER));
}

#[test]
fn rate_limiter_and_hook_guard_are_revalidated_without_consuming_on_capacity_refusal() {
    let (mut actor, id, owner, _) = fixture();
    actor.component_updates_per_sec = 1;
    let update = |rev| InstanceCommand::UpdateEntityComponent {
        command_id: None,
        entity_id: id,
        component_key: "example.rate".into(),
        payload_bytes: vec![1],
        expected_revision: rev,
        now: now(),
        requester: owner,
        permissions: WorldPermissions::all(),
    };
    let revision = actor.entity_snapshot(id).unwrap().revision();
    let before = footprint(&actor);
    actor.submit_prepared(update(revision), request(RESPONSE_BYTES + 1));
    assert_eq!(before, footprint(&actor));
    let expected = actor.entity_snapshot(id).map(Box::new);
    assert!(matches!(
        actor.submit_prepared(update(revision), request(1)).outcome,
        CommandOutcome::Applied { .. }
    ));
    let before = footprint(&actor);
    let stale = InstanceCommand::WithExpectedEntityState {
        entity_id: id,
        expected,
        command: Box::new(update(actor.entity_snapshot(id).unwrap().revision())),
    };
    assert!(matches!(
        actor.submit_prepared(stale, request(1)).outcome,
        CommandOutcome::Rejected {
            code: "REVISION_MISMATCH",
            ..
        }
    ));
    assert_eq!(before, footprint(&actor));
    assert!(matches!(
        actor
            .submit_prepared(
                update(actor.entity_snapshot(id).unwrap().revision()),
                request(1)
            )
            .outcome,
        CommandOutcome::Rejected {
            code: "COMPONENT_RATE_LIMITED",
            ..
        }
    ));
    assert_eq!(before, footprint(&actor));
}

#[test]
fn outbox_saturation_never_drops_unpublished_generation_effects() {
    let mut actor = actor(8 * 1024 * 1024);
    actor.outbox = Outbox::with_capacity(1);
    actor.persistence_outbox = PersistenceOutbox::with_capacity(1);
    actor.handle(spawn(EntityId::generate(), UserId::generate()));
    let before = footprint(&actor);
    let result = actor.submit_prepared(spawn(EntityId::generate(), UserId::generate()), request(1));
    assert!(matches!(
        result.outcome,
        CommandOutcome::Rejected {
            code: CAPACITY_CODE,
            ..
        }
    ));
    assert_eq!(before, footprint(&actor));
}

#[test]
fn expired_committed_attempt_never_renews_and_stale_evidence_fences() {
    let mut actor = actor(8 * 1024 * 1024);
    actor.submit_prepared(InstanceCommand::Tick, request(1));
    let (attempt, snapshot) = actor.capture_generation(now()).unwrap();
    assert!(
        actor
            .resolve_generation(
                &attempt,
                GenerationResolution::Committed { publish_seq: 1 },
                TTL
            )
            .is_err()
    );
    assert!(
        actor
            .admission
            .as_ref()
            .unwrap()
            .receipts
            .values()
            .all(|r| r.durable)
    );
    assert_eq!(actor.build_checkpoint(now()).dedup, snapshot.dedup);
    let (next, _) = actor.capture_generation(now()).unwrap();
    assert_ne!(next.generation_id, attempt.generation_id);
    assert!(
        actor
            .resolve_generation(&attempt, GenerationResolution::ProvenAbsent, TTL)
            .is_err()
    );
    assert!(actor.capture_generation(now()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_task_call_cancels_queued_and_in_preparation_work() {
    use std::future::Future;
    #[derive(Debug)]
    struct HoldingEncoder {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }
    impl PreparedOutcomeEncoder for HoldingEncoder {
        fn encode(
            &self,
            _: ProspectiveOutcome<'_>,
            _: &AtomicBool,
        ) -> Result<Vec<u8>, ApplicationError> {
            self.entered.notify_one();
            let (lock, condition) = &*self.release;
            let (_guard, _) = condition
                .wait_timeout_while(
                    lock.lock().unwrap(),
                    std::time::Duration::from_secs(2),
                    |released| !*released,
                )
                .unwrap();
            Ok(vec![1])
        }
    }
    let actor = actor(8 * 1024 * 1024);
    let registry = crate::RuntimeRegistry::new();
    let handle = registry.ensure_instance(actor);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let mut first = request(1);
    first.encoder = Arc::new(HoldingEncoder {
        entered: entered.clone(),
        release: release.clone(),
    });
    let first_cancel = first.cancelled.clone();
    let first_handle = handle.clone();
    let caller = tokio::spawn(async move {
        first_handle
            .submit_prepared(spawn(EntityId::generate(), UserId::generate()), first)
            .await
    });
    entered.notified().await;
    let second = request(1);
    let second_cancel = second.cancelled.clone();
    let mut queued =
        Box::pin(handle.submit_prepared(spawn(EntityId::generate(), UserId::generate()), second));
    std::future::poll_fn(|cx| {
        assert!(queued.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(queued);
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(first_cancel.load(Ordering::Acquire));
    assert!(second_cancel.load(Ordering::Acquire));
    *release.0.lock().unwrap() = true;
    release.1.notify_one();
    let snapshot = handle.read_snapshot().await.unwrap();
    assert!(snapshot.entities.is_empty());
    assert_eq!(snapshot.revision, Revision::INITIAL);
    let (_, checkpoint) = handle.capture_generation(now()).await.unwrap();
    assert!(checkpoint.dedup.is_empty());
}

#[tokio::test]
async fn task_dispatch_exhausts_ingress_and_interest_index_tracks_only_committed_candidates() {
    let (actor, id, owner, presence) = fixture();
    let commands = ingress(&actor, id, owner, presence);
    let registry = crate::RuntimeRegistry::new();
    let handle = registry.ensure_instance(actor);
    let before = handle.read_snapshot().await.unwrap();
    for command in commands {
        let result = handle
            .submit_prepared(command, request(RESPONSE_BYTES + 1))
            .await
            .unwrap();
        assert!(matches!(
            result.outcome,
            CommandOutcome::Rejected {
                code: CAPACITY_CODE,
                ..
            }
        ));
        let after = handle.read_snapshot().await.unwrap();
        assert_eq!(before.entities, after.entities);
        assert_eq!(before.members, after.members);
        assert_eq!(before.revision, after.revision);
        assert_eq!(
            before.oldest_retained_revision,
            after.oldest_retained_revision
        );
    }
    let new_id = EntityId::generate();
    assert!(matches!(
        handle.submit(spawn(new_id, owner)).await.unwrap(),
        CommandOutcome::Applied { .. }
    ));
    assert!(
        handle
            .cached_interest_views()
            .iter()
            .any(|view| view.id == new_id)
    );
    assert!(
        !registry.remove_task(handle.instance_id()),
        "legacy removal must retain generation ownership"
    );
}

#[test]
fn handing_out_a_retry_invalidates_older_absence_proof() {
    let mut actor = actor(8 * 1024 * 1024);
    let (attempt, _) = actor.capture_generation(now()).unwrap();
    actor
        .resolve_generation(&attempt, GenerationResolution::ProvenAbsent, 0)
        .unwrap();
    let (retry, _) = actor.capture_generation(now()).unwrap();
    assert_eq!(retry, attempt);
    assert!(actor.retire_absent_generation(&attempt).is_err());
    actor
        .resolve_generation(&attempt, GenerationResolution::ProvenAbsent, 0)
        .unwrap();
    actor.retire_absent_generation(&attempt).unwrap();
}

#[test]
fn invalid_outcome_metadata_and_empty_encoding_never_apply_entity_state() {
    let mut actor = actor(8 * 1024 * 1024);
    let before = footprint(&actor);
    let command = spawn(EntityId::generate(), UserId::generate());
    let mut invalid = request(1);
    invalid.message_id = Default::default();
    assert_eq!(
        actor.submit_prepared(command.clone(), invalid).phase,
        AdmissionPhase::NotAdmitted
    );
    let mut overflow = request(1);
    overflow.now_millis = i64::MAX;
    assert_eq!(
        actor.submit_prepared(command.clone(), overflow).phase,
        AdmissionPhase::NotAdmitted
    );
    let result = actor.submit_prepared(command, request(0));
    assert!(matches!(
        result.outcome,
        CommandOutcome::Rejected {
            code: CAPACITY_CODE,
            ..
        }
    ));
    assert_eq!(before, footprint(&actor));
    assert!(
        actor.capture_generation(now()).is_ok(),
        "retained rejection must be encodable"
    );
}

#[test]
fn committed_effect_prefixes_drain_independently_and_respect_writer_fencing() {
    for revoke in [false, true] {
        let mut actor = actor(8 * 1024 * 1024);
        let first = EntityId::generate();
        let second = EntityId::generate();
        actor.handle(spawn(first, UserId::generate()));
        let (attempt, _) = actor.capture_generation(now()).unwrap();
        actor
            .resolve_generation(
                &attempt,
                GenerationResolution::Committed { publish_seq: 1 },
                0,
            )
            .unwrap();
        assert_eq!(actor.drain_outbox().len(), 1);
        actor.handle(spawn(second, UserId::generate()));
        if revoke {
            actor
                .admission
                .as_ref()
                .unwrap()
                .binding
                .writer
                .invalidate();
            assert!(actor.drain_outbox().is_empty());
            assert!(actor.drain_persistence_batch().is_empty());
            assert_eq!(actor.persistence_outbox.len(), 2);
        } else {
            assert!(actor.drain_outbox().is_empty());
            assert!(matches!(actor.drain_persistence_batch().as_slice(),
                [EntityPersistenceEvent::Spawned(entity)] if entity.id() == first));
            assert!(actor.drain_persistence_batch().is_empty());
            let (attempt, _) = actor.capture_generation(now()).unwrap();
            actor
                .resolve_generation(
                    &attempt,
                    GenerationResolution::Committed { publish_seq: 2 },
                    0,
                )
                .unwrap();
            assert_eq!(actor.drain_outbox().len(), 1);
            assert!(matches!(actor.drain_persistence_batch().as_slice(),
                [EntityPersistenceEvent::Spawned(entity)] if entity.id() == second));
        }
    }
}
