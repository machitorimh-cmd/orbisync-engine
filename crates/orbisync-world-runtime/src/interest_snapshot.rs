//! Immutable, indexed interest snapshots with structural sharing.

use std::collections::{HashMap, hash_map::RandomState};
use std::hash::BuildHasher;
use std::sync::{Arc, Weak};

use orbisync_domain::EntityId;

use crate::actor::EntityInterestView;

const SHARDS: usize = 64;

/// Lightweight entity views shared by every receiver of a broadcast.
///
/// A published snapshot never changes. Updating an entity copies only its hash
/// shard and the fixed-size array of shard references, rather than all views.
/// Lookup and membership checks use the same shared index; receivers need not
/// build their own maps or live-entity sets. Iteration order is unspecified.
#[derive(Debug, Clone)]
pub struct InterestSnapshot {
    shards: [Arc<HashMap<EntityId, EntityInterestView>>; SHARDS],
    hash: RandomState,
    len: usize,
    membership: Arc<()>,
}

impl Default for InterestSnapshot {
    fn default() -> Self {
        let empty = Arc::new(HashMap::new());
        Self {
            shards: std::array::from_fn(|_| Arc::clone(&empty)),
            hash: RandomState::new(),
            len: 0,
            membership: Arc::new(()),
        }
    }
}

impl InterestSnapshot {
    fn shard(&self, id: EntityId) -> usize {
        self.hash.hash_one(id) as usize % SHARDS
    }

    /// Looks up an authoritative view without scanning other entities.
    #[must_use]
    pub fn get(&self, id: EntityId) -> Option<&EntityInterestView> {
        self.shards[self.shard(id)].get(&id)
    }

    /// Tests whether an entity is present in this complete snapshot.
    #[must_use]
    pub fn contains(&self, id: EntityId) -> bool {
        self.get(id).is_some()
    }

    /// Number of entities in the snapshot.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the snapshot contains no entities.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterates all views, for initial snapshots and diagnostics.
    pub fn iter(&self) -> impl Iterator<Item = &EntityInterestView> {
        self.shards.iter().flat_map(|shard| shard.values())
    }

    /// Identifies the entity membership of this snapshot without keeping its
    /// views alive. Position, owner and policy updates retain the same token.
    #[must_use]
    pub fn membership_token(&self) -> Weak<()> {
        Arc::downgrade(&self.membership)
    }

    pub(crate) fn insert(&mut self, view: EntityInterestView) {
        let index = self.shard(view.id);
        if Arc::make_mut(&mut self.shards[index])
            .insert(view.id, view)
            .is_none()
        {
            self.len += 1;
            self.membership = Arc::new(());
        }
    }

    pub(crate) fn remove(&mut self, id: EntityId) {
        let index = self.shard(id);
        if Arc::make_mut(&mut self.shards[index]).remove(&id).is_some() {
            self.len -= 1;
            self.membership = Arc::new(());
        }
    }
}

impl FromIterator<EntityInterestView> for InterestSnapshot {
    fn from_iter<T: IntoIterator<Item = EntityInterestView>>(iter: T) -> Self {
        let mut snapshot = Self::default();
        for view in iter {
            snapshot.insert(view);
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use orbisync_domain::{UserId, VisibilityPolicy};

    #[test]
    #[ignore = "manual bounded performance measurement"]
    fn interest_snapshot_performance_probe() {
        const ENTITIES: usize = 512;
        const UPDATES: usize = 256;
        let owner = UserId::generate();
        let views: Vec<_> = (0..ENTITIES)
            .map(|_| EntityInterestView {
                id: EntityId::generate(),
                owner: None,
                position: None,
                visibility: VisibilityPolicy::Global,
            })
            .collect();
        for indexed in [false, true] {
            let mut samples = Vec::new();
            for sample in 0..6 {
                let mut canonical: HashMap<_, _> =
                    views.iter().cloned().map(|view| (view.id, view)).collect();
                let mut shared = Arc::new(views.iter().cloned().collect::<InterestSnapshot>());
                let mut rebuilt = Arc::new(views.clone());
                let started = std::time::Instant::now();
                for view in views.iter().take(UPDATES) {
                    let changed = canonical.get_mut(&view.id).expect("entity");
                    changed.owner = Some(owner);
                    if indexed {
                        let reader = Arc::clone(&shared);
                        Arc::make_mut(&mut shared).insert(changed.clone());
                        std::hint::black_box(reader);
                    } else {
                        let reader = Arc::clone(&rebuilt);
                        rebuilt = Arc::new(canonical.values().cloned().collect());
                        std::hint::black_box(reader);
                    }
                }
                if sample > 0 {
                    samples.push(started.elapsed().as_micros());
                }
                if indexed {
                    assert_eq!(
                        shared.iter().filter(|v| v.owner == Some(owner)).count(),
                        UPDATES
                    );
                } else {
                    assert_eq!(
                        rebuilt.iter().filter(|v| v.owner == Some(owner)).count(),
                        UPDATES
                    );
                }
            }
            samples.sort_unstable();
            eprintln!(
                "INTEREST_UPDATE indexed={indexed} entities={ENTITIES} updates={UPDATES} median_us={} samples_us={samples:?}",
                samples[2]
            );
        }
    }

    #[test]
    fn updating_and_deleting_share_unaffected_shards_and_preserve_old_readers() {
        let original: InterestSnapshot = (0..512)
            .map(|_| EntityInterestView {
                id: EntityId::generate(),
                owner: None,
                position: None,
                visibility: VisibilityPolicy::Global,
            })
            .collect();
        let mut next = original.clone();
        let mut changed = original.iter().next().cloned().expect("fixture");
        let id = changed.id;
        changed.owner = Some(UserId::generate());
        next.insert(changed.clone());
        assert!(Weak::ptr_eq(
            &original.membership_token(),
            &next.membership_token()
        ));
        assert_eq!(original.get(id).expect("original").owner, None);
        assert_eq!(next.get(id), Some(&changed));
        assert_eq!(next.len(), original.len());
        assert_eq!(
            original
                .shards
                .iter()
                .zip(&next.shards)
                .filter(|(left, right)| Arc::ptr_eq(left, right))
                .count(),
            SHARDS - 1
        );
        next.remove(id);
        assert!(!Weak::ptr_eq(
            &original.membership_token(),
            &next.membership_token()
        ));
        assert!(!next.contains(id));
        assert!(original.contains(id));
        assert_eq!(next.len(), original.len() - 1);
        assert_eq!(next.iter().count(), next.len());
    }
}
