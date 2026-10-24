//! Count-only runtime diagnostics. Samples tolerate concurrent lifecycle changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use orbisync_application::metrics::{Gauge, OperationalGauge};
use orbisync_domain::Clock;

use crate::realtime_ws::RealtimeState;

/// Owns diagnostics/reclamation task lifetimes, including failed startup and
/// cancellation of the serving future. Tasks cannot outlive pool shutdown.
#[derive(Default)]
pub struct OperationalTasks(Vec<tokio::task::JoinHandle<()>>);

impl OperationalTasks {
    /// Transfers task ownership to the server lifetime.
    pub fn push(&mut self, task: tokio::task::JoinHandle<()>) {
        self.0.push(task);
    }
    /// Cancels and joins read-only/background collectors before pool closure.
    pub async fn stop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
        for task in self.0.drain(..) {
            let _cancelled = task.await;
        }
    }
}
impl Drop for OperationalTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Tracks effects owned by maintenance while durable writes retry. It never
/// retains payloads or high-cardinality identifiers in metrics.
pub struct PersistenceBacklog {
    clock: Arc<dyn Clock>,
    entries: Mutex<PersistenceEntries>,
}

#[derive(Default)]
struct PersistenceEntries {
    next_id: u64,
    batches: HashMap<u64, PendingBatch>,
}

struct PendingBatch {
    remaining: u64,
    started_at_millis: i64,
}

impl PersistenceBacklog {
    /// Creates an empty tracker with the runtime clock.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            entries: Mutex::new(PersistenceEntries::default()),
        }
    }

    /// Starts ownership of a drained batch. Dropping the guard on cancellation
    /// removes work that is no longer owned by the in-memory worker.
    #[must_use]
    pub fn track(self: &Arc<Self>, count: usize) -> PersistenceBatch {
        self.track_since(count, None)
    }

    /// Preserves the oldest enqueue age when an actor hands off accepted work.
    /// The age follows its batch until every effect has finished retrying.
    #[must_use]
    pub fn track_since(
        self: &Arc<Self>,
        count: usize,
        oldest: Option<std::time::Instant>,
    ) -> PersistenceBatch {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let id = entries.next_id;
        entries.next_id = entries.next_id.wrapping_add(1);
        if count != 0 {
            entries.batches.insert(
                id,
                PendingBatch {
                    remaining: count as u64,
                    started_at_millis: self
                        .clock
                        .now()
                        .to_unix_millis()
                        .unwrap_or(0)
                        .saturating_sub(oldest.map_or(0, |since| {
                            i64::try_from(since.elapsed().as_millis()).unwrap_or(i64::MAX)
                        })),
                },
            );
        }
        PersistenceBatch {
            backlog: Arc::clone(self),
            id,
        }
    }

    /// Returns outstanding work units and oldest age in seconds.
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64) {
        let now = self.clock.now().to_unix_millis().unwrap_or(0);
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .batches
            .values()
            .fold((0, 0), |(count, age), batch| {
                (
                    count + batch.remaining,
                    age.max(now.saturating_sub(batch.started_at_millis).max(0) as u64 / 1000),
                )
            })
    }
}

/// Lifetime of one owned effects batch.
pub struct PersistenceBatch {
    backlog: Arc<PersistenceBacklog>,
    id: u64,
}

impl PersistenceBatch {
    /// Records one confirmed durable effect.
    pub fn complete_one(&self) {
        let mut entries = self
            .backlog
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(batch) = entries.batches.get_mut(&self.id) {
            batch.remaining = batch.remaining.saturating_sub(1);
            if batch.remaining == 0 {
                entries.batches.remove(&self.id);
            }
        }
    }
}

impl Drop for PersistenceBatch {
    fn drop(&mut self) {
        self.backlog
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .batches
            .remove(&self.id);
    }
}

impl RealtimeState {
    /// Publishes one bounded, identifier-free sample. Grace-held memberships
    /// are covered owners and do not count as connection ownership anomalies.
    pub fn refresh_operational_metrics(&self, backlog: &PersistenceBacklog) {
        use OperationalGauge as O;
        let ownership = self.resume_store.ownership_snapshot(|instance, presence| {
            self.registry.is_present(instance, presence) == Some(true)
        });
        let memberships = self.registry.member_count() as u64;
        let joined = self.delivery.total_sender_count() as u64;
        let queued = self.registry.pending_persistence();
        let owned = backlog.snapshot();
        let entities = (queued.0 + owned.0, queued.1.max(owned.1));
        let outcomes = self
            .command_dedup
            .pending_persistence(self.clock.now().to_unix_millis().unwrap_or(0));
        let generations = self
            .generation
            .as_ref()
            .map_or((0, 0), |service| service.pending_persistence());
        let now = self.clock.now().to_unix_millis().unwrap_or(0) / 1000;
        let samples = [
            (O::PresenceConnected, ownership.connected),
            (O::PresenceGraceWaiting, ownership.grace_waiting),
            (O::PresenceReclaimWaiting, ownership.reclaim_waiting),
            (O::PresenceMemberships, memberships),
            (
                O::MembershipsWithoutBinding,
                memberships.saturating_sub(ownership.bound_memberships),
            ),
            (
                O::BindingsWithoutMembership,
                ownership.bindings_without_membership,
            ),
            (O::JoinedConnections, joined),
            (
                O::ConnectionOwnershipMismatch,
                joined.abs_diff(ownership.connected),
            ),
            (
                O::PersistencePending,
                entities.0 + outcomes.0 + generations.0,
            ),
            (
                O::PersistenceOldestPendingSeconds,
                entities.1.max(outcomes.1).max(generations.1),
            ),
            (O::PersistenceEntityEvents, entities.0),
            (O::PersistenceCommandOutcomes, outcomes.0),
            (O::PersistenceGenerationSnapshots, generations.0),
        ];
        for (kind, value) in samples {
            self.metrics_recorder.set(
                Gauge::Operational { kind },
                i64::try_from(value).unwrap_or(i64::MAX),
            );
        }
        self.metrics_recorder.set(
            Gauge::Operational {
                kind: O::PresenceObservedUnixSeconds,
            },
            now,
        );
        if let Some(generation) = &self.generation {
            self.metrics_recorder.set(
                Gauge::Operational {
                    kind: O::LatestCheckpointUnixSeconds,
                },
                generation.latest_checkpoint_unix_seconds(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PersistenceBacklog;
    use orbisync_domain::Timestamp;
    use orbisync_testkit::FixedClock;
    use std::sync::Arc;

    #[test]
    fn persistence_backlog_keeps_retry_age_and_cleans_cancelled_batches() {
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(10_000).expect("time"),
        ));
        let backlog = Arc::new(PersistenceBacklog::new(clock.clone()));
        let batch = backlog.track(3);
        clock.advance_millis(9_000);
        batch.complete_one();
        assert_eq!(backlog.snapshot(), (2, 9));
        let newer = backlog.track(1);
        drop(batch);
        assert_eq!(backlog.snapshot(), (1, 0));
        newer.complete_one();
        assert_eq!(backlog.snapshot(), (0, 0));
        let queued = backlog.track_since(
            1,
            Some(std::time::Instant::now() - std::time::Duration::from_secs(40)),
        );
        assert_eq!(
            backlog.snapshot(),
            (1, 40),
            "draining must not reset an accepted batch's waiting age"
        );
        clock.advance_millis(10_000);
        assert_eq!(backlog.snapshot(), (1, 50));
        drop(queued);
    }
}
