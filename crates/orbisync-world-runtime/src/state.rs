//! In-memory instance state (ephemeral, spec §10.1).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use orbisync_domain::{
    DomainError, DomainErrorKind, Entity, EntityId, InstanceId, PresenceId, Revision, UserId,
};

/// Ephemeral state owned by the instance actor.
#[derive(Debug)]
pub struct InstanceState {
    instance_id: InstanceId,
    revision: Revision,
    /// PresenceId -> UserId
    members: HashMap<PresenceId, UserId>,
    present_presences: Arc<HashSet<PresenceId>>,
    /// EntityId -> Entity
    entities: HashMap<EntityId, Entity>,
}

impl InstanceState {
    pub(crate) fn install_revision(&mut self, revision: Revision) {
        self.revision = revision;
    }
    /// Creates empty state.
    #[must_use]
    pub fn new(instance_id: InstanceId) -> Self {
        Self {
            instance_id,
            revision: Revision::INITIAL,
            members: HashMap::new(),
            present_presences: Arc::new(HashSet::new()),
            entities: HashMap::new(),
        }
    }

    /// Restores durable state while deliberately starting with no live members.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when an entity belongs to another
    /// instance, exceeds the instance revision, or duplicates another ID.
    pub fn from_persisted(
        instance_id: InstanceId,
        revision: Revision,
        entities: Vec<Entity>,
    ) -> Result<Self, DomainError> {
        let mut restored = HashMap::with_capacity(entities.len());
        for entity in entities {
            if entity.instance_id() != instance_id {
                return Err(DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "persisted entity belongs to a different instance",
                ));
            }
            if entity.revision() > revision {
                return Err(DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "persisted entity revision exceeds instance revision",
                ));
            }
            if restored.insert(entity.id(), entity).is_some() {
                return Err(DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "persisted state contains a duplicate entity id",
                ));
            }
        }
        Ok(Self {
            instance_id,
            revision,
            members: HashMap::new(),
            present_presences: Arc::new(HashSet::new()),
            entities: restored,
        })
    }

    /// Returns instance_id.
    #[must_use]
    pub const fn instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// Returns current revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns member count.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Returns presence/user pairs for the coordinator's interest pass.
    #[must_use]
    pub fn members_snapshot(&self) -> Vec<(PresenceId, UserId)> {
        self.members
            .iter()
            .map(|(&presence, &user)| (presence, user))
            .collect()
    }

    /// Shared presence index, replaced only when membership changes.
    pub(crate) fn present_presences(&self) -> Arc<HashSet<PresenceId>> {
        Arc::clone(&self.present_presences)
    }

    /// Returns whether a presence is a member.
    #[must_use]
    pub fn is_member(&self, presence: PresenceId) -> bool {
        self.members.contains_key(&presence)
    }

    /// Adds a member. Returns false when capacity would be exceeded.
    #[must_use]
    pub fn add_member(&mut self, presence: PresenceId, user: UserId, capacity: u32) -> bool {
        if self.members.len() >= capacity as usize {
            return false;
        }
        if self.members.insert(presence, user).is_none() {
            Arc::make_mut(&mut self.present_presences).insert(presence);
        }
        true
    }

    /// Removes a member.
    pub fn remove_member(&mut self, presence: PresenceId) -> bool {
        let removed = self.members.remove(&presence).is_some();
        if removed {
            Arc::make_mut(&mut self.present_presences).remove(&presence);
        }
        removed
    }

    /// Returns the user for a presence, if member.
    #[must_use]
    pub fn user_for_presence(&self, presence: PresenceId) -> Option<UserId> {
        self.members.get(&presence).copied()
    }

    /// Inserts or replaces an entity.
    pub fn upsert_entity(&mut self, entity: Entity) {
        self.entities.insert(entity.id(), entity);
    }

    /// Removes and returns an entity if present.
    pub fn remove_entity(&mut self, id: EntityId) -> Option<Entity> {
        self.entities.remove(&id)
    }

    /// Returns true when an entity exists.
    #[must_use]
    pub fn contains_entity(&self, id: EntityId) -> bool {
        self.entities.contains_key(&id)
    }

    /// Returns entity.
    #[must_use]
    pub fn get_entity(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    /// Returns mutable entity.
    pub fn get_entity_mut(&mut self, id: EntityId) -> Option<&mut Entity> {
        self.entities.get_mut(&id)
    }

    /// Returns number of entities.
    #[must_use]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Returns a cloned snapshot of all entities for checkpointing.
    #[must_use]
    pub fn entities_snapshot(&self) -> Vec<Entity> {
        self.entities.values().cloned().collect()
    }

    /// Iterates over all entities by reference (for interest filtering).
    pub fn iter_entities(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }

    /// Iterates over entities that have a transform, yielding `(entity, position)`.
    ///
    /// This exposes positions for interest filtering without leaking storage details
    /// (`server::interest_filter` consumes it).
    pub fn entities_with_transform(
        &self,
    ) -> impl Iterator<Item = (&Entity, orbisync_domain::Transform)> {
        self.entities
            .values()
            .filter_map(|e| e.transform().map(|t| (e, t)))
    }

    /// Iterates over entities with their `Vec3` positions when a transform exists.
    pub fn entity_positions(&self) -> impl Iterator<Item = (EntityId, orbisync_domain::Vec3)> + '_ {
        self.entities
            .values()
            .filter_map(|e| e.transform().map(|t| (e.id(), t.position())))
    }

    /// Advances revision.
    ///
    /// # Errors
    ///
    /// Returns overflow.
    pub fn advance_revision(&mut self) -> Result<Revision, DomainError> {
        self.revision = self.revision.next()?;
        Ok(self.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceState;
    use orbisync_domain::{InstanceId, PresenceId, UserId};

    #[test]
    fn presence_cache_shares_unchanged_members_and_detaches_on_membership_change() {
        let mut state = InstanceState::new(InstanceId::generate());
        let first = PresenceId::generate();
        let second = PresenceId::generate();
        let empty = state.present_presences();
        assert!(state.add_member(first, UserId::generate(), 1));
        let joined = state.present_presences();
        assert!(empty.is_empty());
        assert!(joined.contains(&first));
        assert!(!state.add_member(second, UserId::generate(), 1));
        state.advance_revision().unwrap();
        assert!(!state.remove_member(second));
        assert!(std::sync::Arc::ptr_eq(&joined, &state.present_presences()));
        assert!(state.remove_member(first));
        assert!(state.present_presences().is_empty());
        assert!(
            joined.contains(&first),
            "previous readers keep their snapshot"
        );
        assert!(state.add_member(second, UserId::generate(), 1));
        assert!(!state.present_presences().contains(&first));
        assert!(state.present_presences().contains(&second));
    }

    #[test]
    fn member_management_respects_capacity() {
        let mut state = InstanceState::new(InstanceId::generate());
        let p1 = PresenceId::generate();
        let p2 = PresenceId::generate();
        assert!(state.add_member(p1, UserId::generate(), 1));
        assert!(!state.add_member(p2, UserId::generate(), 1));
        assert_eq!(state.member_count(), 1);
    }
}
