//! Checkpoint persistence (`state-and-runtime.md` §1.3, spec §10.3).
//!
//! Captures a point-in-time snapshot of an instance for persistence. Only
//! persistent entities are included; for M4 all entities are treated as
//! persistent for the demo, but the filtering path is `is_persistent`.

pub mod stream;

use std::collections::{BTreeMap, HashMap, HashSet};

use orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES;
use orbisync_domain::{
    CommandId, Entity, EntityId, EntityKind, InstanceId, Quaternion, Revision, RoleId, Timestamp,
    Transform, UserId, Vec3, VisibilityPolicy,
};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

const CHECKPOINT_FORMAT_VERSION: u32 = 4;

/// Hard ceilings for untrusted checkpoint input and retained command outcomes.
/// These are deliberately enforced before JSON decoding and before encoding so
/// a corrupt row cannot turn into an unbounded allocation.
pub const MAX_CHECKPOINT_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded checkpoint size produced for durable storage.
pub const MAX_CHECKPOINT_SERIALIZED_BYTES: usize = MAX_CHECKPOINT_PAYLOAD_BYTES;
/// Maximum number of dedup records in one checkpoint.
pub const MAX_DEDUP_RECORDS: usize = 4_096;
/// Maximum encoded replay response size.
pub const MAX_DEDUP_RESPONSE_BYTES: usize = orbisync_application::checkpoint_record::RESPONSE_BYTES;
/// Maximum combined field size of one dedup record.
pub const MAX_DEDUP_RECORD_BYTES: usize = 512 * 1024;
/// Maximum deterministic error code size.
pub const MAX_DEDUP_CODE_BYTES: usize = orbisync_application::checkpoint_record::CODE_BYTES;
/// Maximum deterministic error detail size.
pub const MAX_DEDUP_DETAIL_BYTES: usize = orbisync_application::checkpoint_record::DETAIL_BYTES;
/// Maximum aggregate size of dedup fields and payloads.
pub const MAX_DEDUP_AGGREGATE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum retention interval for a dedup record.
pub const MAX_DEDUP_TTL_MILLIS: i64 = 24 * 60 * 60 * 1_000;

/// Failure to decode or validate a durable runtime checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointDecodeError {
    detail: String,
}

impl CheckpointDecodeError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl core::fmt::Display for CheckpointDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for CheckpointDecodeError {}

#[derive(Debug, Serialize, Deserialize)]
struct StoredCheckpoint {
    #[serde(default = "legacy_format_version")]
    format_version: u32,
    instance_id: String,
    revision: u64,
    timestamp: String,
    entities: Vec<StoredEntity>,
    #[serde(default)]
    dedup: Vec<StoredDedupEntry>,
}

const fn legacy_format_version() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredEntity {
    id: String,
    instance_id: String,
    kind: String,
    owner: Option<String>,
    transform: Option<StoredTransform>,
    visibility: StoredVisibilityWire,
    revision: u64,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    components: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredTransform {
    position: StoredVec3,
    rotation: StoredQuaternion,
    scale: StoredVec3,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredVec3 {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredQuaternion {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredVisibilityWire {
    Structured(StoredVisibility),
    Legacy(String),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StoredVisibility {
    Global,
    Spatial { radius: f32 },
    OwnerOnly,
    RoleRestricted { roles: Vec<String> },
    Explicit { users: Vec<String> },
    Custom { tag: String },
}

/// Protocol-neutral durable record for a realtime command outcome.
///
/// The server stores the protobuf response bytes as an opaque payload so the
/// runtime checkpoint remains independent from the realtime transport crate.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointDedupEntry {
    /// Canonical UUIDv7 command identifier.
    pub command_id: String,
    /// SHA-256 fingerprint of the command payload without its ID.
    pub fingerprint: Vec<u8>,
    /// Creation time in Unix milliseconds.
    pub created_at_millis: i64,
    /// Expiry time in Unix milliseconds.
    pub expires_at_millis: i64,
    /// Stable envelope message identifier reused for every replay.
    pub message_id: String,
    /// Deterministic response retained for replay.
    pub result: CheckpointDedupResult,
}

/// Deterministic command outcome retained in a checkpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckpointDedupResult {
    /// Successful response/event encoded as a protobuf payload.
    Applied {
        /// Encoded `EntityCommand` response/event.
        response_payload: Vec<u8>,
    },
    /// Stable protocol rejection.
    Rejected {
        /// Stable protocol error code.
        code: String,
        /// Human-readable deterministic detail.
        detail: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredDedupEntry {
    #[serde(deserialize_with = "orbisync_application::checkpoint_record::bounded_string::<_, 36>")]
    command_id: String,
    #[serde(deserialize_with = "orbisync_application::checkpoint_record::bounded_bytes::<_, 32>")]
    fingerprint: Vec<u8>,
    created_at_millis: i64,
    expires_at_millis: i64,
    #[serde(deserialize_with = "orbisync_application::checkpoint_record::bounded_string::<_, 36>")]
    message_id: String,
    result: StoredDedupResult,
}

type StoredDedupResult = orbisync_application::checkpoint_record::ReceiptResult;

/// Checkpoint of a world instance (`state-and-runtime.md` §1.3).
///
/// Produced by `instance_runtime` and handed to the `persistence` port. The
/// checkpoint contains only persistent entities (spec §10.3); high-frequency
/// ephemeral state such as position/velocity is not persisted every tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    /// Instance owning the snapshot.
    pub instance_id: InstanceId,
    /// Revision at checkpoint time.
    pub revision: Revision,
    /// Snapshot of persistent entities.
    pub entities: Vec<Entity>,
    /// Wall-clock time of checkpoint creation (UTC).
    pub timestamp: Timestamp,
    /// Bounded realtime idempotency outcomes retained with the instance.
    pub dedup: Vec<CheckpointDedupEntry>,
}

/// Wrapper used to filter persistent vs ephemeral entities.
///
/// For M4 the demo treats all entities as persistent (`is_persistent = true`),
/// but callers can mark individual entities as ephemeral to exercise the
/// filter.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointEntity {
    /// Entity to maybe checkpoint.
    pub entity: Entity,
    /// Whether this entity should be included in a checkpoint.
    pub is_persistent: bool,
}

impl Checkpoint {
    /// Explicit legacy compatibility reader: the smaller of current read policy
    /// and the legacy 8 MiB contract, with no fallback or source mutation.
    pub fn from_legacy_json_with_limits(
        bytes: &[u8],
        limits: orbisync_application::CheckpointLimits,
    ) -> Result<Self, CheckpointDecodeError> {
        if bytes.len() > limits.max_serialized_bytes() {
            return Err(CheckpointDecodeError::new(
                "selected checkpoint exceeds configured read policy",
            ));
        }
        Self::from_json_bytes(bytes)
    }

    fn validate_payload_size(size: usize) -> Result<(), CheckpointDecodeError> {
        if size > MAX_CHECKPOINT_PAYLOAD_BYTES {
            return Err(CheckpointDecodeError::new(format!(
                "checkpoint payload exceeds {} byte limit",
                MAX_CHECKPOINT_PAYLOAD_BYTES
            )));
        }
        Ok(())
    }

    /// Creates a checkpoint from an already-filtered entity list.
    #[must_use]
    pub fn new(
        instance_id: InstanceId,
        revision: Revision,
        entities: Vec<Entity>,
        timestamp: Timestamp,
    ) -> Self {
        Self {
            instance_id,
            revision,
            entities,
            timestamp,
            dedup: Vec::new(),
        }
    }

    /// Creates a checkpoint from mixed persistent/ephemeral snapshots,
    /// filtering to `is_persistent == true`.
    ///
    /// This is the M4 persistence filter: only persistent components/entities
    /// are saved (spec §10.3).
    #[must_use]
    pub fn from_persistent_snapshots(
        instance_id: InstanceId,
        revision: Revision,
        snapshots: Vec<CheckpointEntity>,
        timestamp: Timestamp,
    ) -> Self {
        let entities = snapshots
            .into_iter()
            .filter(|s| s.is_persistent)
            .map(|s| s.entity)
            .collect();
        Self {
            instance_id,
            revision,
            entities,
            timestamp,
            dedup: Vec::new(),
        }
    }

    /// Creates a checkpoint directly from an [`crate::state::InstanceState`] snapshot.
    ///
    /// For M4 this treats every entity as persistent; future milestones can
    /// replace the closure with a real persistence flag per entity.
    #[must_use]
    pub fn from_state(state: &crate::state::InstanceState, timestamp: Timestamp) -> Self {
        Self::from_state_filtered(state, timestamp, |_| true)
    }

    /// Creates a checkpoint from state, filtering entities via predicate.
    ///
    /// The predicate receives `&Entity` and returns `true` when the entity
    /// should be persisted.
    #[must_use]
    pub fn from_state_filtered<F>(
        state: &crate::state::InstanceState,
        timestamp: Timestamp,
        is_persistent: F,
    ) -> Self
    where
        F: Fn(&Entity) -> bool,
    {
        let entities: Vec<Entity> = state
            .entities_snapshot()
            .into_iter()
            .filter(|e| is_persistent(e))
            .collect();
        Self {
            instance_id: state.instance_id(),
            revision: state.revision(),
            entities,
            timestamp,
            dedup: Vec::new(),
        }
    }

    /// Creates a checkpoint from an explicit entity list filtered by predicate.
    #[must_use]
    pub fn from_entities_filtered<F>(
        instance_id: InstanceId,
        revision: Revision,
        entities: Vec<Entity>,
        timestamp: Timestamp,
        is_persistent: F,
    ) -> Self
    where
        F: Fn(&Entity) -> bool,
    {
        let filtered = entities.into_iter().filter(|e| is_persistent(e)).collect();
        Self {
            instance_id,
            revision,
            entities: filtered,
            timestamp,
            dedup: Vec::new(),
        }
    }

    /// Returns number of entities in the checkpoint.
    #[must_use]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Serializes the checkpoint to versioned JSON bytes for persistence.
    ///
    /// The format is deliberately simple and stable for storage in
    /// `instance_checkpoints` table: `instance_id`, `revision`, `timestamp`
    /// (RFC3339 `UTC`), and an array of entity views.
    ///
    /// # Errors
    ///
    /// Returns a JSON serialization error if any timestamp cannot be rendered
    /// as RFC3339 (should not happen for valid `Timestamp` values).
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        validate_dedup_entries(&self.dedup).map_err(serde_json::Error::custom)?;
        let timestamp = self
            .timestamp
            .to_rfc3339()
            .map_err(serde_json::Error::custom)?;
        let checkpoint_millis = self
            .timestamp
            .to_unix_millis()
            .map_err(serde_json::Error::custom)?;
        if self
            .dedup
            .iter()
            .any(|entry| entry.created_at_millis > checkpoint_millis)
        {
            return Err(serde_json::Error::custom(
                "dedup creation time is in the future",
            ));
        }
        let entities = self
            .entities
            .iter()
            .map(stored_entity)
            .collect::<Result<Vec<_>, _>>()?;
        let bytes = serde_json::to_vec(&StoredCheckpoint {
            format_version: CHECKPOINT_FORMAT_VERSION,
            instance_id: self.instance_id.to_string(),
            revision: self.revision.as_u64(),
            timestamp,
            entities,
            dedup: self.dedup.iter().map(stored_dedup_entry).collect(),
        })?;
        if bytes.len() > MAX_CHECKPOINT_SERIALIZED_BYTES {
            return Err(serde_json::Error::custom(
                "checkpoint exceeds serialized size limit",
            ));
        }
        Ok(bytes)
    }

    /// Decodes and validates a checkpoint previously produced by
    /// [`Self::to_json_bytes`]. Version 1 payloads produced before structured
    /// visibility/components are accepted when their visibility can be restored
    /// without ambiguity.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, unsupported versions, invalid domain
    /// values, duplicate entities, or entity/checkpoint identity mismatches.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, CheckpointDecodeError> {
        if bytes.len() > MAX_CHECKPOINT_INPUT_BYTES {
            return Err(CheckpointDecodeError::new(
                "checkpoint input exceeds serialized size limit",
            ));
        }
        // Deliberately enforced before JSON decoding so a corrupt or
        // oversized row cannot turn into an unbounded allocation.
        Self::validate_payload_size(bytes.len())?;
        stream::validate_legacy_structure(bytes)?;
        let stored: StoredCheckpoint = serde_json::from_slice(bytes)
            .map_err(|_| CheckpointDecodeError::new("checkpoint payload is not valid JSON"))?;
        if !(1..=CHECKPOINT_FORMAT_VERSION).contains(&stored.format_version) {
            return Err(CheckpointDecodeError::new(format!(
                "unsupported checkpoint format version {}",
                stored.format_version
            )));
        }
        let instance_id = InstanceId::parse(&stored.instance_id)
            .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
        let revision = Revision::from_u64(stored.revision);
        let timestamp = Timestamp::parse_rfc3339(&stored.timestamp)
            .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
        if stored.entities.len() > 65_536 {
            return Err(CheckpointDecodeError::new("checkpoint entity count limit"));
        }
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        for entity in &stored.entities {
            count_record(entity, 1024 * 1024, &cancelled)
                .map_err(|e| CheckpointDecodeError::new(e.to_string()))?;
        }
        for receipt in &stored.dedup {
            count_record(receipt, 2 * 1024 * 1024, &cancelled)
                .map_err(|e| CheckpointDecodeError::new(e.to_string()))?;
        }
        let mut seen = HashSet::with_capacity(stored.entities.len());
        let mut entities = Vec::with_capacity(stored.entities.len());
        for stored_entity in stored.entities {
            let entity = restore_entity(stored_entity)?;
            if entity.instance_id() != instance_id {
                return Err(CheckpointDecodeError::new(
                    "checkpoint entity belongs to a different instance",
                ));
            }
            if entity.revision() > revision {
                return Err(CheckpointDecodeError::new(
                    "checkpoint entity revision exceeds instance revision",
                ));
            }
            if !seen.insert(entity.id()) {
                return Err(CheckpointDecodeError::new(
                    "checkpoint contains a duplicate entity id",
                ));
            }
            entities.push(entity);
        }
        let checkpoint_millis = timestamp
            .to_unix_millis()
            .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
        let dedup = validate_and_restore_dedup(stored.dedup, checkpoint_millis)?;
        Ok(Self {
            instance_id,
            revision,
            entities,
            timestamp,
            dedup,
        })
    }

    /// Returns the size of the JSON serialization in bytes.
    ///
    /// Convenience for persistence sizing / tests.
    ///
    /// # Errors
    ///
    /// Propagates JSON serialization errors from [`Self::to_json_bytes`].
    pub fn serialized_size(&self) -> Result<usize, serde_json::Error> {
        self.to_json_bytes().map(|b| b.len())
    }
}

fn stored_entity(entity: &Entity) -> Result<StoredEntity, serde_json::Error> {
    Ok(StoredEntity {
        id: entity.id().to_string(),
        instance_id: entity.instance_id().to_string(),
        kind: entity.kind().as_str().to_owned(),
        owner: entity.owner().map(|owner| owner.to_string()),
        transform: entity.transform().map(stored_transform),
        visibility: StoredVisibilityWire::Structured(stored_visibility(entity.visibility())),
        revision: entity.revision().as_u64(),
        created_at: entity
            .created_at()
            .to_rfc3339()
            .map_err(serde_json::Error::custom)?,
        updated_at: entity
            .updated_at()
            .to_rfc3339()
            .map_err(serde_json::Error::custom)?,
        components: entity
            .components()
            .iter()
            .map(|(key, payload)| (key.clone(), payload.clone()))
            .collect(),
    })
}

// Generation admission and output share the canonical record emitter. Legacy
// deserialization retains its separate compatibility counter below.
pub(crate) fn entity_charge(
    entity: &Entity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<usize, serde_json::Error> {
    if entity.revision().as_u64() == 0
        || entity.revision().as_u64() > i64::MAX as u64
        || entity.updated_at() < entity.created_at()
    {
        return Err(serde_json::Error::custom(
            "candidate entity cannot be restored",
        ));
    }
    let valid_visibility = match entity.visibility() {
        VisibilityPolicy::RoleRestricted { roles } => (1..=16).contains(&roles.len()),
        VisibilityPolicy::Explicit { users } => (1..=64).contains(&users.len()),
        VisibilityPolicy::Spatial { radius } => VisibilityPolicy::spatial(*radius).is_ok(),
        _ => true,
    };
    let visibility_len = match entity.visibility() {
        VisibilityPolicy::RoleRestricted { roles } => roles.len().saturating_mul(39),
        VisibilityPolicy::Explicit { users } => users.len().saturating_mul(39),
        VisibilityPolicy::Custom { tag } => tag.as_str().len().saturating_mul(6),
        _ => 64,
    };
    if !valid_visibility
        || visibility_len > 1024 * 1024
        || cancelled.load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(serde_json::Error::custom(
            "entity preparation bound exceeded",
        ));
    }
    // Reserve numeric revision and mutable timestamp growth. Other fields are
    // recounted on replacement, including transform auto-create and ownership.
    orbisync_application::checkpoint_record::canonical::emit::entity(
        entity,
        std::io::sink(),
        cancelled,
    )
    .map_err(serde_json::Error::custom)?
    .checked_add(128)
    .ok_or_else(|| serde_json::Error::custom("entity charge overflow"))
}

pub(crate) fn receipt_charge(
    entry: &CheckpointDedupEntry,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<usize, serde_json::Error> {
    orbisync_application::checkpoint_record::canonical::emit::receipt(
        borrowed_receipt(entry),
        std::io::sink(),
        cancelled,
    )
    .map(|n| n + 40)
    .map_err(serde_json::Error::custom) // two signed i64 times may grow to twenty characters
}

fn borrowed_receipt(
    entry: &CheckpointDedupEntry,
) -> orbisync_application::checkpoint_record::canonical::emit::ReceiptRef<'_> {
    use orbisync_application::checkpoint_record::canonical::emit::{ReceiptRef, ResultRef};
    ReceiptRef {
        command_id: &entry.command_id,
        fingerprint: &entry.fingerprint,
        created_at_millis: entry.created_at_millis,
        expires_at_millis: entry.expires_at_millis,
        message_id: &entry.message_id,
        result: match &entry.result {
            CheckpointDedupResult::Applied { response_payload } => {
                ResultRef::Applied(response_payload)
            }
            CheckpointDedupResult::Rejected { code, detail } => {
                ResultRef::Rejected { code, detail }
            }
        },
    }
}

fn count_record(
    value: &impl Serialize,
    limit: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<usize, serde_json::Error> {
    struct Counter<'a> {
        bytes: usize,
        limit: usize,
        cancelled: &'a std::sync::atomic::AtomicBool,
    }
    impl std::io::Write for Counter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            for chunk in bytes.chunks(16 * 1024) {
                if self.cancelled.load(std::sync::atomic::Ordering::Acquire)
                    || chunk.len() > self.limit.saturating_sub(self.bytes)
                {
                    return Err(std::io::Error::other(
                        "checkpoint record preparation bound exceeded",
                    ));
                }
                self.bytes += chunk.len();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        bytes: 0,
        limit,
        cancelled,
    };
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}

fn stored_transform(transform: Transform) -> StoredTransform {
    StoredTransform {
        position: StoredVec3 {
            x: transform.position().x(),
            y: transform.position().y(),
            z: transform.position().z(),
        },
        rotation: StoredQuaternion {
            x: transform.rotation().x(),
            y: transform.rotation().y(),
            z: transform.rotation().z(),
            w: transform.rotation().w(),
        },
        scale: StoredVec3 {
            x: transform.scale().x(),
            y: transform.scale().y(),
            z: transform.scale().z(),
        },
    }
}

fn stored_visibility(visibility: &VisibilityPolicy) -> StoredVisibility {
    match visibility {
        VisibilityPolicy::Global => StoredVisibility::Global,
        VisibilityPolicy::Spatial { radius } => StoredVisibility::Spatial { radius: *radius },
        VisibilityPolicy::OwnerOnly => StoredVisibility::OwnerOnly,
        VisibilityPolicy::RoleRestricted { roles } => StoredVisibility::RoleRestricted {
            roles: roles.iter().map(ToString::to_string).collect(),
        },
        VisibilityPolicy::Explicit { users } => StoredVisibility::Explicit {
            users: users.iter().map(ToString::to_string).collect(),
        },
        VisibilityPolicy::Custom { tag } => StoredVisibility::Custom {
            tag: tag.as_str().to_owned(),
        },
    }
}

fn stored_dedup_entry(entry: &CheckpointDedupEntry) -> StoredDedupEntry {
    let result = match &entry.result {
        CheckpointDedupResult::Applied { response_payload } => StoredDedupResult::Applied {
            response_payload: response_payload.clone(),
        },
        CheckpointDedupResult::Rejected { code, detail } => StoredDedupResult::Rejected {
            code: code.clone(),
            detail: detail.clone(),
        },
    };
    StoredDedupEntry {
        command_id: entry.command_id.clone(),
        fingerprint: entry.fingerprint.clone(),
        created_at_millis: entry.created_at_millis,
        expires_at_millis: entry.expires_at_millis,
        message_id: entry.message_id.clone(),
        result,
    }
}

fn restore_dedup_entry(
    stored: StoredDedupEntry,
    checkpoint_millis: i64,
) -> Result<CheckpointDedupEntry, CheckpointDecodeError> {
    if stored.fingerprint.len() != 32 {
        return Err(CheckpointDecodeError::new(
            "dedup fingerprint must contain exactly 32 bytes",
        ));
    }
    if stored.created_at_millis > checkpoint_millis {
        return Err(CheckpointDecodeError::new(
            "dedup creation time is in the future",
        ));
    }
    if stored.expires_at_millis
        != stored
            .created_at_millis
            .checked_add(MAX_DEDUP_TTL_MILLIS)
            .ok_or_else(|| {
                CheckpointDecodeError::new("dedup expiry is outside the 24 hour retention window")
            })?
    {
        return Err(CheckpointDecodeError::new(
            "dedup expiry is outside the 24 hour retention window",
        ));
    }
    if CommandId::parse(&stored.message_id).is_err() {
        return Err(CheckpointDecodeError::new(
            "dedup message id must be canonical UUIDv7",
        ));
    }
    let result = match stored.result {
        StoredDedupResult::Applied { response_payload } => {
            if response_payload.is_empty() || response_payload.len() > MAX_DEDUP_RESPONSE_BYTES {
                return Err(CheckpointDecodeError::new(
                    "applied dedup response payload must not be empty",
                ));
            }
            CheckpointDedupResult::Applied { response_payload }
        }
        StoredDedupResult::Rejected { code, detail } => {
            if code.is_empty()
                || code.len() > MAX_DEDUP_CODE_BYTES
                || detail.len() > MAX_DEDUP_DETAIL_BYTES
            {
                return Err(CheckpointDecodeError::new(
                    "rejected dedup result code must not be empty",
                ));
            }
            CheckpointDedupResult::Rejected { code, detail }
        }
    };
    Ok(CheckpointDedupEntry {
        command_id: stored.command_id,
        fingerprint: stored.fingerprint,
        created_at_millis: stored.created_at_millis,
        expires_at_millis: stored.expires_at_millis,
        message_id: stored.message_id,
        result,
    })
}

fn dedup_entry_size(entry: &CheckpointDedupEntry) -> usize {
    let result_size = match &entry.result {
        CheckpointDedupResult::Applied { response_payload } => response_payload.len(),
        CheckpointDedupResult::Rejected { code, detail } => code.len().saturating_add(detail.len()),
    };
    entry
        .command_id
        .len()
        .saturating_add(entry.fingerprint.len())
        .saturating_add(entry.message_id.len())
        .saturating_add(result_size)
}

/// Decode-path counterpart of [`validate_dedup_entries`] (which validates
/// already-restored entries before encoding). Parses and restores each
/// `StoredDedupEntry`, tracking command-id uniqueness and both the
/// per-record (`MAX_DEDUP_RECORD_BYTES`) and aggregate
/// (`MAX_DEDUP_AGGREGATE_BYTES`) size bounds as it goes, so a corrupt or
/// oversized dedup section is rejected without restoring every entry first.
///
/// Extracted from `Checkpoint::from_json_bytes` so tests can exercise this
/// bound directly instead of through the full decode pipeline, which also
/// enforces `MAX_CHECKPOINT_PAYLOAD_BYTES` up front and would otherwise mask
/// this check for any input large enough to trip both.
fn validate_and_restore_dedup(
    dedup: Vec<StoredDedupEntry>,
    checkpoint_millis: i64,
) -> Result<Vec<CheckpointDedupEntry>, CheckpointDecodeError> {
    if dedup.len() > MAX_DEDUP_RECORDS {
        return Err(CheckpointDecodeError::new(
            "checkpoint contains too many dedup records",
        ));
    }
    let mut seen_command_ids = HashSet::with_capacity(dedup.len());
    let mut dedup_aggregate = 0_usize;
    dedup
        .into_iter()
        .map(|entry| {
            let id = CommandId::parse(&entry.command_id)
                .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
            if !seen_command_ids.insert(id) {
                return Err(CheckpointDecodeError::new(
                    "checkpoint contains a duplicate dedup command id",
                ));
            }
            let restored = restore_dedup_entry(entry, checkpoint_millis)?;
            if dedup_entry_size(&restored) > MAX_DEDUP_RECORD_BYTES {
                return Err(CheckpointDecodeError::new(
                    "dedup record size exceeds limit",
                ));
            }
            dedup_aggregate = dedup_aggregate
                .checked_add(dedup_entry_size(&restored))
                .ok_or_else(|| CheckpointDecodeError::new("dedup aggregate size exceeds limit"))?;
            if dedup_aggregate > MAX_DEDUP_AGGREGATE_BYTES {
                return Err(CheckpointDecodeError::new(
                    "dedup aggregate size exceeds limit",
                ));
            }
            Ok(restored)
        })
        .collect()
}

pub(crate) fn validate_dedup_entries(entries: &[CheckpointDedupEntry]) -> Result<(), &'static str> {
    validate_dedup_entries_cancellable(entries, None)
}

fn validate_dedup_entries_cancellable(
    entries: &[CheckpointDedupEntry],
    cancelled: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), &'static str> {
    if entries.len() > MAX_DEDUP_RECORDS {
        return Err("checkpoint contains too many dedup records");
    }
    let mut aggregate = 0_usize;
    let mut seen = orbisync_application::checkpoint_record::canonical::index::IdentityIndex::new(
        entries.len(),
    );
    for entry in entries {
        if cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
            return Err("checkpoint cancelled");
        }
        if let Some(flag) = cancelled {
            orbisync_application::checkpoint_record::canonical::work::observe(
                flag,
                orbisync_application::checkpoint_record::canonical::work::Stage::Domain,
                8192,
            )
            .map_err(|_| "checkpoint cancelled")?;
        }
        if entry.fingerprint.len() != 32 {
            return Err("dedup fingerprint must contain exactly 32 bytes");
        }
        let id = CommandId::parse(&entry.command_id)
            .map_err(|_| "dedup command id must be canonical UUIDv7")?;
        seen.push(id.as_uuid())
            .map_err(|_| "dedup identity capacity")?;
        if entry.expires_at_millis
            != entry
                .created_at_millis
                .checked_add(MAX_DEDUP_TTL_MILLIS)
                .ok_or("dedup expiry is outside the 24 hour retention window")?
        {
            return Err("dedup expiry is outside the 24 hour retention window");
        }
        aggregate = aggregate.saturating_add(entry.command_id.len());
        aggregate = aggregate.saturating_add(entry.fingerprint.len());
        if CommandId::parse(&entry.message_id).is_err() {
            return Err("dedup message id must be canonical UUIDv7");
        }
        aggregate = aggregate.saturating_add(entry.message_id.len());
        let mut record_size = entry
            .command_id
            .len()
            .saturating_add(entry.fingerprint.len())
            .saturating_add(entry.message_id.len());
        match &entry.result {
            CheckpointDedupResult::Applied { response_payload } => {
                if response_payload.is_empty() || response_payload.len() > MAX_DEDUP_RESPONSE_BYTES
                {
                    return Err("applied dedup response payload exceeds size limit");
                }
                aggregate = aggregate.saturating_add(response_payload.len());
                record_size = record_size.saturating_add(response_payload.len());
            }
            CheckpointDedupResult::Rejected { code, detail } => {
                if code.is_empty()
                    || code.len() > MAX_DEDUP_CODE_BYTES
                    || detail.len() > MAX_DEDUP_DETAIL_BYTES
                {
                    return Err("rejected dedup fields exceed size limit");
                }
                aggregate = aggregate
                    .saturating_add(code.len())
                    .saturating_add(detail.len());
                record_size = record_size
                    .saturating_add(code.len())
                    .saturating_add(detail.len());
            }
        }
        if record_size > MAX_DEDUP_RECORD_BYTES {
            return Err("dedup record size exceeds limit");
        }
        if aggregate > MAX_DEDUP_AGGREGATE_BYTES {
            return Err("dedup aggregate size exceeds limit");
        }
    }
    loop {
        if cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
            return Err("checkpoint cancelled");
        }
        if let Some(flag) = cancelled {
            orbisync_application::checkpoint_record::canonical::work::observe(
                flag,
                orbisync_application::checkpoint_record::canonical::work::Stage::Index,
                4096,
            )
            .map_err(|_| "checkpoint cancelled")?;
        }
        if seen
            .drain_step()
            .map_err(|_| "checkpoint contains a duplicate dedup command id")?
        {
            break;
        }
    }
    Ok(())
}

fn restore_entity(stored: StoredEntity) -> Result<Entity, CheckpointDecodeError> {
    let id = EntityId::parse(&stored.id)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let instance_id = InstanceId::parse(&stored.instance_id)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let kind = EntityKind::parse(&stored.kind)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let owner = stored
        .owner
        .as_deref()
        .map(UserId::parse)
        .transpose()
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let transform = stored.transform.map(restore_transform).transpose()?;
    let visibility = restore_visibility(stored.visibility)?;
    let created_at = Timestamp::parse_rfc3339(&stored.created_at)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let updated_at = Timestamp::parse_rfc3339(&stored.updated_at)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    Entity::from_persisted(
        id,
        instance_id,
        kind,
        owner,
        transform,
        visibility,
        Revision::from_u64(stored.revision),
        created_at,
        updated_at,
        stored.components.into_iter().collect::<HashMap<_, _>>(),
    )
    .map_err(|error| CheckpointDecodeError::new(error.to_string()))
}

fn restore_transform(stored: StoredTransform) -> Result<Transform, CheckpointDecodeError> {
    let position = Vec3::new(stored.position.x, stored.position.y, stored.position.z)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let rotation = Quaternion::new(
        stored.rotation.x,
        stored.rotation.y,
        stored.rotation.z,
        stored.rotation.w,
    )
    .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    let scale = Vec3::new(stored.scale.x, stored.scale.y, stored.scale.z)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
    Transform::new(position, rotation, scale)
        .map_err(|error| CheckpointDecodeError::new(error.to_string()))
}

fn restore_visibility(
    stored: StoredVisibilityWire,
) -> Result<VisibilityPolicy, CheckpointDecodeError> {
    let structured = match stored {
        StoredVisibilityWire::Structured(value) => value,
        StoredVisibilityWire::Legacy(value) => restore_legacy_visibility(&value)?,
    };
    match structured {
        StoredVisibility::Global => Ok(VisibilityPolicy::Global),
        StoredVisibility::Spatial { radius } => VisibilityPolicy::spatial(radius)
            .map_err(|error| CheckpointDecodeError::new(error.to_string())),
        StoredVisibility::OwnerOnly => Ok(VisibilityPolicy::OwnerOnly),
        StoredVisibility::RoleRestricted { roles } => {
            let roles = roles
                .iter()
                .map(|role| RoleId::parse(role))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
            VisibilityPolicy::role_restricted(roles)
                .map_err(|error| CheckpointDecodeError::new(error.to_string()))
        }
        StoredVisibility::Explicit { users } => {
            let users = users
                .iter()
                .map(|user| UserId::parse(user))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| CheckpointDecodeError::new(error.to_string()))?;
            VisibilityPolicy::explicit(users)
                .map_err(|error| CheckpointDecodeError::new(error.to_string()))
        }
        StoredVisibility::Custom { tag } => VisibilityPolicy::custom(tag)
            .map_err(|error| CheckpointDecodeError::new(error.to_string())),
    }
}

fn restore_legacy_visibility(value: &str) -> Result<StoredVisibility, CheckpointDecodeError> {
    if value == "Global" {
        return Ok(StoredVisibility::Global);
    }
    if value == "OwnerOnly" {
        return Ok(StoredVisibility::OwnerOnly);
    }
    if let Some(radius) = value
        .strip_prefix("Spatial { radius: ")
        .and_then(|rest| rest.strip_suffix(" }"))
        .and_then(|value| value.parse::<f32>().ok())
    {
        return Ok(StoredVisibility::Spatial { radius });
    }
    if let Some(raw_roles) = value
        .strip_prefix("RoleRestricted { roles: {")
        .and_then(|rest| rest.strip_suffix("} }"))
    {
        return Ok(StoredVisibility::RoleRestricted {
            roles: parse_legacy_id_set(raw_roles, "RoleId(")?,
        });
    }
    if let Some(raw_users) = value
        .strip_prefix("Explicit { users: {")
        .and_then(|rest| rest.strip_suffix("} }"))
    {
        return Ok(StoredVisibility::Explicit {
            users: parse_legacy_id_set(raw_users, "UserId(")?,
        });
    }
    if let Some(tag) = value
        .strip_prefix("Custom { tag: CustomVisibilityTag(\"")
        .and_then(|rest| rest.strip_suffix("\") }"))
    {
        return Ok(StoredVisibility::Custom {
            tag: tag.to_owned(),
        });
    }
    Err(CheckpointDecodeError::new(
        "legacy checkpoint visibility cannot be restored safely",
    ))
}

fn parse_legacy_id_set(value: &str, wrapper: &str) -> Result<Vec<String>, CheckpointDecodeError> {
    if value.is_empty() {
        return Err(CheckpointDecodeError::new(
            "legacy checkpoint visibility contains an empty id set",
        ));
    }
    value
        .split(", ")
        .map(|entry| {
            entry
                .strip_prefix(wrapper)
                .and_then(|rest| rest.strip_suffix(')'))
                .map(str::to_owned)
                .ok_or_else(|| {
                    CheckpointDecodeError::new(
                        "legacy checkpoint visibility cannot be restored safely",
                    )
                })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{
        Checkpoint, CheckpointDedupEntry, CheckpointDedupResult, CheckpointEntity,
        MAX_CHECKPOINT_INPUT_BYTES, MAX_DEDUP_RECORDS, MAX_DEDUP_RESPONSE_BYTES,
        MAX_DEDUP_TTL_MILLIS, StoredDedupEntry, StoredDedupResult, validate_and_restore_dedup,
    };
    use orbisync_domain::{
        CommandId, Entity, EntityId, EntityKind, InstanceId, Revision, RoleId, Timestamp,
        Transform, UserId, Vec3, VisibilityPolicy,
    };
    use std::collections::HashMap;

    fn ts(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis).expect("valid ts")
    }

    fn entity_at(kind: EntityKind) -> Entity {
        Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            kind,
            None,
            None,
            VisibilityPolicy::Global,
            ts(1_000),
        )
    }

    #[test]
    fn checkpoint_new_stores_fields() {
        let iid = InstanceId::generate();
        let rev = Revision::from_u64(42);
        let e = entity_at(EntityKind::Object);
        let cp = Checkpoint::new(iid, rev, vec![e.clone()], ts(2_000));
        assert_eq!(cp.instance_id, iid);
        assert_eq!(cp.revision, rev);
        assert_eq!(cp.entity_count(), 1);
        assert_eq!(cp.entities[0].id(), e.id());
    }

    #[test]
    fn checkpoint_filters_persistent_entities() {
        let iid = InstanceId::generate();
        let rev = Revision::INITIAL;
        let e1 = entity_at(EntityKind::Avatar);
        let e2 = entity_at(EntityKind::Object);
        let snapshots = vec![
            CheckpointEntity {
                entity: e1.clone(),
                is_persistent: true,
            },
            CheckpointEntity {
                entity: e2.clone(),
                is_persistent: false,
            },
        ];
        let cp = Checkpoint::from_persistent_snapshots(iid, rev, snapshots, ts(3_000));
        assert_eq!(cp.entity_count(), 1);
        assert_eq!(cp.entities[0].id(), e1.id());
    }

    #[test]
    fn checkpoint_m4_treats_all_as_persistent_when_flag_true() {
        let iid = InstanceId::generate();
        let rev = Revision::from_u64(1);
        let entities = vec![entity_at(EntityKind::Avatar), entity_at(EntityKind::Object)];
        let cp =
            Checkpoint::from_entities_filtered(iid, rev, entities.clone(), ts(4_000), |_| true);
        assert_eq!(cp.entity_count(), 2);
        let cp_none = Checkpoint::from_entities_filtered(iid, rev, entities, ts(4_000), |_| false);
        assert_eq!(cp_none.entity_count(), 0);
    }

    #[test]
    fn checkpoint_serializes_to_json_and_has_size() {
        let iid = InstanceId::generate();
        let rev = Revision::from_u64(5);
        let e = entity_at(EntityKind::Trigger);
        let cp = Checkpoint::new(iid, rev, vec![e], ts(5_000));
        let bytes = cp.to_json_bytes().expect("serialize");
        assert!(!bytes.is_empty());
        assert!(bytes.len() < 64 * 1024);
        let size = cp.serialized_size().expect("size");
        assert_eq!(size, bytes.len());
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed["instance_id"], iid.to_string());
        assert_eq!(parsed["revision"], 5);
        assert_eq!(parsed["entities"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn checkpoint_empty_entities_serializes() {
        let iid = InstanceId::generate();
        let cp = Checkpoint::new(iid, Revision::INITIAL, Vec::new(), ts(6_000));
        let bytes = cp.to_json_bytes().expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed["entities"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn checkpoint_rejects_payload_over_size_limit_before_json_decode() {
        let error = Checkpoint::validate_payload_size(
            orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES + 1,
        )
        .expect_err("payload must be bounded");
        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn checkpoint_json_timestamp_is_rfc3339() {
        let iid = InstanceId::generate();
        let cp = Checkpoint::new(iid, Revision::INITIAL, Vec::new(), ts(0));
        let bytes = cp.to_json_bytes().expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        let ts_str = parsed["timestamp"].as_str().expect("timestamp string");
        assert!(ts_str.contains('T'));
        // round-trips through domain Timestamp parser
        assert!(Timestamp::parse_rfc3339(ts_str).is_ok());
    }

    #[test]
    fn checkpoint_does_not_serialize_ephemeral_velocity() {
        let iid = InstanceId::generate();
        let mut entity = entity_at(EntityKind::Object);
        entity
            .update_velocity(
                Revision::from_u64(1),
                Vec3::new(1.0_f32, 2.0_f32, 3.0_f32).expect("velocity"),
                ts(7_000),
            )
            .expect("update velocity");
        let cp = Checkpoint::new(iid, entity.revision(), vec![entity], ts(7_000));
        let bytes = cp.to_json_bytes().expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["entities"][0].get("velocity").is_none());
    }

    #[test]
    fn checkpoint_does_not_serialize_ephemeral_animation_or_presence() {
        let mut entity = entity_at(EntityKind::Object);
        entity
            .update_velocity(
                entity.revision(),
                Vec3::new(1.0_f32, 2.0_f32, 3.0_f32).expect("velocity"),
                ts(1_000),
            )
            .expect("velocity update");
        let animation = orbisync_domain::Animation::new("walk", 0.5, 1.0, true).expect("valid");
        entity
            .update_animation(entity.revision(), animation, ts(1_000))
            .expect("animation update");
        let presence =
            orbisync_domain::Presence::new(orbisync_domain::PresenceState::Online, 1_000);
        entity
            .update_presence(entity.revision(), presence, ts(1_000))
            .expect("presence update");
        let cp = Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(1),
            vec![entity],
            ts(1_000),
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&cp.to_json_bytes().expect("serialize")).expect("json");
        let saved = &parsed["entities"][0];
        assert!(saved.get("velocity").is_none());
        assert!(saved.get("animation").is_none());
        assert!(saved.get("presence").is_none());
    }

    #[test]
    fn checkpoint_round_trip_restores_all_durable_entity_fields() {
        let instance_id = InstanceId::generate();
        let owner = UserId::generate();
        let explicit_user = UserId::generate();
        let role = RoleId::generate();
        let created_at = ts(10_000);
        let updated_at = ts(11_000);
        let transform = Transform::new(
            Vec3::new(1.0, 2.0, 3.0).expect("position"),
            orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("rotation"),
            Vec3::new(1.0, 2.0, 1.0).expect("scale"),
        )
        .expect("transform");
        let visibilities = vec![
            VisibilityPolicy::Global,
            VisibilityPolicy::spatial(25.0).expect("spatial"),
            VisibilityPolicy::OwnerOnly,
            VisibilityPolicy::role_restricted([role]).expect("roles"),
            VisibilityPolicy::explicit([explicit_user]).expect("users"),
            VisibilityPolicy::custom("example.policy").expect("custom"),
        ];
        let entities = visibilities
            .into_iter()
            .enumerate()
            .map(|(index, visibility)| {
                let mut components = HashMap::new();
                components.insert("example.state".to_owned(), vec![index as u8, 0, 255]);
                Entity::from_persisted(
                    EntityId::generate(),
                    instance_id,
                    EntityKind::Object,
                    Some(owner),
                    Some(transform),
                    visibility,
                    Revision::from_u64(3),
                    created_at,
                    updated_at,
                    components,
                )
                .expect("persisted entity")
            })
            .collect::<Vec<_>>();
        let checkpoint = Checkpoint::new(instance_id, Revision::from_u64(9), entities, ts(12_000));

        let bytes = checkpoint.to_json_bytes().expect("serialize");
        let restored = Checkpoint::from_json_bytes(&bytes).expect("restore");

        assert_eq!(restored, checkpoint);
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["format_version"], 4);
        assert_eq!(json["entities"][0]["components"]["example.state"][2], 255);
    }

    #[test]
    fn checkpoint_decodes_unambiguous_legacy_visibility() {
        let instance_id = InstanceId::generate();
        let entity_id = EntityId::generate();
        let payload = serde_json::json!({
            "instance_id": instance_id.to_string(),
            "revision": 4,
            "timestamp": "1970-01-01T00:00:04Z",
            "entities": [{
                "id": entity_id.to_string(),
                "instance_id": instance_id.to_string(),
                "kind": "object",
                "owner": null,
                "transform": null,
                "visibility": "Spatial { radius: 30.0 }",
                "revision": 1,
                "created_at": "1970-01-01T00:00:01Z",
                "updated_at": "1970-01-01T00:00:02Z"
            }]
        });

        let restored =
            Checkpoint::from_json_bytes(&serde_json::to_vec(&payload).expect("legacy serialize"))
                .expect("legacy restore");

        assert_eq!(restored.instance_id, instance_id);
        assert_eq!(restored.entities[0].id(), entity_id);
        assert_eq!(
            restored.entities[0].visibility(),
            &VisibilityPolicy::spatial(30.0).expect("spatial")
        );
        assert!(restored.entities[0].components().is_empty());
    }

    #[test]
    fn checkpoint_decodes_all_legacy_visibility_debug_forms() {
        let instance_id = InstanceId::generate();
        let policies = vec![
            VisibilityPolicy::Global,
            VisibilityPolicy::spatial(30.0).expect("spatial"),
            VisibilityPolicy::OwnerOnly,
            VisibilityPolicy::role_restricted([RoleId::generate()]).expect("roles"),
            VisibilityPolicy::explicit([UserId::generate()]).expect("users"),
            VisibilityPolicy::custom("example.policy").expect("custom"),
        ];

        for policy in policies {
            let payload = serde_json::json!({
                "instance_id": instance_id.to_string(),
                "revision": 4,
                "timestamp": "1970-01-01T00:00:04Z",
                "entities": [{
                    "id": EntityId::generate().to_string(),
                    "instance_id": instance_id.to_string(),
                    "kind": "object",
                    "owner": null,
                    "transform": null,
                    "visibility": format!("{policy:?}"),
                    "revision": 1,
                    "created_at": "1970-01-01T00:00:01Z",
                    "updated_at": "1970-01-01T00:00:02Z"
                }]
            });
            let restored = Checkpoint::from_json_bytes(
                &serde_json::to_vec(&payload).expect("legacy serialize"),
            )
            .expect("legacy visibility must restore");
            assert_eq!(restored.entities[0].visibility(), &policy);
        }
    }

    #[test]
    fn checkpoint_rejects_ambiguous_legacy_visibility() {
        let instance_id = InstanceId::generate();
        let payload = serde_json::json!({
            "instance_id": instance_id.to_string(),
            "revision": 4,
            "timestamp": "1970-01-01T00:00:04Z",
            "entities": [{
                "id": EntityId::generate().to_string(),
                "instance_id": instance_id.to_string(),
                "kind": "object",
                "owner": null,
                "transform": null,
                "visibility": "RoleRestricted { roles: {...} }",
                "revision": 1,
                "created_at": "1970-01-01T00:00:01Z",
                "updated_at": "1970-01-01T00:00:02Z"
            }]
        });

        let error =
            Checkpoint::from_json_bytes(&serde_json::to_vec(&payload).expect("legacy serialize"))
                .expect_err("ambiguous legacy visibility must fail closed");

        assert!(error.to_string().contains("cannot be restored safely"));
    }

    #[test]
    fn checkpoint_rejects_entity_from_another_instance() {
        let checkpoint_instance = InstanceId::generate();
        let payload = serde_json::json!({
            "format_version": 2,
            "instance_id": checkpoint_instance.to_string(),
            "revision": 4,
            "timestamp": "1970-01-01T00:00:04Z",
            "entities": [{
                "id": EntityId::generate().to_string(),
                "instance_id": InstanceId::generate().to_string(),
                "kind": "object",
                "owner": null,
                "transform": null,
                "visibility": {"type": "global"},
                "revision": 1,
                "created_at": "1970-01-01T00:00:01Z",
                "updated_at": "1970-01-01T00:00:02Z",
                "components": {}
            }]
        });

        let error = Checkpoint::from_json_bytes(&serde_json::to_vec(&payload).expect("serialize"))
            .expect_err("cross-instance entity must be rejected");

        assert!(error.to_string().contains("different instance"));
    }

    #[test]
    fn checkpoint_dedup_validation_rejects_future_duplicate_and_oversized_records() {
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let entry = || CheckpointDedupEntry {
            command_id: id.to_string(),
            fingerprint: vec![0; 32],
            created_at_millis: 1_000,
            expires_at_millis: 1_000 + MAX_DEDUP_TTL_MILLIS,
            message_id: id.to_string(),
            result: CheckpointDedupResult::Rejected {
                code: String::from("INVALID_ARGUMENT"),
                detail: String::from("invalid"),
            },
        };
        let mut duplicate = Checkpoint::new(
            instance,
            Revision::INITIAL,
            Vec::new(),
            Timestamp::from_unix_millis(1_000).expect("timestamp"),
        );
        duplicate.dedup = vec![entry(), entry()];
        assert!(duplicate.to_json_bytes().is_err());

        let mut future = duplicate.clone();
        future.dedup = vec![CheckpointDedupEntry {
            created_at_millis: 1_001,
            ..entry()
        }];
        assert!(future.to_json_bytes().is_err());

        let mut oversized = duplicate;
        oversized.dedup = vec![CheckpointDedupEntry {
            result: CheckpointDedupResult::Applied {
                response_payload: vec![0; MAX_DEDUP_RESPONSE_BYTES + 1],
            },
            ..entry()
        }];
        assert!(oversized.to_json_bytes().is_err());
        assert!(Checkpoint::from_json_bytes(&vec![b' '; MAX_CHECKPOINT_INPUT_BYTES + 1]).is_err());
    }

    #[test]
    fn checkpoint_decode_enforces_dedup_aggregate_bound() {
        // Exercises `validate_and_restore_dedup` directly instead of through
        // `Checkpoint::from_json_bytes`: `MAX_CHECKPOINT_PAYLOAD_BYTES` and
        // `MAX_DEDUP_AGGREGATE_BYTES` are both 8 MiB, so any input large
        // enough to trip this bound also trips the payload-size gate that
        // `from_json_bytes` enforces before decoding even starts, and the
        // wrong error message would win the assertion below instead of this
        // bound ever actually being exercised.
        let detail = "x".repeat(2_048);
        let dedup: Vec<StoredDedupEntry> = (0..MAX_DEDUP_RECORDS)
            .map(|_| StoredDedupEntry {
                command_id: CommandId::generate().to_string(),
                fingerprint: vec![0; 32],
                created_at_millis: 0,
                expires_at_millis: MAX_DEDUP_TTL_MILLIS,
                message_id: CommandId::generate().to_string(),
                result: StoredDedupResult::Rejected {
                    code: "INVALID_ARGUMENT".to_owned(),
                    detail: detail.clone(),
                },
            })
            .collect();
        let error = validate_and_restore_dedup(dedup, 0)
            .expect_err("aggregate bound must reject the dedup section");
        assert!(error.to_string().contains("aggregate"), "{error}");
    }
}
