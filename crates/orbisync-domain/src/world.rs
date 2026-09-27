//! World definition aggregate.

use crate::{DomainError, DomainErrorKind, Revision, Timestamp, WorldId, transform::Transform};

fn invalid(detail: impl Into<String>) -> DomainError {
    DomainError::new(DomainErrorKind::InvalidValue, detail)
}

/// Lifecycle status of a world definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorldStatus {
    /// Available for instance creation and listing.
    Active,
    /// No new instances may be created.
    Archived,
}

impl WorldStatus {
    /// Returns canonical string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }

    /// Parses canonical string.
    ///
    /// # Errors
    ///
    /// Returns invalid-value when the string is not a known status.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            _ => Err(invalid("world status must be active or archived")),
        }
    }
}

/// World definition aggregate root (`domain-model.md` §3.3).
#[derive(Debug, Clone, PartialEq)]
pub struct World {
    id: WorldId,
    name: String,
    description: Option<String>,
    status: WorldStatus,
    default_spawn: Transform,
    capacity: u32,
    revision: Revision,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl World {
    /// Creates a new world definition.
    ///
    /// # Errors
    ///
    /// Returns invalid-value when name/capacity/description violate invariants.
    pub fn new(
        id: WorldId,
        name: impl Into<String>,
        description: Option<String>,
        default_spawn: Transform,
        capacity: u32,
        now: Timestamp,
    ) -> Result<Self, DomainError> {
        let name = validate_name(name.into())?;
        let description = validate_description(description)?;
        validate_capacity(capacity)?;
        Ok(Self {
            id,
            name,
            description,
            status: WorldStatus::Active,
            default_spawn,
            capacity,
            revision: Revision::from_u64(1),
            created_at: now,
            updated_at: now,
        })
    }

    /// Returns id.
    #[must_use]
    pub const fn id(&self) -> WorldId {
        self.id
    }

    /// Returns name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns status.
    #[must_use]
    pub const fn status(&self) -> WorldStatus {
        self.status
    }

    /// Returns default spawn.
    #[must_use]
    pub const fn default_spawn(&self) -> Transform {
        self.default_spawn
    }

    /// Returns capacity.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Returns revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns created_at.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns updated_at.
    #[must_use]
    pub const fn updated_at(&self) -> Timestamp {
        self.updated_at
    }

    /// Updates mutable fields, enforcing optimistic concurrency.
    ///
    /// # Errors
    ///
    /// Returns revision-mismatch or invalid-value.
    pub fn update(
        &mut self,
        expected_revision: Revision,
        name: Option<String>,
        description: Option<Option<String>>,
        default_spawn: Option<Transform>,
        capacity: Option<u32>,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        if let Some(name) = name {
            self.name = validate_name(name)?;
        }
        if let Some(desc) = description {
            self.description = validate_description(desc)?;
        }
        if let Some(spawn) = default_spawn {
            self.default_spawn = spawn;
        }
        if let Some(cap) = capacity {
            validate_capacity(cap)?;
            self.capacity = cap;
        }
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }

    /// Archives the world. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns overflow when revision cannot advance.
    pub fn archive(&mut self, now: Timestamp) -> Result<bool, DomainError> {
        if self.status == WorldStatus::Archived {
            return Ok(false);
        }
        self.status = WorldStatus::Archived;
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(true)
    }

    /// Restores a world definition from persisted storage without transitions.
    ///
    /// Validates all invariants (name, description, capacity, revision) but does
    /// not apply state transitions, allowing the storage adapter to rehydrate
    /// any persisted row faithfully.
    ///
    /// # Errors
    ///
    /// Returns `InvalidValue` when invariants are violated and `revision` is
    /// `0`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted(
        id: WorldId,
        name: String,
        description: Option<String>,
        status: WorldStatus,
        default_spawn: Transform,
        capacity: u32,
        revision: Revision,
        created_at: Timestamp,
        updated_at: Timestamp,
    ) -> Result<Self, DomainError> {
        let name = validate_name(name)?;
        let description = validate_description(description)?;
        validate_capacity(capacity)?;
        if revision.as_u64() == 0 {
            return Err(invalid("world revision must be >= 1"));
        }
        Ok(Self {
            id,
            name,
            description,
            status,
            default_spawn,
            capacity,
            revision,
            created_at,
            updated_at,
        })
    }
}

fn validate_name(name: String) -> Result<String, DomainError> {
    let len = name.chars().count();
    if !(1..=128).contains(&len) || name.chars().any(char::is_control) {
        return Err(invalid("world name must be 1..128 non-control characters"));
    }
    Ok(name)
}

fn validate_description(desc: Option<String>) -> Result<Option<String>, DomainError> {
    if let Some(value) = desc {
        if value.chars().count() > 1024 || value.chars().any(char::is_control) {
            return Err(invalid(
                "world description must be <=1024 non-control chars",
            ));
        }
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

fn validate_capacity(capacity: u32) -> Result<(), DomainError> {
    if !(1..=1000).contains(&capacity) {
        return Err(invalid("world capacity must be 1..1000"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{World, WorldStatus};
    use crate::{Revision, Timestamp, WorldId, transform::Transform};

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(1_000).expect("valid")
    }

    #[test]
    fn creates_and_updates() {
        let mut world = World::new(
            WorldId::generate(),
            "Arena",
            None,
            Transform::identity(),
            100,
            now(),
        )
        .expect("valid");
        assert_eq!(world.revision(), Revision::from_u64(1));
        let rev = world.revision();
        world
            .update(rev, Some("NewName".to_owned()), None, None, None, now())
            .expect("update");
        assert_eq!(world.name(), "NewName");
        assert_eq!(world.revision(), Revision::from_u64(2));
    }

    #[test]
    fn rejects_invalid_capacity() {
        assert!(
            World::new(
                WorldId::generate(),
                "W",
                None,
                Transform::identity(),
                0,
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn archive_is_idempotent() {
        let mut world = World::new(
            WorldId::generate(),
            "W",
            None,
            Transform::identity(),
            10,
            now(),
        )
        .expect("valid");
        assert!(world.archive(now()).expect("first archive"));
        assert!(!world.archive(now()).expect("second idempotent"));
        assert_eq!(world.status(), WorldStatus::Archived);
    }

    #[test]
    fn from_persisted_restores_all_fields() {
        let id = WorldId::generate();
        let created = now();
        let updated = Timestamp::from_unix_millis(2_000).expect("valid");
        let world = World::from_persisted(
            id,
            "Persisted".to_owned(),
            Some("desc".to_owned()),
            WorldStatus::Archived,
            Transform::identity(),
            123,
            Revision::from_u64(7),
            created,
            updated,
        )
        .expect("valid persisted");
        assert_eq!(world.id(), id);
        assert_eq!(world.name(), "Persisted");
        assert_eq!(world.description(), Some("desc"));
        assert_eq!(world.status(), WorldStatus::Archived);
        assert_eq!(world.capacity(), 123);
        assert_eq!(world.revision(), Revision::from_u64(7));
        assert_eq!(world.created_at(), created);
        assert_eq!(world.updated_at(), updated);
    }

    #[test]
    fn from_persisted_validates_capacity() {
        let err = World::from_persisted(
            WorldId::generate(),
            "W".to_owned(),
            None,
            WorldStatus::Active,
            Transform::identity(),
            0,
            Revision::from_u64(1),
            now(),
            now(),
        )
        .expect_err("capacity 0 must be rejected");
        assert_eq!(err.kind(), crate::DomainErrorKind::InvalidValue);
    }

    #[test]
    fn from_persisted_validates_revision() {
        let err = World::from_persisted(
            WorldId::generate(),
            "W".to_owned(),
            None,
            WorldStatus::Active,
            Transform::identity(),
            10,
            Revision::from_u64(0),
            now(),
            now(),
        )
        .expect_err("revision 0 must be rejected");
        assert_eq!(err.kind(), crate::DomainErrorKind::InvalidValue);
    }

    #[test]
    fn from_persisted_handles_invalid_capacity_as_port_failure_mapping() {
        // Storage layer must map DomainError::InvalidValue for capacity to PortFailure.
        // Verify that invalid capacity through from_persisted surfaces as InvalidValue,
        // which the storage adapter then maps to ApplicationErrorKind::PortFailure.
        let domain_err = World::from_persisted(
            WorldId::generate(),
            "W".to_owned(),
            None,
            WorldStatus::Active,
            Transform::identity(),
            9999,
            Revision::from_u64(1),
            now(),
            now(),
        )
        .expect_err("capacity 9999 must be rejected");
        assert_eq!(domain_err.kind(), crate::DomainErrorKind::InvalidValue);
    }
}
