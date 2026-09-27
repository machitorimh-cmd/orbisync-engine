//! Extension events and outbox (`extension-mechanism.md` §4, `state-and-runtime.md` §2.1).
//!
//! `world-runtime` owns the canonical transient state. After a command is
//! applied, it records a fact to the in-memory [`Outbox`]. Delivery is
//! out-of-process via webhook and is not performed inside the actor
//! (`extension-mechanism.md` §2, §4.1).
//!
//! Runtime keeps a short-lived buffer. Durable persistence is owned by the
//! application/storage boundary and webhook delivery is deferred to WH2.

pub use orbisync_application::ExtensionEvent;

/// In-memory outbox for extension events (M4).
///
/// The actor pushes one [`ExtensionEvent`] per successful state transition.
/// The coordinator drains the outbox after `handle` returns and forwards the
/// batch to the delivery worker without blocking the actor tick. Events are
/// not emitted for rejected commands, preserving the "only facts" invariant
/// (`domain-model.md` §5).
#[derive(Debug, Clone)]
pub struct Outbox {
    events: Vec<ExtensionEvent>,
    capacity: usize,
}

impl Default for Outbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Outbox {
    /// Default outbox capacity, matching `realtime.outbound_queue_capacity`.
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
    /// A drop means the coordinator drain is not keeping up (or is broken).
    /// Events are normally made durable by the coordinator and store; this cap
    /// is only a safety valve to prevent an out-of-memory failure if draining
    /// stops. Durable events should not be dropped during normal operation.
    /// Returns `true` when an older event was dropped.
    pub fn push(&mut self, event: ExtensionEvent) -> bool {
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

    /// Returns a view of buffered events in insertion order.
    #[must_use]
    pub fn events(&self) -> &[ExtensionEvent] {
        &self.events
    }

    /// Drains all events, returning them in insertion order.
    #[must_use]
    pub fn drain(&mut self) -> Vec<ExtensionEvent> {
        core::mem::take(&mut self.events)
    }

    pub(crate) fn drain_prefix(&mut self, count: usize) -> Vec<ExtensionEvent> {
        self.events.drain(..count).collect()
    }

    /// Clears buffered events without returning them.
    pub fn clear(&mut self) {
        self.events.clear();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{ExtensionEvent, Outbox};
    use orbisync_domain::{EntityId, InstanceId, PresenceId, UserId};

    #[test]
    fn outbox_push_and_drain_preserves_order() {
        let mut outbox = Outbox::new();
        assert!(outbox.is_empty());
        let instance = InstanceId::generate();
        let e1 = ExtensionEvent::MemberJoined {
            instance_id: instance,
            presence_id: PresenceId::generate(),
            user_id: UserId::generate(),
        };
        let e2 = ExtensionEvent::EntitySpawned {
            instance_id: instance,
            entity_id: EntityId::generate(),
            owner: None,
        };
        outbox.push(e1.clone());
        outbox.push(e2.clone());
        assert_eq!(outbox.len(), 2);
        assert_eq!(outbox.events(), &[e1.clone(), e2.clone()]);
        let drained = outbox.drain();
        assert_eq!(drained, vec![e1, e2]);
        assert!(outbox.is_empty());
    }

    #[test]
    fn event_kind_matches_webhook_name() {
        let instance = InstanceId::generate();
        let ev = ExtensionEvent::MemberLeft {
            instance_id: instance,
            presence_id: PresenceId::generate(),
            user_id: UserId::generate(),
        };
        assert_eq!(ev.kind(), "member.left");
        assert_eq!(ev.instance_id(), Some(instance));
        let ev2 = ExtensionEvent::OwnershipTransferred {
            instance_id: instance,
            entity_id: EntityId::generate(),
            previous_owner: None,
            new_owner: Some(UserId::generate()),
        };
        assert_eq!(ev2.kind(), "entity.ownership_transferred");
    }

    #[test]
    fn clear_empties_outbox() {
        let mut outbox = Outbox::new();
        outbox.push(ExtensionEvent::EntityDeleted {
            instance_id: InstanceId::generate(),
            entity_id: EntityId::generate(),
        });
        outbox.clear();
        assert!(outbox.is_empty());
        assert_eq!(outbox.drain().len(), 0);
    }

    #[test]
    fn bounded_outbox_drops_oldest_event() {
        let mut outbox = Outbox::with_capacity(2);
        let instance = InstanceId::generate();
        let first = ExtensionEvent::EntityDeleted {
            instance_id: instance,
            entity_id: EntityId::generate(),
        };
        let second = ExtensionEvent::EntityDeleted {
            instance_id: instance,
            entity_id: EntityId::generate(),
        };
        let third = ExtensionEvent::EntityDeleted {
            instance_id: instance,
            entity_id: EntityId::generate(),
        };
        assert!(!outbox.push(first));
        assert!(!outbox.push(second.clone()));
        assert!(outbox.push(third.clone()));
        assert_eq!(outbox.capacity(), 2);
        assert_eq!(outbox.events(), &[second, third]);
    }
}
