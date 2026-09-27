//! World instance aggregate.

use crate::{DomainError, DomainErrorKind, InstanceId, Revision, Timestamp, WorldId};

fn invalid(detail: impl Into<String>) -> DomainError {
    DomainError::new(DomainErrorKind::InvalidValue, detail)
}

/// Lifecycle of a world instance (`domain-model.md` §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstanceLifecycle {
    /// Created but not yet running.
    Created,
    /// Accepting members and ticking.
    Running,
    /// Draining members before stop.
    Stopping,
    /// Terminated.
    Stopped,
}

impl InstanceLifecycle {
    /// Returns canonical string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }

    /// Parses canonical string.
    ///
    /// # Errors
    ///
    /// Returns invalid-value for unknown.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "created" => Ok(Self::Created),
            "running" => Ok(Self::Running),
            "stopping" => Ok(Self::Stopping),
            "stopped" => Ok(Self::Stopped),
            _ => Err(invalid(
                "instance lifecycle must be created/running/stopping/stopped",
            )),
        }
    }

    /// Returns true when new members may join.
    #[must_use]
    pub const fn accepts_joins(self) -> bool {
        matches!(self, Self::Created | Self::Running)
    }
}

/// World instance aggregate root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldInstance {
    id: InstanceId,
    world_id: WorldId,
    lifecycle: InstanceLifecycle,
    capacity: u32,
    created_at: Timestamp,
    started_at: Option<Timestamp>,
    revision: Revision,
}

impl WorldInstance {
    /// Creates an instance in `Created` state.
    ///
    /// # Errors
    ///
    /// Returns invalid-value when capacity is out of 1..1000.
    pub fn new(
        id: InstanceId,
        world_id: WorldId,
        capacity: u32,
        now: Timestamp,
    ) -> Result<Self, DomainError> {
        if !(1..=1000).contains(&capacity) {
            return Err(invalid("instance capacity must be 1..1000"));
        }
        Ok(Self {
            id,
            world_id,
            lifecycle: InstanceLifecycle::Created,
            capacity,
            created_at: now,
            started_at: None,
            revision: Revision::from_u64(1),
        })
    }

    /// Returns id.
    #[must_use]
    pub const fn id(&self) -> InstanceId {
        self.id
    }

    /// Returns world_id.
    #[must_use]
    pub const fn world_id(&self) -> WorldId {
        self.world_id
    }

    /// Returns lifecycle.
    #[must_use]
    pub const fn lifecycle(&self) -> InstanceLifecycle {
        self.lifecycle
    }

    /// Returns capacity.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Returns created_at.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns started_at.
    #[must_use]
    pub const fn started_at(&self) -> Option<Timestamp> {
        self.started_at
    }

    /// Returns revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Transitions to running. Idempotent if already running.
    ///
    /// # Errors
    ///
    /// Returns invalid-value when already stopping/stopped, or overflow.
    pub fn start(&mut self, now: Timestamp) -> Result<bool, DomainError> {
        match self.lifecycle {
            InstanceLifecycle::Running => return Ok(false),
            InstanceLifecycle::Created => {}
            _ => return Err(invalid("instance cannot start from stopping/stopped")),
        }
        self.lifecycle = InstanceLifecycle::Running;
        self.started_at = Some(now);
        self.revision = self.revision.next()?;
        Ok(true)
    }

    /// Transitions to stopping.
    ///
    /// # Errors
    ///
    /// Returns invalid-value when not running, or overflow.
    pub fn stop(&mut self) -> Result<bool, DomainError> {
        match self.lifecycle {
            InstanceLifecycle::Stopping | InstanceLifecycle::Stopped => return Ok(false),
            InstanceLifecycle::Running | InstanceLifecycle::Created => {}
        }
        self.lifecycle = InstanceLifecycle::Stopping;
        self.revision = self.revision.next()?;
        Ok(true)
    }

    /// Restores an instance from persisted storage without applying transitions.
    ///
    /// Validates domain invariants (capacity range, revision bounds) but does not
    /// enforce lifecycle transitions, so a storage adapter can rehydrate any
    /// persisted state faithfully.
    ///
    /// # Errors
    ///
    /// Returns `InvalidValue` when `capacity` is outside `1..=1000`, when
    /// `revision` is `0` (persistence requires `revision >= 1` per `0004` migration),
    /// or when `started_at` is inconsistent with `lifecycle` (`Created` must not
    /// have `started_at`, `Running` must have it).
    pub fn from_persisted(
        id: InstanceId,
        world_id: WorldId,
        lifecycle: InstanceLifecycle,
        capacity: u32,
        created_at: Timestamp,
        started_at: Option<Timestamp>,
        revision: Revision,
    ) -> Result<Self, DomainError> {
        if !(1..=1000).contains(&capacity) {
            return Err(invalid("instance capacity must be 1..1000"));
        }
        if revision.as_u64() == 0 {
            return Err(invalid("instance revision must be >= 1"));
        }
        match (lifecycle, started_at) {
            (InstanceLifecycle::Created, Some(_)) => {
                return Err(invalid("created instance must not have started_at"));
            }
            (InstanceLifecycle::Running, None) => {
                return Err(invalid("running instance must have started_at"));
            }
            _ => {}
        }
        Ok(Self {
            id,
            world_id,
            lifecycle,
            capacity,
            created_at,
            started_at,
            revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{InstanceLifecycle, WorldInstance};
    use crate::{InstanceId, Timestamp, WorldId};

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(1_000).expect("valid")
    }

    #[test]
    fn lifecycle_transitions() {
        let mut inst = WorldInstance::new(InstanceId::generate(), WorldId::generate(), 50, now())
            .expect("valid");
        assert_eq!(inst.lifecycle(), InstanceLifecycle::Created);
        assert!(inst.start(now()).expect("start"));
        assert_eq!(inst.lifecycle(), InstanceLifecycle::Running);
        assert!(!inst.start(now()).expect("idempotent"));
        assert!(inst.stop().expect("stop"));
        assert_eq!(inst.lifecycle(), InstanceLifecycle::Stopping);
    }

    #[test]
    fn rejects_invalid_capacity() {
        assert!(WorldInstance::new(InstanceId::generate(), WorldId::generate(), 0, now()).is_err());
    }

    #[test]
    fn from_persisted_restores_all_fields() {
        let id = InstanceId::generate();
        let world_id = WorldId::generate();
        let created = now();
        let started = Timestamp::from_unix_millis(2_000).expect("valid");
        let rev = crate::Revision::from_u64(7);
        let inst = WorldInstance::from_persisted(
            id,
            world_id,
            InstanceLifecycle::Running,
            123,
            created,
            Some(started),
            rev,
        )
        .expect("valid persisted");
        assert_eq!(inst.id(), id);
        assert_eq!(inst.world_id(), world_id);
        assert_eq!(inst.lifecycle(), InstanceLifecycle::Running);
        assert_eq!(inst.capacity(), 123);
        assert_eq!(inst.created_at(), created);
        assert_eq!(inst.started_at(), Some(started));
        assert_eq!(inst.revision(), rev);
    }

    #[test]
    fn from_persisted_validates_capacity() {
        let id = InstanceId::generate();
        let world_id = WorldId::generate();
        let err = WorldInstance::from_persisted(
            id,
            world_id,
            InstanceLifecycle::Created,
            0,
            now(),
            None,
            crate::Revision::from_u64(1),
        )
        .expect_err("capacity 0 must be rejected");
        assert_eq!(err.kind(), crate::DomainErrorKind::InvalidValue);
    }

    #[test]
    fn from_persisted_validates_revision() {
        let err = WorldInstance::from_persisted(
            InstanceId::generate(),
            WorldId::generate(),
            InstanceLifecycle::Created,
            10,
            now(),
            None,
            crate::Revision::from_u64(0),
        )
        .expect_err("revision 0 must be rejected");
        assert_eq!(err.kind(), crate::DomainErrorKind::InvalidValue);
    }

    #[test]
    fn from_persisted_validates_started_at_consistency() {
        // Created must not have started_at
        assert!(
            WorldInstance::from_persisted(
                InstanceId::generate(),
                WorldId::generate(),
                InstanceLifecycle::Created,
                10,
                now(),
                Some(now()),
                crate::Revision::from_u64(1),
            )
            .is_err()
        );
        // Running must have started_at
        assert!(
            WorldInstance::from_persisted(
                InstanceId::generate(),
                WorldId::generate(),
                InstanceLifecycle::Running,
                10,
                now(),
                None,
                crate::Revision::from_u64(2),
            )
            .is_err()
        );
        // Stopping without started_at is allowed (may never have started)
        assert!(
            WorldInstance::from_persisted(
                InstanceId::generate(),
                WorldId::generate(),
                InstanceLifecycle::Stopping,
                10,
                now(),
                None,
                crate::Revision::from_u64(2),
            )
            .is_ok()
        );
    }

    #[test]
    fn from_persisted_does_not_transition() {
        let created = now();
        let started = Timestamp::from_unix_millis(2_000).expect("valid");
        let inst = WorldInstance::from_persisted(
            InstanceId::generate(),
            WorldId::generate(),
            InstanceLifecycle::Stopped,
            50,
            created,
            Some(started),
            crate::Revision::from_u64(5),
        )
        .expect("valid");
        // Must retain Stopped without transitioning
        assert_eq!(inst.lifecycle(), InstanceLifecycle::Stopped);
        assert_eq!(inst.started_at(), Some(started));
        assert_eq!(inst.revision().as_u64(), 5);
    }
}
