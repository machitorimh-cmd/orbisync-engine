//! Batched persistence effects and outbox (`state-and-runtime.md` §1.2, HIGH-002).
//!
//! `world-runtime` owns the canonical transient state and must not touch the
//! database directly (`architecture.md` §3.1). After a command spawns,
//! deletes, or updates a component, the actor records an
//! [`EntityPersistenceEvent`] to this in-memory outbox. The coordinator
//! drains it in an independent storage task (alongside [`crate::extension::Outbox`])
//! and forwards the batch to `PersistentEntityStore`. Tick wakeups are coalesced
//! while storage is busy; simulation continues. Before applying a command that
//! needs persistence, the actor checks this buffer's capacity and rejects the
//! command if full, preserving previously accepted entity writes.
//!
//! Kept separate from [`crate::extension::Outbox`] because `ExtensionEvent`
//! is the public webhook contract and does not carry the durable fields
//! (`kind`, `transform`, `visibility`, component payloads) the persistence
//! port needs; reusing it would mean widening that public contract for an
//! internal concern.

pub use orbisync_application::EntityPersistenceEvent;

/// In-memory outbox for persistence effects (HIGH-002).
#[derive(Debug, Clone)]
pub struct PersistenceOutbox {
    events: Vec<EntityPersistenceEvent>,
    capacity: usize,
}

impl Default for PersistenceOutbox {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistenceOutbox {
    /// Default outbox capacity, matching `extension::Outbox::DEFAULT_CAPACITY`.
    pub const DEFAULT_CAPACITY: usize = 256;

    /// Creates an empty outbox.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// Creates an empty outbox with a bounded capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    /// Pushes an event, dropping the oldest event when the safety cap is full.
    ///
    /// Events are normally drained every tick; this cap is only a safety
    /// valve against unbounded growth if draining stops. Returns `true` when
    /// an older event was dropped.
    pub fn push(&mut self, event: EntityPersistenceEvent) -> bool {
        let dropped = self.events.len() >= self.capacity;
        if dropped {
            self.events.remove(0);
        }
        self.events.push(event);
        dropped
    }

    /// Returns the configured maximum number of buffered events.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns number of buffered events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns `true` when no events are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Drains all events, returning them in insertion order.
    #[must_use]
    pub fn drain(&mut self) -> Vec<EntityPersistenceEvent> {
        core::mem::take(&mut self.events)
    }

    pub(crate) fn drain_prefix(&mut self, count: usize) -> Vec<EntityPersistenceEvent> {
        self.events.drain(..count).collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{EntityPersistenceEvent, PersistenceOutbox};
    use orbisync_domain::{EntityId, InstanceId};

    fn deleted(instance_id: InstanceId) -> EntityPersistenceEvent {
        EntityPersistenceEvent::Deleted {
            entity_id: EntityId::generate(),
            instance_id,
        }
    }

    #[test]
    fn push_and_drain_preserves_order() {
        let mut outbox = PersistenceOutbox::new();
        assert!(outbox.is_empty());
        let instance = InstanceId::generate();
        let e1 = deleted(instance);
        let e2 = deleted(instance);
        outbox.push(e1.clone());
        outbox.push(e2.clone());
        assert_eq!(outbox.len(), 2);
        let drained = outbox.drain();
        assert_eq!(drained, vec![e1, e2]);
        assert!(outbox.is_empty());
    }

    #[test]
    fn bounded_outbox_drops_oldest_event() {
        let mut outbox = PersistenceOutbox::with_capacity(2);
        let instance = InstanceId::generate();
        let first = deleted(instance);
        let second = deleted(instance);
        let third = deleted(instance);
        assert!(!outbox.push(first));
        assert!(!outbox.push(second.clone()));
        assert!(outbox.push(third.clone()));
        assert_eq!(outbox.capacity(), 2);
        assert_eq!(outbox.drain(), vec![second, third]);
    }
}
