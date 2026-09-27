//! Generation persistence and prospective-outcome contracts. Legacy saves do
//! not implement these contracts; a missing generation adapter is unavailable.

use crate::ApplicationError;
use orbisync_domain::{CommandId, Entity, InstanceId, Revision};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

/// Maximum opaque response bytes.
pub const RESPONSE_BYTES: usize = 262_144;
/// Maximum logical fields in one receipt.
pub const RECEIPT_BYTES: usize = 524_288;
/// Conservative JSON reservation, including identity and separator.
pub const OUTCOME_RESERVATION: usize = 4 * RESPONSE_BYTES + 6 * (128 + 8192) + 4096;
/// Stable capacity error code.
pub const CAPACITY_CODE: &str = "CHECKPOINT_CAPACITY";
/// Stable admitted capacity rejection detail.
pub const CAPACITY_DETAIL: &str = "persistent state capacity exceeded";

/// Writer identity acquired by the phase 3 startup gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterToken {
    /// Positive database epoch.
    pub epoch: i64,
    /// Boot identity, never replaced in a running process.
    pub boot: Uuid,
}

/// Revocable actor capability. Startup must supply the verified token; this
/// object alone is not evidence of deployment singleton ownership.
#[derive(Debug, Clone)]
pub struct WriterPermit {
    token: WriterToken,
    live: Arc<AtomicBool>,
}
impl WriterPermit {
    /// Constructs a capability after the external writer gate succeeds.
    pub fn new(token: WriterToken) -> Result<Self, ApplicationError> {
        if token.epoch <= 0 || token.boot.is_nil() {
            return Err(ApplicationError::port_failure("invalid writer token"));
        }
        Ok(Self {
            token,
            live: Arc::new(AtomicBool::new(true)),
        })
    }
    /// Invalidates all clones permanently on writer connection loss.
    pub fn invalidate(&self) {
        self.live.store(false, Ordering::Release);
    }
    /// Whether this boot may still admit work.
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
    /// Original writer identity; never refreshed during retry.
    pub fn token(&self) -> WriterToken {
        self.token
    }
}

/// Immutable publication attempt. Construct once after acquiring work permits
/// and freezing coherent actor state. Retry all fields unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationAttempt {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Fixed generation identity.
    pub generation_id: Uuid,
    /// Expected current publication sequence (zero only for proven new state).
    pub expected_head: i64,
    /// Original writer capability identity.
    pub writer: WriterToken,
    /// Pinned completion time, not acknowledgement time.
    pub completed_at_millis: i64,
    /// Deterministic codec version.
    pub codec: u32,
    /// Exact emitted length.
    pub serialized_bytes: u64,
    /// Fixed stream partition size; retries cannot change configuration.
    pub chunk_bytes: u64,
    /// Fixed number of ordered chunks.
    pub chunk_count: u64,
    /// Digest binding the immutable contents.
    pub digest: [u8; 32],
}

impl GenerationAttempt {
    /// Pin incrementally measured bytes without constructing a whole payload.
    pub fn from_manifest(
        instance_id: InstanceId,
        expected_head: i64,
        writer: WriterToken,
        completed_at_millis: i64,
        codec: u32,
        limits: crate::CheckpointLimits,
        manifest: &crate::checkpoint_stream::StreamManifest,
    ) -> Result<Self, ApplicationError> {
        limits.validate_manifest(manifest)?;
        Ok(Self {
            instance_id,
            generation_id: Uuid::now_v7(),
            expected_head,
            writer,
            completed_at_millis,
            codec,
            serialized_bytes: manifest.serialized_bytes,
            chunk_bytes: manifest.chunk_bytes,
            chunk_count: manifest.chunk_count,
            digest: manifest.digest,
        })
    }

    /// Pins identity and content metadata for the bounded phase 1 codec. Phase 2
    /// computes the same digest/length incrementally from its immutable source.
    pub fn from_payload(
        instance_id: InstanceId,
        expected_head: i64,
        writer: WriterToken,
        completed_at_millis: i64,
        codec: u32,
        payload: &[u8],
    ) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            instance_id,
            generation_id: Uuid::now_v7(),
            expected_head,
            writer,
            completed_at_millis,
            codec,
            serialized_bytes: payload.len() as u64,
            chunk_bytes: crate::CheckpointLimits::default().chunk_bytes() as u64,
            chunk_count: payload
                .len()
                .div_ceil(crate::CheckpointLimits::default().chunk_bytes())
                as u64,
            digest: Sha256::digest(payload).into(),
        }
    }
}

/// Primary-store resolution evidence for the exact supplied attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationResolution {
    /// The exact identity/content committed; sequence must be expected + 1.
    Committed {
        /// Verified publication sequence.
        publish_seq: i64,
    },
    /// Original transaction ended and the generation is absent under the
    /// instance lock. An ordinary missing row or timeout is not this proof.
    ProvenAbsent,
    /// Resolution unavailable. Keep the attempt pinned and actor frozen.
    Uncertain,
    /// Unexpected head, writer or contents. Recovery is required.
    Fenced,
}

/// Compatibility name for the shared metadata-bearing stream contract.
pub use crate::checkpoint_stream::CheckpointSource as GenerationSource;

/// Phase 3 adapter connection point. Deliberately separate from CheckpointStore.
#[async_trait::async_trait]
pub trait GenerationStore: Send + Sync + std::fmt::Debug {
    /// Join retained terminal transaction/connection cleanup before releasing a job.
    /// Repeated or interrupted observers must retain the underlying owners.
    async fn finish_cleanup(&self) {}

    /// Publish exact immutable bytes under writer/expected-head fencing.
    async fn publish(
        &self,
        attempt: &GenerationAttempt,
        source: &mut dyn GenerationSource,
    ) -> Result<GenerationResolution, ApplicationError>;
    /// Validated policy for admission/codec/store consistency.
    fn limits(&self) -> crate::CheckpointLimits;
    /// Resolve on primary, waiting out the original transaction under the
    /// instance lock. Errors leave ownership uncertain.
    async fn resolve(
        &self,
        attempt: &GenerationAttempt,
    ) -> Result<GenerationResolution, ApplicationError>;
    /// Retire storage-side ownership only after matching primary absence proof.
    /// The coordinator then retires the actor's matching pinned snapshot. This
    /// order preserves recoverability if cancellation interrupts retirement.
    async fn retire_absent(&self, _attempt: &GenerationAttempt) -> Result<(), ApplicationError> {
        Err(ApplicationError::port_failure(
            "generation retirement unavailable",
        ))
    }
}

/// Selected primary generation and its fixed snapshot cursor. Missing or blocked
/// authority is an error; implementations must never select an older fallback.
pub struct SelectedGeneration {
    /// Immutable selected identity and sequence.
    pub attempt: GenerationAttempt,
    /// Cursor over that identity only.
    pub source: Box<dyn GenerationSource>,
    /// Explicit baseline command issuance cutoff, when reconciliation reset history.
    pub cutoff_millis: Option<i64>,
}

/// Exact operator-reviewed selection. No runtime path creates this approval.
#[derive(Debug, Clone)]
pub struct ReconciledAuthority {
    /// Fixed legacy UUID, absent only for independently proven empty state.
    pub source_id: Option<Uuid>,
    /// Digest of the exact selected source.
    pub source_digest: Option<[u8; 32]>,
    /// Exact persisted approval report, compared again under the instance lock.
    pub report: String,
}

/// Both preserved legacy sources, fetched from one fixed database snapshot.
pub struct LegacyInventory {
    /// Fixed UUID selected before fetching the payload.
    pub source_id: Option<Uuid>,
    /// Digest of the exact storage representation for later revalidation.
    pub source_digest: Option<[u8; 32]>,
    /// Selected legacy payload and original metadata.
    pub checkpoint: Option<crate::AppCheckpoint>,
    /// Persistent entity/component rows, never automatically overlaid.
    pub rows: Vec<Entity>,
}

/// Recovery and row projection ports composed with the publication store.
#[async_trait::async_trait]
pub trait GenerationRecovery: GenerationStore {
    /// One bounded cleanup pass, retaining heads, predecessors and projection pins.
    async fn cleanup(&self, _instance: InstanceId) -> Result<u64, ApplicationError> {
        Ok(0)
    }
    /// Read-only fixed-snapshot operator inventory, under the conversion budget.
    async fn inventory(&self, _instance: InstanceId) -> Result<LegacyInventory, ApplicationError> {
        Err(ApplicationError::port_failure(
            "legacy inventory unavailable",
        ))
    }
    /// Bounded page of durable projection lag, including inactive instances.
    async fn pending_projections(
        &self,
        _after: Option<InstanceId>,
    ) -> Result<Vec<InstanceId>, ApplicationError> {
        Ok(Vec::new())
    }
    /// Select the authoritative head in a consistent primary snapshot.
    async fn select(&self, instance: InstanceId) -> Result<SelectedGeneration, ApplicationError>;
    /// Replace the complete row projection, including deletions, and advance its
    /// applied sequence atomically only if the selected head is still current.
    async fn project(
        &self,
        selected: &GenerationAttempt,
        entities: &[Entity],
    ) -> Result<(), ApplicationError>;
    /// Publish only an externally approved, source-bound reconciliation.
    async fn convert(
        &self,
        _attempt: &GenerationAttempt,
        _source: &mut dyn GenerationSource,
        _approval: &ReconciledAuthority,
    ) -> Result<GenerationResolution, ApplicationError> {
        Err(ApplicationError::port_failure("reconciliation unavailable"))
    }
}

/// Prospective result passed to the transport encoder before actor mutation.
#[derive(Debug)]
pub enum ProspectiveOutcome<'a> {
    /// Candidate state (pre-removal for delete).
    Applied {
        /// Next instance revision.
        revision: Revision,
        /// Next entity revision, absent for delete/control.
        entity_revision: Option<Revision>,
        /// Detached entity.
        entity: Option<&'a Entity>,
    },
    /// Stable deterministic rejection.
    Rejected {
        /// Stable code.
        code: &'static str,
        /// Bounded detail.
        detail: &'a str,
    },
}

/// Opaque response preparation. Implementations must bound work and allocation
/// to RESPONSE_BYTES, honor cancellation, and check actual encoded length.
pub trait PreparedOutcomeEncoder: Send + Sync + std::fmt::Debug {
    /// Encode the prospective applied response; runtime checks the result again.
    fn encode(
        &self,
        outcome: ProspectiveOutcome<'_>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, ApplicationError>;
}

/// Transport identity and encoder carried to the serialized actor turn.
#[derive(Debug, Clone)]
pub struct AdmissionRequest {
    /// Retry identity.
    pub command_id: CommandId,
    /// Fingerprint of the authorized logical command.
    pub fingerprint: [u8; 32],
    /// Stable envelope identity.
    pub message_id: Uuid,
    /// Preparation clock; generation capture supplies final completion time.
    pub now_millis: i64,
    /// Precommit cancellation. After commit the actor retains ownership.
    pub cancelled: Arc<AtomicBool>,
    /// Transport encoder, independent of runtime implementation.
    pub encoder: Arc<dyn PreparedOutcomeEncoder>,
}

/// Phase is explicit: completed local outcomes still need durable publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPhase {
    /// No mutation or receipt; same identity may be admitted later.
    NotAdmitted,
    /// Actor installed an immutable outcome. This does not mean durable ACK.
    Completed,
    /// Existing identity; replay only after durable publication.
    Replay,
    /// Retained durable identity is past its replay window; do not reapply.
    Expired,
}
