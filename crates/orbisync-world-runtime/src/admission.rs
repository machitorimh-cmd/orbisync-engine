//! Actor-owned admission. Detached preparation contains one entity/member and
//! reuses command validation without changing the live actor's side effects.
use super::*;
use crate::checkpoint::{
    CheckpointDedupEntry, CheckpointDedupResult, entity_charge, receipt_charge,
};
use orbisync_application::ApplicationError;
use orbisync_application::checkpoint_admission::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

const HEADER: usize = 4096;
const MAX_ENTITIES: usize = 65_536;
const MAX_RECEIPTS: usize = 4096;
const MAX_LOGICAL: usize = 8 * 1024 * 1024;
const TTL: i64 = 86_400_000;

/// Incremental generation interest index. Materialization belongs to read calls,
/// not preparation/commit; a small mutation never clones all world views.
#[derive(Debug, Clone)]
pub(crate) struct GenerationInterestIndex {
    views: Arc<std::sync::RwLock<BTreeMap<EntityId, EntityInterestView>>>,
    permit: WriterPermit,
    fenced: Arc<AtomicBool>,
}
impl GenerationInterestIndex {
    pub(crate) fn snapshot(&self) -> Arc<InterestSnapshot> {
        if !self.available() {
            return Arc::new(InterestSnapshot::default());
        }
        Arc::new(
            self.views
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect::<InterestSnapshot>(),
        )
    }
    pub(crate) fn available(&self) -> bool {
        self.permit.is_live() && !self.fenced.load(Ordering::Acquire)
    }
}

/// Phase 3 connection point. Activation must verify generation authority and
/// coherent restore before supplying this capability. There is no legacy adapter.
#[derive(Debug, Clone)]
pub struct GenerationBinding {
    /// Dedicated generation adapter.
    pub store: Arc<dyn GenerationStore>,
    /// Original writer boot permit.
    pub writer: WriterPermit,
    /// Verified publication sequence.
    pub head: i64,
}
/// Local result; completion does not authorize a durable acknowledgement.
#[derive(Debug, Clone)]
pub struct AdmissionResult {
    /// Admission phase.
    pub phase: AdmissionPhase,
    /// Local outcome.
    pub outcome: CommandOutcome,
    /// Fixed receipt; only durable, unexpired receipts may be replayed to a client.
    pub receipt: Option<CheckpointDedupEntry>,
}
#[derive(Debug)]
pub(super) struct Receipt {
    pub(super) entry: CheckpointDedupEntry,
    bytes: usize,
    logical: usize,
    durable: bool,
}
#[derive(Debug)]
pub(super) struct AdmissionState {
    durable_revision: Revision,
    cutoff_millis: Option<i64>,
    index: GenerationInterestIndex,
    binding: GenerationBinding,
    limit: usize,
    limits: orbisync_application::CheckpointLimits,
    entity_bytes: usize,
    charges: HashMap<EntityId, usize>,
    pub(super) receipts: BTreeMap<String, Receipt>,
    receipt_bytes: usize,
    logical: usize,
    pinned: Option<(GenerationAttempt, Arc<Checkpoint>)>,
    proven_absent: bool,
    fenced: bool,
    pub(super) committed_events: usize,
    pub(super) committed_persistence_events: usize,
}
impl AdmissionState {
    fn fence(&mut self) {
        self.fenced = true;
        self.index.fenced.store(true, Ordering::Release);
    }
    fn bytes(&self) -> usize {
        HEADER
            + self.entity_bytes
            + self.charges.len().saturating_sub(1)
            + self.receipt_bytes
            + self.receipts.len().saturating_sub(1)
    }
    fn available(&self) -> bool {
        !self.fenced
            && self.pinned.is_none()
            && self.binding.writer.is_live()
            && self.binding.head < i64::MAX
    }
}
fn rejected(code: &'static str, detail: impl Into<String>) -> CommandOutcome {
    CommandOutcome::Rejected {
        code,
        detail: detail.into(),
    }
}
fn not_admitted(code: &'static str, detail: &str) -> AdmissionResult {
    AdmissionResult {
        phase: AdmissionPhase::NotAdmitted,
        outcome: rejected(code, detail),
        receipt: None,
    }
}
fn target(command: &InstanceCommand) -> Option<EntityId> {
    match command {
        InstanceCommand::WithExpectedEntityState { entity_id, .. }
        | InstanceCommand::SpawnEntity { entity_id, .. }
        | InstanceCommand::DeleteEntity { entity_id, .. }
        | InstanceCommand::UpdateEntityComponent { entity_id, .. }
        | InstanceCommand::UpdateTransform { entity_id, .. }
        | InstanceCommand::TransferOwnership { entity_id, .. } => Some(*entity_id),
        InstanceCommand::Join { .. }
        | InstanceCommand::Leave { .. }
        | InstanceCommand::RetainReliable { .. }
        | InstanceCommand::PublishEvent { .. }
        | InstanceCommand::Tick
        | InstanceCommand::Shutdown => None,
    }
}
fn command_identity(command: &InstanceCommand) -> Option<orbisync_domain::CommandId> {
    match command {
        InstanceCommand::WithExpectedEntityState { command, .. } => command_identity(command),
        InstanceCommand::SpawnEntity { command_id, .. }
        | InstanceCommand::DeleteEntity { command_id, .. }
        | InstanceCommand::UpdateEntityComponent { command_id, .. } => *command_id,
        InstanceCommand::UpdateTransform { .. }
        | InstanceCommand::TransferOwnership { .. }
        | InstanceCommand::Join { .. }
        | InstanceCommand::Leave { .. }
        | InstanceCommand::RetainReliable { .. }
        | InstanceCommand::PublishEvent { .. }
        | InstanceCommand::Tick
        | InstanceCommand::Shutdown => None,
    }
}
impl InstanceActor {
    pub(crate) fn generation_stop_is_durable(&self) -> bool {
        self.descriptor.state == RuntimeState::Draining
            && self.member_count() == 0
            && self.admission.as_ref().is_some_and(|state| {
                state.binding.writer.is_live()
                    && !state.fenced
                    && state.pinned.is_none()
                    && state.binding.head > 0
                    && state.durable_revision == self.state.revision()
                    && state.receipts.values().all(|receipt| receipt.durable)
                    && state.committed_events == self.outbox.len()
                    && state.committed_persistence_events == self.persistence_outbox.len()
            })
    }
    /// Enables staged generation admission with an explicit adapter and verified
    /// authority binding. Receipts must come from the same head as actor state.
    pub fn enable_generation_admission(
        &mut self,
        binding: Option<GenerationBinding>,
        limit: usize,
        receipts: Vec<CheckpointDedupEntry>,
    ) -> Result<(), ApplicationError> {
        let limits = orbisync_application::CheckpointLimits::new(262_144, limit as u64)?;
        self.enable_generation_admission_with_limits(binding, limits, receipts)
    }

    /// Staged activation uses the same validated policy as codec and store.
    pub fn enable_generation_admission_with_limits(
        &mut self,
        binding: Option<GenerationBinding>,
        limits: orbisync_application::CheckpointLimits,
        receipts: Vec<CheckpointDedupEntry>,
    ) -> Result<(), ApplicationError> {
        if binding.as_ref().is_some_and(|b| b.store.limits() != limits) {
            return Err(ApplicationError::port_failure(
                "generation store policy mismatch",
            ));
        }
        self.enable_generation_admission_inner(
            binding,
            limits,
            limits.max_serialized_bytes(),
            receipts,
        )
    }

    /// Policy retained by staged admission and generation capture.
    pub fn generation_limits(&self) -> Option<orbisync_application::CheckpointLimits> {
        self.admission.as_ref().map(|state| state.limits)
    }

    /// Starts a bounded retry source for exactly the actor-owned pinned attempt.
    /// Coordinator must hold permits and cancel/join before releasing them.
    pub fn generation_source(
        &self,
        attempt: &GenerationAttempt,
    ) -> Result<crate::checkpoint::stream::EncodedCheckpoint, ApplicationError> {
        let state = self
            .admission
            .as_ref()
            .ok_or_else(|| ApplicationError::port_failure("generation unavailable"))?;
        let (pinned, snapshot) = state
            .pinned
            .as_ref()
            .ok_or_else(|| ApplicationError::port_failure("generation not pinned"))?;
        if pinned != attempt || !state.binding.writer.is_live() || state.fenced {
            return Err(ApplicationError::port_failure("generation source fenced"));
        }
        let c = state.limits.chunk_bytes() as u64;
        let manifest = orbisync_application::checkpoint_stream::StreamManifest {
            serialized_bytes: attempt.serialized_bytes,
            chunk_bytes: c,
            chunk_count: 1 + (attempt.serialized_bytes - 1) / c,
            digest: attempt.digest,
        };
        crate::checkpoint::stream::EncodedCheckpoint::new(
            Arc::clone(snapshot),
            state.limits,
            manifest,
        )
    }

    fn enable_generation_admission_inner(
        &mut self,
        binding: Option<GenerationBinding>,
        limits: orbisync_application::CheckpointLimits,
        limit: usize,
        receipts: Vec<CheckpointDedupEntry>,
    ) -> Result<(), ApplicationError> {
        let Some(binding) = binding else {
            return Err(ApplicationError::port_failure(
                "generation adapter unavailable",
            ));
        };
        if self.admission.is_some()
            || !binding.writer.is_live()
            || binding.head < 0
            || !(HEADER..=64 * 1024 * 1024).contains(&limit)
            || self.state.entity_count() > MAX_ENTITIES
            || self.member_count() > 1000
            || self.descriptor.revision != self.state.revision()
        {
            return Err(ApplicationError::port_failure(
                "invalid generation admission binding",
            ));
        }
        crate::checkpoint::validate_dedup_entries(&receipts)
            .map_err(ApplicationError::port_failure)?;
        let cancelled = AtomicBool::new(false);
        let mut state = AdmissionState {
            durable_revision: self.state.revision(),
            cutoff_millis: None,
            index: GenerationInterestIndex {
                views: Arc::new(std::sync::RwLock::new(BTreeMap::new())),
                permit: binding.writer.clone(),
                fenced: Arc::new(AtomicBool::new(false)),
            },
            binding,
            limit,
            limits,
            entity_bytes: 0,
            charges: HashMap::new(),
            receipts: BTreeMap::new(),
            receipt_bytes: 0,
            logical: 0,
            pinned: None,
            proven_absent: false,
            fenced: false,
            committed_events: 0,
            committed_persistence_events: 0,
        };
        for entity in self.state.iter_entities() {
            if entity.revision().as_u64() > i64::MAX as u64 {
                return Err(ApplicationError::port_failure(
                    "entity revision exceeds BIGINT",
                ));
            }
            let bytes = entity_charge(entity, &cancelled)
                .map_err(|e| ApplicationError::port_failure(e.to_string()))?;
            state.entity_bytes += bytes;
            state.charges.insert(entity.id(), bytes);
            if state.bytes() > limit {
                return Err(ApplicationError::port_failure(
                    "restored state exceeds checkpoint capacity",
                ));
            }
            state
                .index
                .views
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(entity.id(), EntityInterestView::from_entity(entity));
        }
        for entry in receipts {
            let bytes = receipt_charge(&entry, &cancelled)
                .map_err(|e| ApplicationError::port_failure(e.to_string()))?;
            let logical = logical_bytes(&entry);
            state.receipt_bytes += bytes;
            state.logical += logical;
            state.receipts.insert(
                entry.command_id.clone(),
                Receipt {
                    entry,
                    bytes,
                    logical,
                    durable: true,
                },
            );
        }
        if state.bytes() > limit || self.state.revision().as_u64() > i64::MAX as u64 {
            return Err(ApplicationError::port_failure(
                "restored state exceeds checkpoint capacity",
            ));
        }
        self.admission = Some(state);
        Ok(())
    }
    /// Conservative complete byte bound including dirty outcomes.
    pub fn checkpoint_capacity_bytes(&self) -> Option<usize> {
        self.admission.as_ref().map(AdmissionState::bytes)
    }
    /// Whether this actor uses explicit generation admission.
    pub fn uses_generation_admission(&self) -> bool {
        self.admission.is_some()
    }
    /// Whether authoritative registry reads may use this actor's boot.
    pub fn generation_reads_available(&self) -> bool {
        self.admission
            .as_ref()
            .is_none_or(|s| !s.fenced && s.binding.writer.is_live())
    }
    pub(crate) fn generation_interest_index(&self) -> Option<GenerationInterestIndex> {
        self.admission.as_ref().map(|s| s.index.clone())
    }
    /// Receipt-bearing serialized entry point; cannot fall back to legacy mode.
    pub fn submit_prepared(
        &mut self,
        command: InstanceCommand,
        request: AdmissionRequest,
    ) -> AdmissionResult {
        self.admit(command, Some(request))
    }
    /// Restore the explicit operator baseline cutoff before actor publication.
    pub fn set_generation_cutoff(&mut self, cutoff: Option<i64>) -> Result<(), ApplicationError> {
        let state = self
            .admission
            .as_mut()
            .ok_or_else(|| ApplicationError::port_failure("generation unavailable"))?;
        state.cutoff_millis = cutoff;
        Ok(())
    }
    /// Read-only receipt lookup, allowed through a pin but never writer fencing.
    pub fn lookup_generation(
        &self,
        command_id: orbisync_domain::CommandId,
        fingerprint: [u8; 32],
        now: i64,
    ) -> Option<AdmissionResult> {
        let Some(state) = &self.admission else {
            return Some(not_admitted(
                "PERSISTENCE_UNAVAILABLE",
                "generation unavailable",
            ));
        };
        if state.fenced || !state.binding.writer.is_live() {
            return Some(not_admitted("PERSISTENCE_UNAVAILABLE", "generation fenced"));
        }
        if let Some(cutoff) = state.cutoff_millis {
            let issued = command_id.as_uuid().get_timestamp().and_then(|t| {
                let (seconds, nanos) = t.to_unix();
                i64::try_from(seconds.saturating_mul(1000) + u64::from(nanos / 1_000_000)).ok()
            });
            if issued.is_none_or(|issued| issued < cutoff) {
                return Some(not_admitted(
                    "COMMAND_BEFORE_BASELINE",
                    "command predates approved reconciliation cutoff",
                ));
            }
        }
        if let Some(receipt) = state.receipts.get(&command_id.to_string()) {
            if receipt.entry.fingerprint != fingerprint {
                return Some(not_admitted(
                    "COMMAND_ID_CONFLICT",
                    "command fingerprint differs",
                ));
            }
            if receipt.durable && receipt.entry.expires_at_millis <= now {
                return Some(AdmissionResult {
                    phase: AdmissionPhase::Expired,
                    outcome: rejected(
                        "REPLAY_WINDOW_EXPIRED",
                        "retained outcome expired; do not reapply",
                    ),
                    receipt: None,
                });
            }
            return Some(AdmissionResult {
                phase: AdmissionPhase::Replay,
                outcome: rejected(
                    if receipt.durable {
                        "COMMAND_REPLAY"
                    } else {
                        "PERSISTENCE_UNAVAILABLE"
                    },
                    "retained outcome; do not reapply",
                ),
                receipt: Some(receipt.entry.clone()),
            });
        }
        if !state.available() {
            return Some(not_admitted(
                "PERSISTENCE_UNAVAILABLE",
                "generation admission pinned",
            ));
        }
        if state.bytes().saturating_add(OUTCOME_RESERVATION) > state.limit
            || state.receipts.len() >= MAX_RECEIPTS
            || state.logical.saturating_add(RECEIPT_BYTES) > MAX_LOGICAL
        {
            return Some(not_admitted(
                CAPACITY_CODE,
                "checkpoint receipt capacity unavailable",
            ));
        }
        None
    }
    /// Deterministic transport rejection enters the same bounded receipt ledger.
    pub fn reject_prepared(
        &mut self,
        request: AdmissionRequest,
        code: &'static str,
        detail: String,
    ) -> AdmissionResult {
        self.admit_inner(
            InstanceCommand::Tick,
            Some(request),
            Some(rejected(code, detail)),
        )
    }
    pub(super) fn admit(
        &mut self,
        command: InstanceCommand,
        request: Option<AdmissionRequest>,
    ) -> AdmissionResult {
        self.admit_inner(command, request, None)
    }
    fn admit_inner(
        &mut self,
        command: InstanceCommand,
        request: Option<AdmissionRequest>,
        rejection: Option<CommandOutcome>,
    ) -> AdmissionResult {
        if let Some(request) = &request
            && let Some(result) =
                self.lookup_generation(request.command_id, request.fingerprint, request.now_millis)
        {
            return result;
        }
        let Some(admission) = self.admission.as_ref() else {
            return not_admitted("PERSISTENCE_UNAVAILABLE", "generation adapter unavailable");
        };
        if admission.fenced || !admission.binding.writer.is_live() {
            return not_admitted("PERSISTENCE_UNAVAILABLE", "generation admission fenced");
        }
        let never_cancelled = AtomicBool::new(false);
        let cancelled = request
            .as_ref()
            .map_or(&never_cancelled, |r| r.cancelled.as_ref());
        if cancelled.load(Ordering::Acquire) {
            return not_admitted("COMMAND_CANCELLED", "cancelled before application");
        }
        if !bounded_command(&command) {
            return not_admitted(CAPACITY_CODE, "command preparation bound exceeded");
        }
        if let Some(id) = command_identity(&command)
            && request.as_ref().is_none_or(|r| r.command_id != id)
        {
            return not_admitted(
                "ADMISSION_REQUIRED",
                "command identity requires prepared outcome",
            );
        }
        if let Some(request) = &request {
            if orbisync_domain::CommandId::parse(&request.message_id.to_string()).is_err()
                || request.now_millis.checked_add(TTL).is_none()
            {
                return not_admitted("INVALID_ARGUMENT", "invalid outcome metadata");
            }
            // Reserve before deletion credit. This synchronous turn owns the
            // reservation on its stack; no other ingress can observe it partly.
            if admission.bytes().saturating_add(OUTCOME_RESERVATION) > admission.limit
                || admission.receipts.len() >= MAX_RECEIPTS
                || admission.logical.saturating_add(RECEIPT_BYTES) > MAX_LOGICAL
            {
                return not_admitted(CAPACITY_CODE, "checkpoint receipt capacity unavailable");
            }
        }
        if !admission.available() {
            return not_admitted("PERSISTENCE_UNAVAILABLE", "generation admission pinned");
        }
        let id = target(&command);
        let mut scratch = self.detached_candidate(&command);
        let mut outcome = if let Some(rejection) = rejection {
            rejection
        } else if self.state.revision().as_u64() >= i64::MAX as u64 {
            rejected("REVISION_OVERFLOW", "instance revision exceeds BIGINT")
        } else if matches!(&command, InstanceCommand::Join { capacity, .. } if self.member_count() >= *capacity as usize)
        {
            rejected("INSTANCE_FULL", "instance is full")
        } else {
            scratch.handle_legacy(command.clone())
        };
        let candidate = id.and_then(|id| scratch.state.get_entity(id));
        let old_charge = id
            .and_then(|id| admission.charges.get(&id))
            .copied()
            .unwrap_or(0);
        let new_count = admission.charges.len() - usize::from(old_charge != 0)
            + usize::from(candidate.is_some());
        let new_entity_bytes = admission.entity_bytes - old_charge;
        let mut candidate_charge = 0;
        if matches!(outcome, CommandOutcome::Applied { .. }) {
            match candidate
                .map(|entity| entity_charge(entity, cancelled))
                .transpose()
            {
                Ok(bytes) => {
                    candidate_charge = bytes.unwrap_or(0);
                    let reserved = if request.is_some() {
                        OUTCOME_RESERVATION
                    } else {
                        0
                    };
                    let total = HEADER
                        + new_entity_bytes
                        + candidate_charge
                        + new_count.saturating_sub(1)
                        + admission.receipt_bytes
                        + admission.receipts.len().saturating_sub(1)
                        + reserved;
                    if new_count > MAX_ENTITIES
                        || total > admission.limit
                        || self.outbox.len() + scratch.outbox.len() > self.outbox.capacity()
                        || self.persistence_outbox.len() + scratch.persistence_outbox.len()
                            > self.persistence_outbox.capacity()
                        || candidate.is_some_and(|e| e.revision().as_u64() > i64::MAX as u64)
                    {
                        outcome = rejected(CAPACITY_CODE, CAPACITY_DETAIL);
                    }
                }
                Err(_) => outcome = rejected(CAPACITY_CODE, CAPACITY_DETAIL),
            }
        }
        let mut entry = None;
        if let Some(request) = &request {
            let result = match &outcome {
                CommandOutcome::Applied {
                    revision,
                    entity_revision,
                    committed_entity,
                } => {
                    match request.encoder.encode(
                        ProspectiveOutcome::Applied {
                            revision: *revision,
                            entity_revision: *entity_revision,
                            entity: committed_entity.as_deref(),
                        },
                        cancelled,
                    ) {
                        Ok(bytes) if !bytes.is_empty() && bytes.len() <= RESPONSE_BYTES => {
                            CheckpointDedupResult::Applied {
                                response_payload: bytes,
                            }
                        }
                        _ => {
                            outcome = rejected(CAPACITY_CODE, CAPACITY_DETAIL);
                            CheckpointDedupResult::Rejected {
                                code: CAPACITY_CODE.into(),
                                detail: CAPACITY_DETAIL.into(),
                            }
                        }
                    }
                }
                CommandOutcome::Rejected { code, detail } => CheckpointDedupResult::Rejected {
                    code: (*code).into(),
                    detail: detail.clone(),
                },
            };
            entry = Some(CheckpointDedupEntry {
                command_id: request.command_id.to_string(),
                fingerprint: request.fingerprint.to_vec(),
                message_id: request.message_id.to_string(),
                created_at_millis: request.now_millis,
                expires_at_millis: request.now_millis + TTL,
                result,
            });
        }
        let receipt = match entry {
            Some(entry) => {
                if crate::checkpoint::validate_dedup_entries(std::slice::from_ref(&entry)).is_err()
                {
                    return not_admitted(CAPACITY_CODE, "outcome preparation bound exceeded");
                }
                let logical = logical_bytes(&entry);
                match receipt_charge(&entry, cancelled) {
                    Ok(bytes) if bytes < OUTCOME_RESERVATION && logical <= RECEIPT_BYTES => {
                        Some(Receipt {
                            entry,
                            bytes,
                            logical,
                            durable: false,
                        })
                    }
                    _ => return not_admitted(CAPACITY_CODE, "outcome preparation bound exceeded"),
                }
            }
            None => None,
        };
        if cancelled.load(Ordering::Acquire) {
            return not_admitted("COMMAND_CANCELLED", "cancelled before application");
        }
        if !admission.binding.writer.is_live() {
            return not_admitted("PERSISTENCE_UNAVAILABLE", "writer permit invalidated");
        }
        // Local commit: no fallible external calls below this point.
        if matches!(outcome, CommandOutcome::Applied { .. }) {
            self.commit_candidate(&command, scratch);
            if let (Some(admission), Some(id)) = (&mut self.admission, id) {
                admission.entity_bytes = new_entity_bytes + candidate_charge;
                if candidate_charge == 0 {
                    admission.charges.remove(&id);
                } else {
                    admission.charges.insert(id, candidate_charge);
                }
            }
        }
        let result_entry = receipt.as_ref().map(|r| r.entry.clone());
        if let (Some(admission), Some(receipt)) = (&mut self.admission, receipt) {
            admission.receipt_bytes += receipt.bytes;
            admission.logical += receipt.logical;
            admission
                .receipts
                .insert(receipt.entry.command_id.clone(), receipt);
        }
        AdmissionResult {
            phase: AdmissionPhase::Completed,
            outcome,
            receipt: result_entry,
        }
    }
    fn detached_candidate(&self, command: &InstanceCommand) -> Self {
        let mut scratch = Self::with_runtime_limits(
            self.descriptor,
            self.avatar_visibility_radius,
            2,
            self.max_speed,
            self.max_acceleration,
        );
        scratch.state.install_revision(self.state.revision());
        if let Some(entity) = target(command).and_then(|id| self.state.get_entity(id)) {
            scratch.state.upsert_entity(entity.clone());
        }
        if let InstanceCommand::Leave { presence_id } = command
            && let Some(user) = self.state.user_for_presence(*presence_id)
        {
            let _inserted = scratch.state.add_member(*presence_id, user, 1);
        }
        scratch.component_updates_per_sec = self.component_updates_per_sec;
        scratch.component_update_tokens = self.component_update_tokens;
        scratch.component_update_last_refill = self.component_update_last_refill;
        scratch.speed_acceleration_check_enabled = self.speed_acceleration_check_enabled;
        scratch.spawn_hook_active = Arc::clone(&self.spawn_hook_active);
        scratch
    }
    fn commit_candidate(&mut self, command: &InstanceCommand, mut scratch: Self) {
        if let Some(id) = target(command) {
            if let Some(entity) = scratch.state.remove_entity(id) {
                if let Some(admission) = &self.admission {
                    admission
                        .index
                        .views
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id, EntityInterestView::from_entity(&entity));
                }
                self.state.upsert_entity(entity);
            } else {
                if let Some(admission) = &self.admission {
                    admission
                        .index
                        .views
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                }
                self.state.remove_entity(id);
            }
        }
        match command {
            InstanceCommand::Join {
                presence_id,
                user_id,
                capacity,
                ..
            } => {
                let _inserted = self.state.add_member(*presence_id, *user_id, *capacity);
            }
            InstanceCommand::Leave { presence_id } => {
                self.state.remove_member(*presence_id);
            }
            _ => {}
        }
        self.state.install_revision(scratch.state.revision());
        self.descriptor = scratch.descriptor;
        self.component_update_tokens = scratch.component_update_tokens;
        self.component_update_last_refill = scratch.component_update_last_refill;
        for event in scratch.outbox.drain() {
            self.push_extension_event(event);
        }
        for event in scratch.persistence_outbox.drain() {
            self.push_persistence_event(event);
        }
        self.push_history(self.state.revision());
    }
    /// Expire only durable receipts; dirty outcomes never expire implicitly.
    pub fn expire_generation_receipts(&mut self, now_millis: i64) {
        let Some(state) = &mut self.admission else {
            return;
        };
        if !state.available() {
            return;
        }
        state.receipts.retain(|_, r| {
            let retain = !r.durable || r.entry.expires_at_millis > now_millis;
            if !retain {
                state.receipt_bytes -= r.bytes;
                state.logical -= r.logical;
            }
            retain
        });
    }

    /// Pins actor-owned state and incrementally counts/hashes the generation.
    /// Caller must acquire work permits first. No full JSON payload is retained.
    pub fn capture_generation(
        &mut self,
        now: Timestamp,
    ) -> Result<(GenerationAttempt, Arc<Checkpoint>), ApplicationError> {
        let error = || ApplicationError::port_failure("generation capture unavailable");
        let state = self.admission.as_ref().ok_or_else(error)?;
        if !state.binding.writer.is_live() || state.fenced {
            return Err(error());
        }
        if let Some(pinned) = state.pinned.clone() {
            // Handing the attempt to a new retry invalidates earlier absence
            // proof. That proof cannot authorize retirement of an in-flight retry.
            if let Some(state) = &mut self.admission {
                state.proven_absent = false;
            }
            return Ok(pinned);
        }
        let now_millis = now.to_unix_millis().map_err(|_| error())?;
        let expiry = now_millis.checked_add(TTL).ok_or_else(error)?;
        let mut snapshot = self.build_checkpoint(now);
        for entry in &mut snapshot.dedup {
            if state
                .receipts
                .get(&entry.command_id)
                .is_some_and(|r| !r.durable)
            {
                entry.created_at_millis = now_millis;
                entry.expires_at_millis = expiry;
            }
        }
        let manifest = snapshot.stream_manifest(state.limits, &AtomicBool::new(false))?;
        if manifest.serialized_bytes > state.bytes() as u64
            || manifest.serialized_bytes > state.limit as u64
        {
            if let Some(state) = &mut self.admission {
                state.fence();
            }
            return Err(ApplicationError::port_failure(
                "checkpoint ledger undercount",
            ));
        }
        let attempt = GenerationAttempt::from_manifest(
            self.state.instance_id(),
            state.binding.head,
            state.binding.writer.token(),
            now_millis,
            orbisync_application::checkpoint_record::canonical::VERSION,
            state.limits,
            &manifest,
        )?;
        let pinned = (attempt, Arc::new(snapshot));
        if let Some(state) = &mut self.admission {
            // Adopt pinned times together; original durable receipt times survive.
            for entry in &pinned.1.dedup {
                if let Some(receipt) = state.receipts.get_mut(&entry.command_id) {
                    receipt.entry = entry.clone();
                }
            }
            state.pinned = Some(pinned.clone());
            state.proven_absent = false;
        }
        Ok(pinned)
    }

    /// Applies evidence returned by the dedicated generation adapter. A timeout
    /// must be Uncertain, never ProvenAbsent. No legacy save receipt is accepted.
    pub fn resolve_generation(
        &mut self,
        attempt: &GenerationAttempt,
        resolution: GenerationResolution,
        now_millis: i64,
    ) -> Result<(), ApplicationError> {
        let error = || ApplicationError::port_failure("generation remains fenced");
        let state = self.admission.as_mut().ok_or_else(error)?;
        if !state.binding.writer.is_live() || state.fenced {
            return Err(error());
        }
        if state.pinned.as_ref().is_none_or(|p| &p.0 != attempt) {
            state.fence();
            return Err(error());
        }
        match resolution {
            GenerationResolution::Uncertain => {
                state.proven_absent = false;
                Err(error())
            }
            GenerationResolution::Fenced => {
                state.fence();
                Err(error())
            }
            GenerationResolution::ProvenAbsent => {
                state.proven_absent = true;
                Ok(())
            }
            GenerationResolution::Committed { publish_seq } => {
                if attempt.expected_head.checked_add(1) != Some(publish_seq) {
                    state.fence();
                    return Err(error());
                }
                let expired = state
                    .receipts
                    .values()
                    .any(|r| !r.durable && r.entry.expires_at_millis <= now_millis);
                for receipt in state.receipts.values_mut() {
                    receipt.durable = true;
                }
                state.binding.head = publish_seq;
                state.durable_revision = self.state.revision();
                // Admission is pinned during publication, so these prefixes
                // contain exactly the effects covered by the committed attempt.
                state.committed_events = self.outbox.len();
                state.committed_persistence_events = self.persistence_outbox.len();
                state.pinned = None;
                state.proven_absent = false;
                if expired {
                    return Err(ApplicationError::new(
                        orbisync_application::ApplicationErrorKind::CommittedButExpired,
                        "replay window expired after commit; do not reapply",
                    ));
                }
                Ok(())
            }
        }
    }

    /// Retires an aborted attempt only after explicit proof. Dirty identities
    /// and results stay owned; a later capture allocates a fresh UUID and time.
    pub fn retire_absent_generation(
        &mut self,
        attempt: &GenerationAttempt,
    ) -> Result<(), ApplicationError> {
        let state = self
            .admission
            .as_mut()
            .ok_or_else(|| ApplicationError::port_failure("generation unavailable"))?;
        if !state.proven_absent
            || state.fenced
            || !state.binding.writer.is_live()
            || state.pinned.as_ref().is_none_or(|p| &p.0 != attempt)
        {
            return Err(ApplicationError::port_failure(
                "absence has not been proven",
            ));
        }
        state.pinned = None;
        state.proven_absent = false;
        Ok(())
    }
    pub(super) fn generation_start_allowed(&self) -> bool {
        self.admission.as_ref().is_none_or(|state| {
            self.descriptor.state == RuntimeState::Starting && state.available()
        })
    }
}
fn bounded_command(command: &InstanceCommand) -> bool {
    match command {
        InstanceCommand::WithExpectedEntityState {
            command, expected, ..
        } => {
            !matches!(
                command.as_ref(),
                InstanceCommand::WithExpectedEntityState { .. }
            ) && expected
                .as_ref()
                .is_none_or(|e| entity_charge(e, &AtomicBool::new(false)).is_ok())
                && bounded_command(command)
        }
        InstanceCommand::UpdateEntityComponent {
            component_key,
            payload_bytes,
            ..
        } => component_key.len() <= 256 && payload_bytes.len() <= 4096,
        InstanceCommand::SpawnEntity { visibility, .. } => match visibility {
            VisibilityPolicy::RoleRestricted { roles } => (1..=16).contains(&roles.len()),
            VisibilityPolicy::Explicit { users } => (1..=64).contains(&users.len()),
            VisibilityPolicy::Spatial { radius } => VisibilityPolicy::spatial(*radius).is_ok(),
            VisibilityPolicy::Custom { tag } => tag.as_str().len() <= 1024 * 1024 / 6,
            _ => true,
        },
        InstanceCommand::Join { capacity, .. } => (1..=1000).contains(capacity),
        InstanceCommand::DeleteEntity { .. }
        | InstanceCommand::TransferOwnership { .. }
        | InstanceCommand::UpdateTransform { .. }
        | InstanceCommand::Leave { .. }
        | InstanceCommand::RetainReliable { .. }
        | InstanceCommand::PublishEvent { .. }
        | InstanceCommand::Tick
        | InstanceCommand::Shutdown => true,
    }
}
fn logical_bytes(entry: &CheckpointDedupEntry) -> usize {
    entry.command_id.len()
        + entry.message_id.len()
        + entry.fingerprint.len()
        + match &entry.result {
            CheckpointDedupResult::Applied { response_payload } => response_payload.len(),
            CheckpointDedupResult::Rejected { code, detail } => code.len() + detail.len(),
        }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
