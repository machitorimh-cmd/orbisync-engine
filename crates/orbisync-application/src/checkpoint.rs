//! Checkpoint persistence port (`state-and-runtime.md` §1.3).
//!
//! M4 keeps checkpoint storage behind a port. The `instance_runtime` produces a
//! checkpoint snapshot and hands it to `CheckpointStore`; the
//! `storage-postgres` adapter will implement durable writes in a later
//! milestone. For M4 a stub trait is sufficient — no DB implementation yet.

use orbisync_domain::{InstanceId, Revision, Timestamp};

use crate::ApplicationError;

/// Maximum serialized checkpoint payload accepted by checkpoint adapters.
///
/// Checkpoints are restored before an instance actor is published, so this
/// bound protects both the database adapter and the runtime decoder from
/// unbounded JSON input. The value is deliberately a compile-time policy
/// shared by all checkpoint implementations.
///
/// Derivation (HIGH-001 fix, round 3). The domain does NOT bound the number
/// of entities per instance: `1..=1000` in `orbisync-domain` is instance
/// *member* capacity, and DM-04 only caps per-entity components
/// (16 x 4,096 bytes). A world can therefore grow to any checkpoint size, so
/// the limit must be derived from a process memory budget instead of a domain
/// worst case. Measured process peaks (release build, Windows
/// `GetProcessMemoryInfo` peak commit) on the full PostgreSQL round trip with
/// a worst-case component shape (502 entities x 16 components x 1,024-byte
/// payloads -> 16.0 MiB compact payload, every byte serialized as a JSON
/// number-array element): save peak 305 MiB (~19x) and load/restore peak
/// 321 MiB (~20x). The earlier round measured 64.2 MiB payload -> 660 MiB
/// save / 914 MiB load (~10x / ~14x), so ~20x is the conservative envelope.
/// Budget: 1 GiB process peak with 256 MiB reserved for the base runtime
/// leaves 768 MiB for transient checkpoint work. Concurrency is bounded to at
/// most 2 simultaneous activations
/// (`RealtimeState::MAX_CONCURRENT_INSTANCE_ACTIVATIONS`) and 1 simultaneous
/// save (saves run sequentially inside the single tick task; 2 are budgeted
/// for margin), so the payload limit must satisfy
/// `limit * (2 * 20 + 1 * 19) <= 768 MiB` -> `limit <= 13.0 MiB`.
/// 8 MiB is the power-of-two below that bound; at the limit the expected
/// process peak is `2 * 20 * 8 + 19 * 8 = 472 MiB`, inside the 768 MiB
/// transient budget with margin.
pub const MAX_CHECKPOINT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Persistent snapshot of an instance at a revision.
///
/// Only persistent entities are included; high-frequency ephemeral state such
/// as position/velocity is not persisted every tick (spec §10.3,
/// `state-and-runtime.md` §1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Instance that was checkpointed.
    pub instance_id: InstanceId,
    /// Revision at checkpoint time.
    pub revision: Revision,
    /// Serialized payload (JSON bytes produced by `world-runtime::Checkpoint::to_json_bytes`).
    pub payload: Vec<u8>,
    /// Wall-clock time the checkpoint was created.
    pub created_at: Timestamp,
}

/// The deduplication timestamps accepted by a durable checkpoint write.
///
/// A store may merge a same-revision checkpoint with an already durable
/// outcome.  Returning the adopted timestamps lets a caller keep its local
/// replay window aligned with the durable record rather than with a retry's
/// wall clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointSaveReceipt {
    /// Canonical command identifier for the retained outcome.
    pub command_id: String,
    /// Creation time adopted by the durable store, in Unix milliseconds.
    pub created_at_millis: i64,
    /// Expiry time adopted by the durable store, in Unix milliseconds.
    pub expires_at_millis: i64,
}

impl Checkpoint {
    /// Creates a checkpoint from raw parts.
    #[must_use]
    pub fn new(
        instance_id: InstanceId,
        revision: Revision,
        payload: Vec<u8>,
        created_at: Timestamp,
    ) -> Self {
        Self {
            instance_id,
            revision,
            payload,
            created_at,
        }
    }
}

/// Port for durable checkpoint storage.
///
/// The `storage-postgres` crate will implement this against the
/// `instance_checkpoints` table (`rest-api-persistence.md` §8). For M4 the
/// trait exists so the application layer can depend on an abstraction without
/// naming any SQL type (`repo-crate-conventions.md` §3.2).
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync + 'static {
    /// Persists a checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable. The payload is already
    /// serialized; the adapter must not log it verbatim (PII redaction,
    /// `observability-and-config.md` §5).
    async fn save_checkpoint(
        &self,
        checkpoint: Checkpoint,
    ) -> Result<Vec<CheckpointSaveReceipt>, ApplicationError>;

    /// Loads the latest checkpoint for an instance.
    ///
    /// The latest checkpoint is the row with the greatest `revision` for the
    /// given instance (and newest `created_at` as tie-breaker).
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationErrorKind::PortFailure`](crate::ApplicationErrorKind::PortFailure)
    /// when the backing store is unavailable. Returns `Ok(None)` when no
    /// checkpoint exists for `instance_id`.
    async fn load_latest(
        &self,
        instance_id: InstanceId,
    ) -> Result<Option<Checkpoint>, ApplicationError>;
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{Checkpoint, CheckpointStore};
    use crate::{ApplicationError, ApplicationErrorKind};
    use orbisync_domain::{InstanceId, Revision, Timestamp};
    use std::sync::{Arc, Mutex};

    struct InMemoryCheckpointStore {
        saved: Arc<Mutex<Vec<Checkpoint>>>,
    }

    #[async_trait::async_trait]
    impl CheckpointStore for InMemoryCheckpointStore {
        async fn save_checkpoint(
            &self,
            checkpoint: Checkpoint,
        ) -> Result<Vec<super::CheckpointSaveReceipt>, ApplicationError> {
            self.saved
                .lock()
                .map_err(|_| {
                    ApplicationError::new(ApplicationErrorKind::PortFailure, "lock poisoned")
                })?
                .push(checkpoint);
            Ok(Vec::new())
        }

        async fn load_latest(
            &self,
            instance_id: InstanceId,
        ) -> Result<Option<Checkpoint>, ApplicationError> {
            let saved = self.saved.lock().map_err(|_| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, "lock poisoned")
            })?;
            let latest = saved
                .iter()
                .filter(|cp| cp.instance_id == instance_id)
                .max_by_key(|cp| (cp.revision.as_u64(), cp.created_at.as_offset_date_time()))
                .cloned();
            Ok(latest)
        }
    }

    fn ts() -> Timestamp {
        Timestamp::from_unix_millis(0).expect("valid")
    }

    #[test]
    fn checkpoint_round_trip_via_store() {
        let store = InMemoryCheckpointStore {
            saved: Arc::new(Mutex::new(Vec::new())),
        };
        let cp = Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(42),
            vec![1, 2, 3],
            ts(),
        );
        pollster::block_on(store.save_checkpoint(cp.clone())).expect("save");
        let saved = store.saved.lock().expect("lock").clone();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].instance_id, cp.instance_id);
        assert_eq!(saved[0].revision, cp.revision);
        assert_eq!(saved[0].payload, cp.payload);
    }
}
