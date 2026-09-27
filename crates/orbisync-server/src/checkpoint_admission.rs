//! Explicit generation-mode transport adapter. Existing realtime composition
//! remains legacy until the phase 3/4 adapter and authority gates are connected.
//! Local completion is never a durable ACK and is never sent to legacy save.

use orbisync_application::{ApplicationError, checkpoint_admission::*};
use orbisync_domain::{CommandId, InstanceId};
use orbisync_protocol::v1::{EntityCommand, ErrorMessage};
use orbisync_world_runtime::{
    MailboxSendError, RuntimeRegistry, actor::AdmissionResult, command::InstanceCommand,
};
use prost::Message;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Early generation dispatch result, before fresh-command limiter and hooks.
pub enum GenerationDispatch {
    /// No retained identity; caller may perform fresh validation.
    Fresh,
    /// Original durable result.
    Reply(crate::command_dedup::CommandDedupResult),
    /// Explicit non-completion; never a legacy no-op durability success.
    Refused {
        /// Stable transport error code.
        code: String,
        /// Bounded explanation without a completed result.
        detail: String,
        /// Whether retrying the unchanged request may make progress.
        retryable: bool,
    },
}

/// Actual WS early-dispatch path. Matching uncertainty drives the same pinned
/// attempt and re-reads its original receipt after durability confirmation.
pub async fn dispatch_generation_receipt(
    state: &crate::realtime_ws::RealtimeState,
    instance: InstanceId,
    command: &EntityCommand,
    deadline: tokio::time::Instant,
) -> Result<GenerationDispatch, ApplicationError> {
    let handle = state
        .registry
        .handle(instance)
        .ok_or_else(|| ApplicationError::port_failure("actor unavailable"))?;
    let id = command
        .command_id
        .parse()
        .map_err(|_| ApplicationError::port_failure("invalid command identity"))?;
    let fingerprint = crate::command_dedup::CommandDedupStore::fingerprint(command);
    let now = || state.clock.now().to_unix_millis().unwrap_or(0);
    let Some(mut result) =
        tokio::time::timeout_at(deadline, handle.lookup_generation(id, fingerprint, now()))
            .await
            .map_err(|_| ApplicationError::port_failure("receipt lookup deadline"))??
    else {
        return Ok(GenerationDispatch::Fresh);
    };
    if matches!(
        result.outcome,
        orbisync_world_runtime::command::CommandOutcome::Rejected {
            code: "PERSISTENCE_UNAVAILABLE",
            ..
        }
    ) && result.receipt.is_some()
    {
        let service = state
            .generation
            .as_ref()
            .ok_or_else(|| ApplicationError::port_failure("generation unavailable"))?;
        let durable =
            tokio::time::timeout_at(deadline, service.persist(handle.clone(), state.clock.now()))
                .await
                .map_err(|_| ApplicationError::port_failure("generation awaiting durability"))?;
        if durable.as_ref().is_err_and(|error| {
            error.kind() == orbisync_application::ApplicationErrorKind::CommittedButExpired
        }) {
            return Ok(GenerationDispatch::Refused {
                code: "REPLAY_WINDOW_EXPIRED".into(),
                detail: "committed outcome expired; do not reapply".into(),
                retryable: false,
            });
        }
        durable?;
        result =
            tokio::time::timeout_at(deadline, handle.lookup_generation(id, fingerprint, now()))
                .await
                .map_err(|_| ApplicationError::port_failure("receipt lookup deadline"))??
                .ok_or_else(|| ApplicationError::port_failure("receipt unavailable"))?;
    }
    if let Some(receipt) = result.receipt {
        let reply = match receipt.result {
            orbisync_world_runtime::CheckpointDedupResult::Applied { response_payload } => {
                crate::command_dedup::CommandDedupResult::Applied {
                    command: EntityCommand::decode(response_payload.as_slice())
                        .map_err(|_| ApplicationError::port_failure("invalid receipt"))?,
                    message_id: receipt.message_id,
                }
            }
            orbisync_world_runtime::CheckpointDedupResult::Rejected { code, detail } => {
                crate::command_dedup::CommandDedupResult::Rejected {
                    code,
                    detail,
                    message_id: receipt.message_id,
                }
            }
        };
        return Ok(GenerationDispatch::Reply(reply));
    }
    if let orbisync_world_runtime::command::CommandOutcome::Rejected { code, detail } =
        result.outcome
    {
        return Ok(GenerationDispatch::Refused {
            code: code.into(),
            detail,
            retryable: matches!(code, "PERSISTENCE_UNAVAILABLE" | CAPACITY_CODE),
        });
    }
    Err(ApplicationError::port_failure("invalid receipt phase"))
}

#[derive(Debug)]
struct RejectionEncoder;
impl PreparedOutcomeEncoder for RejectionEncoder {
    fn encode(
        &self,
        _: ProspectiveOutcome<'_>,
        _: &AtomicBool,
    ) -> Result<Vec<u8>, ApplicationError> {
        Err(ApplicationError::port_failure(
            "rejection has no applied response",
        ))
    }
}

/// Deterministic parsing rejection for a valid UUID uses the generation ledger.
pub async fn reject_generation_command(
    state: &crate::realtime_ws::RealtimeState,
    instance: InstanceId,
    command: &EntityCommand,
    detail: String,
    deadline: tokio::time::Instant,
) -> Result<(), ApplicationError> {
    let handle = state
        .registry
        .handle(instance)
        .ok_or_else(|| ApplicationError::port_failure("actor unavailable"))?;
    let request = AdmissionRequest {
        command_id: command
            .command_id
            .parse()
            .map_err(|_| ApplicationError::port_failure("invalid identity"))?,
        fingerprint: crate::command_dedup::CommandDedupStore::fingerprint(command),
        message_id: uuid::Uuid::now_v7(),
        now_millis: state.clock.now().to_unix_millis().unwrap_or(0),
        cancelled: Arc::new(AtomicBool::new(false)),
        encoder: Arc::new(RejectionEncoder),
    };
    let result = tokio::time::timeout_at(
        deadline,
        handle.reject_prepared(request, "INVALID_ARGUMENT", detail),
    )
    .await
    .map_err(|_| ApplicationError::port_failure("rejection admission deadline"))??;
    if result.phase != AdmissionPhase::Completed {
        return Err(ApplicationError::port_failure("rejection not admitted"));
    }
    let service = state
        .generation
        .as_ref()
        .ok_or_else(|| ApplicationError::port_failure("generation unavailable"))?;
    tokio::time::timeout_at(deadline, service.persist(handle, state.clock.now()))
        .await
        .map_err(|_| ApplicationError::port_failure("rejection awaiting durability"))?
}

/// Prepared protobuf encoder for the existing entity-command result contract.
#[derive(Debug)]
pub struct EntityOutcomeEncoder {
    template: EntityCommand,
}
impl EntityOutcomeEncoder {
    /// Validates bounded transport input before retaining its response template.
    pub fn new(template: EntityCommand) -> Result<Self, ApplicationError> {
        if !matches!(template.operation.as_str(), "spawn" | "update" | "delete") {
            return Err(ApplicationError::port_failure(
                "unsupported entity outcome operation",
            ));
        }
        validate_template(&template, &AtomicBool::new(false))?;
        Ok(Self { template })
    }
}
impl PreparedOutcomeEncoder for EntityOutcomeEncoder {
    fn encode(
        &self,
        outcome: ProspectiveOutcome<'_>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, ApplicationError> {
        validate_template(&self.template, cancelled)?;
        let ProspectiveOutcome::Applied {
            revision,
            entity_revision,
            entity,
        } = outcome
        else {
            return Err(ApplicationError::port_failure(
                "rejections use the stable receipt contract",
            ));
        };
        let entity =
            entity.ok_or_else(|| ApplicationError::port_failure("missing prospective entity"))?;
        if entity.id().to_string() != self.template.entity_id {
            return Err(ApplicationError::port_failure(
                "prospective response target mismatch",
            ));
        }
        let mut response = self.template.clone();
        response.instance_revision = Some(revision.as_u64());
        response.expected_revision = entity_revision.unwrap_or(revision).as_u64();
        if response.operation == "delete" || response.operation == "spawn" {
            // Tombstone metadata comes from the pre-removal candidate, never a
            // later registry read or untrusted request arguments.
            use prost_types::{Struct, Value, value::Kind};
            let mut fields = std::collections::BTreeMap::new();
            fields.insert(
                "kind".into(),
                Value {
                    kind: Some(Kind::StringValue(entity.kind().as_str().into())),
                },
            );
            let visibility = match entity.visibility() {
                orbisync_domain::VisibilityPolicy::Global => "global",
                orbisync_domain::VisibilityPolicy::OwnerOnly => "owner_only",
                orbisync_domain::VisibilityPolicy::Spatial { radius } => {
                    fields.insert(
                        "visibility_radius".into(),
                        Value {
                            kind: Some(Kind::NumberValue(f64::from(*radius))),
                        },
                    );
                    "spatial"
                }
                orbisync_domain::VisibilityPolicy::RoleRestricted { .. } => "role_restricted",
                orbisync_domain::VisibilityPolicy::Explicit { .. } => "explicit",
                orbisync_domain::VisibilityPolicy::Custom { .. } => "custom",
            };
            fields.insert(
                "visibility".into(),
                Value {
                    kind: Some(Kind::StringValue(visibility.into())),
                },
            );
            if let Some(owner) = entity.owner() {
                fields.insert(
                    "owner".into(),
                    Value {
                        kind: Some(Kind::StringValue(owner.to_string())),
                    },
                );
            }
            if let Some(transform) = entity.transform() {
                let p = transform.position();
                for (key, number) in [
                    ("position_x", p.x()),
                    ("position_y", p.y()),
                    ("position_z", p.z()),
                ] {
                    fields.insert(
                        key.into(),
                        Value {
                            kind: Some(Kind::NumberValue(f64::from(number))),
                        },
                    );
                }
            }
            response.arguments = Some(Struct { fields });
        } else if response.operation == "update" {
            // The stored generic component is the server's canonical JSON
            // representation of EntityCommand arguments. Encode the candidate's
            // version so a validated hook rewrite cannot echo stale arguments.
            let key = self
                .template
                .arguments
                .as_ref()
                .and_then(|a| {
                    a.fields
                        .get("component_key")
                        .or_else(|| a.fields.get("key"))
                })
                .and_then(|v| match &v.kind {
                    Some(prost_types::value::Kind::StringValue(key)) => Some(key.as_str()),
                    _ => None,
                })
                .unwrap_or("entity.component");
            let payload = entity
                .components()
                .get(key)
                .ok_or_else(|| ApplicationError::port_failure("prospective component missing"))?;
            let object: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(payload).map_err(|_| {
                    ApplicationError::port_failure("prospective component is not command arguments")
                })?;
            response.arguments = Some(prost_types::Struct {
                fields: object
                    .into_iter()
                    .map(|(k, v)| (k, json_value(v)))
                    .collect(),
            });
        }
        validate_template(&response, cancelled)?;
        let expected = response.encoded_len();
        let bytes = response.encode_to_vec();
        if bytes.len() != expected
            || bytes.len() > RESPONSE_BYTES
            || cancelled.load(Ordering::Acquire)
        {
            return Err(ApplicationError::port_failure(
                "prospective response exceeds bound or was cancelled",
            ));
        }
        Ok(bytes)
    }
}

fn json_value(value: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(v) => Kind::BoolValue(v),
        serde_json::Value::Number(v) => Kind::NumberValue(v.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(v) => Kind::StringValue(v),
        serde_json::Value::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values.into_iter().map(json_value).collect(),
        }),
        serde_json::Value::Object(values) => Kind::StructValue(prost_types::Struct {
            fields: values
                .into_iter()
                .map(|(k, v)| (k, json_value(v)))
                .collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

fn validate_template(
    command: &EntityCommand,
    cancelled: &AtomicBool,
) -> Result<(), ApplicationError> {
    fn value(
        v: &prost_types::Value,
        depth: usize,
        budget: &mut usize,
        cancelled: &AtomicBool,
    ) -> bool {
        if depth > 32 || cancelled.load(Ordering::Acquire) {
            return false;
        }
        let cost = match &v.kind {
            Some(prost_types::value::Kind::StringValue(s)) => s.len().saturating_add(16),
            _ => 16,
        };
        let Some(left) = budget.checked_sub(cost) else {
            return false;
        };
        *budget = left;
        match &v.kind {
            Some(prost_types::value::Kind::StructValue(s)) => {
                structure(s, depth + 1, budget, cancelled)
            }
            Some(prost_types::value::Kind::ListValue(list)) => list
                .values
                .iter()
                .all(|v| value(v, depth + 1, budget, cancelled)),
            _ => true,
        }
    }
    fn structure(
        s: &prost_types::Struct,
        depth: usize,
        budget: &mut usize,
        cancelled: &AtomicBool,
    ) -> bool {
        if depth > 32 {
            return false;
        }
        s.fields.iter().all(|(key, v)| {
            let Some(left) = budget.checked_sub(key.len().saturating_add(16)) else {
                return false;
            };
            *budget = left;
            value(v, depth, budget, cancelled)
        })
    }
    let mut budget = RESPONSE_BYTES;
    let input_bytes = command
        .command_id
        .len()
        .saturating_add(command.entity_id.len())
        .saturating_add(command.operation.len());
    if input_bytes > RESPONSE_BYTES
        || cancelled.load(Ordering::Acquire)
        || command
            .arguments
            .as_ref()
            .is_some_and(|s| !structure(s, 0, &mut budget, cancelled))
        || command.encoded_len() > RESPONSE_BYTES
    {
        return Err(ApplicationError::new(
            orbisync_application::ApplicationErrorKind::CheckpointCapacity,
            "entity response preparation bound exceeded",
        ));
    }
    Ok(())
}

/// Dispatches already-authorized command and prospective protobuf template to
/// the real registry. The caller retains the request identity across retries;
/// a Completed result must go to the generation coordinator, never legacy save.
#[allow(clippy::too_many_arguments)]
pub async fn submit_generation_command(
    registry: &RuntimeRegistry,
    instance: InstanceId,
    command: InstanceCommand,
    template: EntityCommand,
    fingerprint: [u8; 32],
    message_id: uuid::Uuid,
    now_millis: i64,
    cancelled: Arc<AtomicBool>,
) -> Result<AdmissionResult, ApplicationError> {
    let inner_command = match &command {
        InstanceCommand::WithExpectedEntityState { command, .. } => command.as_ref(),
        command => command,
    };
    let (operation, target) = match inner_command {
        InstanceCommand::SpawnEntity { entity_id, .. } => ("spawn", entity_id),
        InstanceCommand::UpdateEntityComponent { entity_id, .. } => ("update", entity_id),
        InstanceCommand::DeleteEntity { entity_id, .. } => ("delete", entity_id),
        _ => {
            return Err(ApplicationError::port_failure(
                "unsupported transport command",
            ));
        }
    };
    if template.operation != operation || template.entity_id != target.to_string() {
        return Err(ApplicationError::port_failure(
            "response template does not match authorized command",
        ));
    }
    let command_id = template
        .command_id
        .parse::<CommandId>()
        .map_err(|_| ApplicationError::port_failure("invalid command identity"))?;
    let encoder = Arc::new(EntityOutcomeEncoder::new(template)?);
    registry
        .submit_prepared(
            instance,
            command,
            AdmissionRequest {
                command_id,
                fingerprint,
                message_id,
                now_millis,
                cancelled,
                encoder,
            },
        )
        .await
        .map_err(|e| match e {
            MailboxSendError::Full { .. } => ApplicationError::port_failure("actor mailbox full"),
            MailboxSendError::Closed { .. } => ApplicationError::port_failure("actor unavailable"),
        })
}

/// Maps capacity phase without changing realtime Snapshot or protobuf framing.
/// Completed may be sent only after generation durability is confirmed.
pub fn capacity_error(phase: AdmissionPhase, request_message_id: String) -> ErrorMessage {
    ErrorMessage {
        code: CAPACITY_CODE.into(),
        message: if phase == AdmissionPhase::NotAdmitted {
            "checkpoint receipt capacity unavailable".into()
        } else {
            CAPACITY_DETAIL.into()
        },
        request_message_id,
        retryable: phase == AdmissionPhase::NotAdmitted,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use orbisync_domain::{Entity, EntityId, EntityKind, Revision, Timestamp, VisibilityPolicy};
    #[test]
    fn prospective_protobuf_and_capacity_phases_are_stable() {
        let entity = Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Avatar,
            None,
            None,
            VisibilityPolicy::Global,
            Timestamp::from_unix_millis(0).unwrap(),
        );
        let template = EntityCommand {
            command_id: CommandId::generate().to_string(),
            entity_id: entity.id().to_string(),
            expected_revision: 0,
            operation: "spawn".into(),
            arguments: None,
            instance_revision: None,
        };
        let encoder = EntityOutcomeEncoder::new(template).unwrap();
        let bytes = encoder
            .encode(
                ProspectiveOutcome::Applied {
                    revision: Revision::from_u64(19),
                    entity_revision: Some(Revision::from_u64(4)),
                    entity: Some(&entity),
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        let decoded = EntityCommand::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.instance_revision, Some(19));
        assert_eq!(decoded.expected_revision, 4);
        assert_eq!(decoded.encoded_len(), bytes.len());
        let not_admitted = capacity_error(AdmissionPhase::NotAdmitted, "message".into());
        let completed = capacity_error(AdmissionPhase::Completed, "message".into());
        assert_eq!(not_admitted.code, "CHECKPOINT_CAPACITY");
        assert!(not_admitted.retryable);
        assert_eq!(completed.message, CAPACITY_DETAIL);
        assert!(!completed.retryable);
    }

    #[derive(Debug)]
    struct UnavailableStore;
    #[async_trait::async_trait]
    impl GenerationStore for UnavailableStore {
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
    #[tokio::test]
    async fn real_server_adapter_prepares_protobuf_inside_actor_and_has_no_legacy_save_fallback() {
        use orbisync_world_runtime::{
            InstanceRuntimeDescriptor,
            actor::{GenerationBinding, InstanceActor},
            command::WorldPermissions,
        };
        let instance = InstanceId::generate();
        let id = EntityId::generate();
        let user = orbisync_domain::UserId::generate();
        let command_id = CommandId::generate();
        let command = InstanceCommand::SpawnEntity {
            command_id: Some(command_id),
            entity_id: id,
            kind: EntityKind::Avatar,
            owner: Some(user),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: user,
            permissions: WorldPermissions::all(),
        };
        let template = EntityCommand {
            command_id: command_id.to_string(),
            entity_id: id.to_string(),
            expected_revision: 0,
            operation: "spawn".into(),
            arguments: None,
            instance_revision: None,
        };
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor::starting(instance));
        actor
            .enable_generation_admission(
                Some(GenerationBinding {
                    store: Arc::new(UnavailableStore),
                    head: 0,
                    writer: WriterPermit::new(WriterToken {
                        epoch: 1,
                        boot: uuid::Uuid::now_v7(),
                    })
                    .unwrap(),
                }),
                8 * 1024 * 1024,
                Vec::new(),
            )
            .unwrap();
        actor.start();
        let registry = RuntimeRegistry::new();
        registry.ensure_instance(actor);
        let message_id = uuid::Uuid::now_v7();
        let result = submit_generation_command(
            &registry,
            instance,
            command.clone(),
            template.clone(),
            [1; 32],
            message_id,
            0,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert_eq!(result.phase, AdmissionPhase::Completed);
        let receipt = result.receipt.unwrap();
        let response_payload = match receipt.result {
            orbisync_world_runtime::CheckpointDedupResult::Applied { response_payload } => {
                Some(response_payload)
            }
            _ => None,
        }
        .unwrap();
        let response = EntityCommand::decode(response_payload.as_slice()).unwrap();
        assert_eq!(response.entity_id, id.to_string());
        assert_eq!(response.command_id, command_id.to_string());
        let before = registry.read_snapshot(instance).await.unwrap();
        assert_eq!(response.instance_revision, Some(before.revision.as_u64()));
        let duplicate = submit_generation_command(
            &registry,
            instance,
            command.clone(),
            template.clone(),
            [1; 32],
            message_id,
            1,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert_eq!(duplicate.phase, AdmissionPhase::Replay);
        assert!(matches!(
            duplicate.outcome,
            orbisync_world_runtime::command::CommandOutcome::Rejected {
                code: "PERSISTENCE_UNAVAILABLE",
                ..
            }
        ));
        assert_eq!(
            before.revision,
            registry.read_snapshot(instance).await.unwrap().revision
        );
        assert!(
            registry
                .build_checkpoint(instance, Timestamp::from_unix_millis(0).unwrap())
                .await
                .is_none()
        );
        let handle = registry.handle(instance).unwrap();
        let (attempt, _) = handle
            .capture_generation(Timestamp::from_unix_millis(0).unwrap())
            .await
            .unwrap();
        handle
            .resolve_generation(
                attempt,
                GenerationResolution::Committed { publish_seq: 1 },
                0,
            )
            .await
            .unwrap();
        for now in [86_399_999, 86_400_000, 86_400_000, 86_400_001] {
            let duplicate = submit_generation_command(
                &registry,
                instance,
                command.clone(),
                template.clone(),
                [1; 32],
                message_id,
                now,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            if now < 86_400_000 {
                assert_eq!(duplicate.phase, AdmissionPhase::Replay);
                assert!(duplicate.receipt.is_some());
            } else {
                assert_eq!(duplicate.phase, AdmissionPhase::Expired);
                assert!(duplicate.receipt.is_none());
                assert!(matches!(
                    duplicate.outcome,
                    orbisync_world_runtime::command::CommandOutcome::Rejected {
                        code: "REPLAY_WINDOW_EXPIRED",
                        ..
                    }
                ));
            }
            assert_eq!(
                before.revision,
                registry.read_snapshot(instance).await.unwrap().revision
            );
        }
    }

    #[test]
    fn oversized_nested_or_cancelled_templates_cannot_prepare_a_response() {
        let template = |arguments| EntityCommand {
            command_id: CommandId::generate().to_string(),
            entity_id: EntityId::generate().to_string(),
            expected_revision: 0,
            operation: "spawn".into(),
            arguments,
            instance_revision: None,
        };
        let mut value = prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "x".repeat(RESPONSE_BYTES),
            )),
        };
        let args = |value| {
            Some(prost_types::Struct {
                fields: [("data".into(), value)].into(),
            })
        };
        assert!(EntityOutcomeEncoder::new(template(args(value.clone()))).is_err());
        value.kind = None;
        for _ in 0..33 {
            value = prost_types::Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue {
                        values: vec![value],
                    },
                )),
            };
        }
        assert!(EntityOutcomeEncoder::new(template(args(value))).is_err());
        assert!(validate_template(&template(None), &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn capacity_rejection_is_compatible_with_server_receipt_restore() {
        use crate::command_dedup::{CommandDedupAdmission, CommandDedupResult, CommandDedupStore};
        let store = CommandDedupStore::default();
        let instance = InstanceId::generate();
        let command_id = CommandId::generate();
        let entry = orbisync_world_runtime::CheckpointDedupEntry {
            command_id: command_id.to_string(),
            fingerprint: vec![1; 32],
            message_id: uuid::Uuid::now_v7().to_string(),
            created_at_millis: 0,
            expires_at_millis: CommandDedupStore::DEFAULT_TTL_MILLIS,
            result: orbisync_world_runtime::CheckpointDedupResult::Rejected {
                code: CAPACITY_CODE.into(),
                detail: CAPACITY_DETAIL.into(),
            },
        };
        assert_eq!(store.restore(instance, &[entry], 1).unwrap(), 1);
        assert!(
            matches!(store.begin(instance, command_id, [1; 32], 1), CommandDedupAdmission::Duplicate(
            CommandDedupResult::Rejected { code, detail, .. }) if code == CAPACITY_CODE && detail == CAPACITY_DETAIL)
        );
    }
}

/// Production boundary between the independent config schema and application policy.
/// This validates again for callers that construct Config without the loader.
pub fn checkpoint_limits(
    config: &orbisync_config::Config,
) -> Result<orbisync_application::CheckpointLimits, orbisync_config::ConfigError> {
    orbisync_application::CheckpointLimits::new(
        config.world.checkpoint_chunk_bytes,
        config.world.checkpoint_max_serialized_bytes,
    )
    .map_err(|_| {
        orbisync_config::ConfigError::new(
            orbisync_config::ConfigErrorKind::InvalidValue,
            "world.checkpoint_max_serialized_bytes",
            "invalid checkpoint limits",
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod limits_tests {
    use super::checkpoint_limits;
    use orbisync_config::{Config, MapEnv, UnknownKeyPolicy};
    use std::sync::Arc;

    #[derive(Debug)]
    struct Store(orbisync_application::CheckpointLimits);
    #[async_trait::async_trait]
    impl orbisync_application::checkpoint_admission::GenerationStore for Store {
        fn limits(&self) -> orbisync_application::CheckpointLimits {
            self.0
        }
        async fn publish(
            &self,
            _: &orbisync_application::checkpoint_admission::GenerationAttempt,
            _: &mut dyn orbisync_application::checkpoint_admission::GenerationSource,
        ) -> Result<
            orbisync_application::checkpoint_admission::GenerationResolution,
            orbisync_application::ApplicationError,
        > {
            Ok(orbisync_application::checkpoint_admission::GenerationResolution::Uncertain)
        }
        async fn resolve(
            &self,
            _: &orbisync_application::checkpoint_admission::GenerationAttempt,
        ) -> Result<
            orbisync_application::checkpoint_admission::GenerationResolution,
            orbisync_application::ApplicationError,
        > {
            Ok(orbisync_application::checkpoint_admission::GenerationResolution::Uncertain)
        }
    }
    #[tokio::test]
    async fn checkpoint_limits_reach_admission_pinned_codec_and_store_contract() {
        use orbisync_application::checkpoint_admission::{WriterPermit, WriterToken};
        use orbisync_application::checkpoint_stream::CheckpointSource;
        use orbisync_world_runtime::{
            InstanceRuntimeDescriptor,
            actor::{GenerationBinding, InstanceActor},
        };
        let config = Config::load(
            None,
            &MapEnv::from_pairs([
                ("ORBISYNC_WORLD_CHECKPOINT_CHUNK_BYTES", "32768"),
                ("ORBISYNC_WORLD_CHECKPOINT_MAX_SERIALIZED_BYTES", "16777216"),
            ]),
            &[],
        )
        .unwrap()
        .config;
        let policy = composed(&config);
        let writer = WriterPermit::new(WriterToken {
            epoch: 1,
            boot: uuid::Uuid::now_v7(),
        })
        .unwrap();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor::starting(
            orbisync_domain::InstanceId::generate(),
        ));
        actor.start();
        assert!(
            actor
                .enable_generation_admission_with_limits(
                    Some(GenerationBinding {
                        store: Arc::new(Store(Default::default())),
                        writer: writer.clone(),
                        head: 0
                    }),
                    policy,
                    Vec::new()
                )
                .is_err()
        );
        actor
            .enable_generation_admission_with_limits(
                Some(GenerationBinding {
                    store: Arc::new(Store(policy)),
                    writer,
                    head: 0,
                }),
                policy,
                Vec::new(),
            )
            .unwrap();
        assert_eq!(actor.generation_limits(), Some(policy));
        let (attempt, snapshot) = actor
            .capture_generation(orbisync_domain::Timestamp::from_unix_millis(1000).unwrap())
            .unwrap();
        let source = actor.generation_source(&attempt).unwrap();
        assert_eq!(source.limits(), policy);
        assert_eq!(source.manifest().digest, attempt.digest);
        let restored = orbisync_world_runtime::checkpoint::stream::decode(
            source,
            policy,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(Default::default()),
        )
        .await
        .unwrap();
        assert_eq!(restored, *snapshot);
    }

    fn composed(config: &Config) -> orbisync_application::CheckpointLimits {
        let policy = checkpoint_limits(config).unwrap();
        let state = crate::realtime_ws::RealtimeState::builder(
            config.realtime.clone(),
            Arc::new(orbisync_world_runtime::RuntimeRegistry::new()),
            Arc::new(crate::delivery::DeliveryRegistry::new()),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .with_checkpoint_limits(policy)
        .build();
        assert_eq!(state.checkpoint_limits, policy);
        assert!(state.checkpoint_store.is_none());
        policy
    }
    #[test]
    fn checkpoint_limits_production_defaults_env_bounds_and_invalid_values() {
        assert_eq!(
            composed(&Config::load(None, &MapEnv::default(), &[]).unwrap().config),
            orbisync_application::CheckpointLimits::default()
        );
        for (c, t) in [(16_384, 2_097_152), (1_048_576, 67_108_864)] {
            let env = MapEnv::from_pairs([
                ("ORBISYNC_WORLD_CHECKPOINT_CHUNK_BYTES", c.to_string()),
                (
                    "ORBISYNC_WORLD_CHECKPOINT_MAX_SERIALIZED_BYTES",
                    t.to_string(),
                ),
            ]);
            let loaded = Config::load(None, &env, &[]).unwrap();
            let policy = composed(&loaded.config);
            assert_eq!(policy.chunk_bytes(), c);
            assert_eq!(policy.max_serialized_bytes(), t);
        }
        for (key, values) in [
            (
                "ORBISYNC_WORLD_CHECKPOINT_CHUNK_BYTES",
                vec!["0", "16383", "1048577", "-1", "1.5", "18446744073709551616"],
            ),
            (
                "ORBISYNC_WORLD_CHECKPOINT_MAX_SERIALIZED_BYTES",
                vec![
                    "0",
                    "2097151",
                    "67108865",
                    "-1",
                    "2.5",
                    "18446744073709551616",
                ],
            ),
        ] {
            for value in values {
                assert!(
                    Config::load(None, &MapEnv::from_pairs([(key, value)]), &[]).is_err(),
                    "{key}={value}"
                );
            }
        }
        assert!(
            Config::load_with_policy(
                None,
                &MapEnv::from_pairs([("ORBISYNC_WORLD_CHECKPOINT_CHUNKS", "1")]),
                &[],
                UnknownKeyPolicy::Reject
            )
            .is_err()
        );
        let mut invalid = Config::default();
        invalid.world.checkpoint_chunk_bytes = 0;
        assert!(checkpoint_limits(&invalid).is_err());
    }
    #[test]
    fn checkpoint_limits_toml_env_and_loader_override_precedence() {
        let path =
            std::env::temp_dir().join(format!("orbisync-phase2-{}.toml", uuid::Uuid::now_v7()));
        struct File(std::path::PathBuf);
        impl Drop for File {
            fn drop(&mut self) {
                let _removed = std::fs::remove_file(&self.0);
            }
        }
        let file = File(path);
        std::fs::write(
            &file.0,
            "[world]\ncheckpoint_chunk_bytes = 32768\ncheckpoint_max_serialized_bytes = 16777216\n",
        )
        .unwrap();
        let loaded = Config::load(Some(&file.0), &MapEnv::default(), &[]).unwrap();
        assert_eq!(composed(&loaded.config).chunk_bytes(), 32768);
        let env = MapEnv::from_pairs([("ORBISYNC_WORLD_CHECKPOINT_CHUNK_BYTES", "65536")]);
        let loaded = Config::load(Some(&file.0), &env, &[]).unwrap();
        assert_eq!(composed(&loaded.config).chunk_bytes(), 65536);
        assert_eq!(composed(&loaded.config).max_serialized_bytes(), 16777216);
        // Loader accepts dotted overrides. The binary has no flags for these
        // two keys; actual CLI only overrides bind/log settings/config path.
        let loaded = Config::load(
            Some(&file.0),
            &env,
            &[("world.checkpoint_chunk_bytes".into(), "131072".into())],
        )
        .unwrap();
        assert_eq!(composed(&loaded.config).chunk_bytes(), 131072);
        std::fs::write(&file.0, "[world]\ncheckpoint_chunk_bytes = 1.5\n").unwrap();
        assert!(Config::load(Some(&file.0), &MapEnv::default(), &[]).is_err());
    }
}
