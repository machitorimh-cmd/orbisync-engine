//! Instance runtime.
//!
//! Owns the canonical transient state of a world instance: the Instance Actor,
//! command processing, snapshot and delta generation, ownership and lifecycle
//! (specification §30.5, `state-and-runtime.md` §3).
//!
//! # Dependency rule
//!
//! `world-runtime` depends on `domain` and `application` only. It must not
//! depend on `interest` or on `realtime` (§8 acceptance condition 4): the
//! runtime publishes state, and delivery decides who receives it.
//!
//! Milestone 2 implements the actor loop; Milestone 0 fixes the lifecycle
//! vocabulary the loop will use.

use core::fmt;

use orbisync_domain::{InstanceId, Revision};

pub mod actor;
pub mod checkpoint;
pub mod command;
pub mod extension;
pub mod interest_snapshot;
pub mod persistence;
pub mod registry;
pub mod state;
pub mod validation;

pub use actor::{MailboxConfig, MailboxEnqueue, MailboxLane, MailboxSendError};
pub use checkpoint::{
    Checkpoint, CheckpointDecodeError, CheckpointDedupEntry, CheckpointDedupResult,
    CheckpointEntity, MAX_CHECKPOINT_INPUT_BYTES, MAX_CHECKPOINT_SERIALIZED_BYTES,
    MAX_DEDUP_AGGREGATE_BYTES, MAX_DEDUP_CODE_BYTES, MAX_DEDUP_DETAIL_BYTES,
    MAX_DEDUP_RECORD_BYTES, MAX_DEDUP_RECORDS, MAX_DEDUP_RESPONSE_BYTES, MAX_DEDUP_TTL_MILLIS,
};
pub use extension::{ExtensionEvent, Outbox};
pub use interest_snapshot::InterestSnapshot;
pub use persistence::{EntityPersistenceEvent, PersistenceOutbox};
pub use registry::{
    InstanceHandle, InstanceReadSnapshot, InstanceReapResult, InstanceTickResult, RuntimeRegistry,
};

/// Lifecycle state of an instance runtime (`state-and-runtime.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeState {
    /// The actor is created but not yet accepting commands.
    Starting,
    /// The actor accepts commands and ticks.
    Running,
    /// The actor rejects new members and flushes pending persistence.
    Draining,
    /// The actor has terminated.
    Stopped,
}

impl RuntimeState {
    /// Returns the stable snake_case name used in logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
        }
    }

    /// Returns `true` when the runtime accepts new commands.
    #[must_use]
    pub const fn accepts_commands(self) -> bool {
        matches!(self, Self::Running)
    }
}

impl fmt::Display for RuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identity and lifecycle summary of one running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceRuntimeDescriptor {
    /// Instance owning the transient state.
    pub instance_id: InstanceId,
    /// Current lifecycle state.
    pub state: RuntimeState,
    /// Revision of the last applied command.
    pub revision: Revision,
}

impl InstanceRuntimeDescriptor {
    /// Creates a descriptor for a freshly created runtime.
    #[must_use]
    pub const fn starting(instance_id: InstanceId) -> Self {
        Self {
            instance_id,
            state: RuntimeState::Starting,
            revision: Revision::INITIAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InstanceRuntimeDescriptor, RuntimeState};
    use orbisync_domain::{InstanceId, Revision};

    #[test]
    fn test_only_running_accepts_commands() {
        assert!(RuntimeState::Running.accepts_commands());
        assert!(!RuntimeState::Starting.accepts_commands());
        assert!(!RuntimeState::Draining.accepts_commands());
        assert!(!RuntimeState::Stopped.accepts_commands());
    }

    #[test]
    fn test_new_runtime_starts_at_initial_revision() {
        let descriptor = InstanceRuntimeDescriptor::starting(InstanceId::generate());
        assert_eq!(descriptor.state, RuntimeState::Starting);
        assert_eq!(descriptor.revision, Revision::INITIAL);
    }
}
