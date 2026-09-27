//! Realtime delivery registry: instance -> connection delivery handoffs.
//!
//! Production WebSocket connections register a sink whose interest filter and
//! bounded outbound queue are one handoff. The registry lives in `server`
//! (composition root) and is shared between WebSocket tasks via `Arc`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use orbisync_application::metrics::{Counter, MetricsRecorder, NoopMetrics};
use orbisync_domain::InstanceId;
use orbisync_interest::CellCoord;
use tokio::sync::mpsc;

type InterestViews = Arc<orbisync_world_runtime::InterestSnapshot>;

fn candidate_sink_ids(
    payload: &[u8],
    views: &orbisync_world_runtime::InterestSnapshot,
    grid: orbisync_interest::UniformGrid,
) -> Option<HashSet<CellCoord>> {
    let envelope = orbisync_realtime::decode_envelope(payload).ok()?;
    let ids = match envelope.payload? {
        orbisync_protocol::v1::envelope::Payload::StateDelta(delta) => delta
            .entities
            .into_iter()
            .map(|entity| entity.entity_id.parse().ok())
            .collect::<Option<Vec<orbisync_domain::EntityId>>>(),
        orbisync_protocol::v1::envelope::Payload::EntityCommand(command) => {
            Some(vec![command.entity_id.parse().ok()?])
        }
        _ => return None,
    }?;
    let mut cells = HashSet::new();
    for id in ids {
        let view = views.get(id)?;
        if !matches!(
            &view.visibility,
            orbisync_domain::VisibilityPolicy::Spatial { .. }
        ) {
            // Global, owner-only and role-based policies may be visible outside
            // the spatial neighbourhood, so they must retain the full fan-out.
            return None;
        }
        let position = view.position?;
        cells.extend(grid.subscribed_cells(position));
    }
    Some(cells)
}

/// Capacity of each per-connection outbound channel (M2 demo).
const PER_CONNECTION_CAPACITY: usize = 256;

/// Delivery guarantee for a broadcast payload (B-3).
///
/// `realtime_ws.rs` documents that `EntityCommand` is a reliable event and
/// "`delivery.rs` must not drop it on overflow", while `StateDelta` is
/// latest-wins and may be dropped. Before B-3 the registry took a bare
/// `Vec<u8>` and dropped both alike with a silent `let _ = try_send(..)`,
/// so the documented reliability contract was not actually enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliability {
    /// Must not be dropped. On overflow the slow consumer is disconnected.
    Reliable,
    /// May be dropped on overflow (latest-wins lane).
    LatestWins,
}

/// Result of one broadcast, so callers and tests can observe drops (B-3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BroadcastOutcome {
    /// Payload accepted by the consumer's channel.
    pub delivered: usize,
    /// Latest-wins payload dropped because the channel was full.
    pub dropped_latest_wins: usize,
    /// Reliable payload could not be queued; the consumer was disconnected.
    pub disconnected_slow_consumers: usize,
    /// Consumer had already closed its receiver.
    pub already_closed: usize,
}

/// The connection-owned delivery queue used by the realtime runtime.
///
/// The trait keeps the delivery registry independent from the interest filter
/// and from the queue implementation. Production connections register one of
/// these sinks, so filtering and queueing happen in the same handoff rather
/// than through an intermediate channel.
pub(crate) trait DeliverySink: Send + Sync {
    /// Returns the connection's current spatial cell, when indexed.
    fn viewer_cell(&self) -> Option<CellCoord>;
    /// Filters once and enqueues a payload for one connection.
    /// Called after the registry lock has been released.
    fn enqueue(
        &self,
        payload: &[u8],
        reliability: Reliability,
        views: Option<&orbisync_world_runtime::InterestSnapshot>,
    ) -> SinkOutcome;

    /// Returns whether this connection can no longer receive payloads.
    fn is_closed(&self) -> bool;
}

/// Result returned by a [`DeliverySink`] enqueue operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SinkOutcome {
    /// Payload was accepted by the connection's queue.
    pub(crate) delivered: usize,
    /// Payload was intentionally filtered out for this connection.
    pub(crate) filtered: usize,
    /// Latest-wins payload was dropped because the queue was full.
    pub(crate) dropped_latest_wins: usize,
    /// Reliable/control overflow makes the connection unusable.
    pub(crate) disconnected_slow_consumer: usize,
}

struct SinkRegistration {
    id: u64,
    sink: Arc<dyn DeliverySink>,
}

enum Registration {
    /// Compatibility registration retained for existing callers and tests.
    /// Production realtime connections use `Sink` and never create this
    /// intermediate mpsc queue.
    Legacy {
        id: u64,
        sender: mpsc::Sender<Vec<u8>>,
    },
    Sink(SinkRegistration),
}

enum RegistrationTarget {
    Legacy {
        id: u64,
        sender: mpsc::Sender<Vec<u8>>,
    },
    Sink {
        id: u64,
        sink: Arc<dyn DeliverySink>,
    },
}

impl Registration {
    fn target(&self) -> RegistrationTarget {
        match self {
            Self::Legacy { id, sender } => RegistrationTarget::Legacy {
                id: *id,
                sender: sender.clone(),
            },
            Self::Sink(sink) => RegistrationTarget::Sink {
                id: sink.id,
                sink: Arc::clone(&sink.sink),
            },
        }
    }
}

#[derive(Debug, Default)]
struct InstanceRegistrations {
    entries: HashMap<u64, Registration>,
    cells: HashMap<CellCoord, HashSet<u64>>,
    sink_cells: HashMap<u64, CellCoord>,
    // Legacy channels and sinks without a spatial position must not be lost.
    unindexed: HashSet<u64>,
}

impl InstanceRegistrations {
    fn set_cell(&mut self, id: u64, cell: CellCoord) {
        if !self.entries.contains_key(&id) {
            return;
        }
        self.remove_cell(id);
        self.unindexed.remove(&id);
        self.cells.entry(cell).or_default().insert(id);
        self.sink_cells.insert(id, cell);
    }

    fn remove_cell(&mut self, id: u64) {
        if let Some(previous) = self.sink_cells.remove(&id)
            && let Some(ids) = self.cells.get_mut(&previous)
        {
            ids.remove(&id);
            if ids.is_empty() {
                self.cells.remove(&previous);
            }
        }
    }

    fn remove(&mut self, id: u64) {
        self.entries.remove(&id);
        self.unindexed.remove(&id);
        self.remove_cell(id);
    }
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Legacy { id, .. } => f.debug_struct("Legacy").field("id", id).finish(),
            Self::Sink(registration) => f
                .debug_struct("Sink")
                .field("id", &registration.id)
                .finish(),
        }
    }
}

/// Registry mapping an instance to its member delivery handoffs.
pub struct DeliveryRegistry {
    inner: Mutex<HashMap<InstanceId, InstanceRegistrations>>,
    capacity: usize,
    metrics: Arc<dyn MetricsRecorder>,
    next_sink_id: AtomicU64,
}

impl std::fmt::Debug for DeliveryRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeliveryRegistry")
            .field("inner", &self.inner)
            .field("capacity", &self.capacity)
            .field("next_sink_id", &self.next_sink_id.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Default for DeliveryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeliveryRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_capacity_and_metrics(PER_CONNECTION_CAPACITY as u32, Arc::new(NoopMetrics))
    }

    /// Creates a registry with an explicit per-connection channel capacity.
    #[must_use]
    pub fn new_with_capacity(capacity: u32) -> Self {
        Self::new_with_capacity_and_metrics(capacity, Arc::new(NoopMetrics))
    }

    /// Creates a registry with explicit capacity and metrics instrumentation.
    #[must_use]
    pub fn new_with_capacity_and_metrics(capacity: u32, metrics: Arc<dyn MetricsRecorder>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity: (capacity as usize).max(1),
            metrics,
            next_sink_id: AtomicU64::new(1),
        }
    }

    /// Registers a new member's sender for an instance and returns the receiver.
    pub fn register(&self, instance: InstanceId) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(self.capacity);
        let mut guard = self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!(
                event = "delivery.lock_poisoned",
                error = %e,
                instance_id = %instance,
                "delivery lock poisoned in register – recovering"
            );
            e.into_inner()
        });
        let id = self.next_sink_id.fetch_add(1, Ordering::Relaxed);
        let registrations = guard.entry(instance).or_default();
        registrations
            .entries
            .insert(id, Registration::Legacy { id, sender: tx });
        registrations.unindexed.insert(id);
        rx
    }

    /// Registers a production realtime connection's queue.
    ///
    /// Unlike [`Self::register`], this does not allocate an mpsc channel. The
    /// sink owns the connection's single bounded queue and performs interest
    /// filtering before inserting into it.
    pub(crate) fn register_sink(
        self: &Arc<Self>,
        instance: InstanceId,
        sink: Arc<dyn DeliverySink>,
    ) -> DeliveryRegistration {
        let id = self.next_sink_id.fetch_add(1, Ordering::Relaxed);
        let cell = sink.viewer_cell();
        let mut guard = self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!(
                event = "delivery.lock_poisoned",
                error = %e,
                instance_id = %instance,
                "delivery lock poisoned in register_sink – recovering"
            );
            e.into_inner()
        });
        let registrations = guard.entry(instance).or_default();
        registrations
            .entries
            .insert(id, Registration::Sink(SinkRegistration { id, sink }));
        if let Some(cell) = cell {
            registrations.set_cell(id, cell);
        } else {
            registrations.unindexed.insert(id);
        }
        DeliveryRegistration {
            delivery: Arc::clone(self),
            instance,
            id,
            cell: Mutex::new(cell),
        }
    }

    /// Broadcasts `payload` to all members of `instance` honouring `reliability`.
    ///
    /// `Reliability::LatestWins` may be dropped when a consumer's channel is
    /// full. `Reliability::Reliable` must not be dropped silently: the slow
    /// consumer is disconnected (its sender is removed so the receiver ends)
    /// and a warning plus a counter-style event is emitted (B-3).
    pub fn broadcast(
        &self,
        instance: InstanceId,
        payload: Vec<u8>,
        reliability: Reliability,
    ) -> BroadcastOutcome {
        self.broadcast_inner(
            instance,
            payload,
            reliability,
            None,
            orbisync_interest::UniformGrid::default(),
        )
    }

    /// Broadcasts using one shared interest snapshot for the whole fan-out.
    pub(crate) fn broadcast_with_views(
        &self,
        instance: InstanceId,
        payload: Vec<u8>,
        reliability: Reliability,
        views: InterestViews,
        grid: orbisync_interest::UniformGrid,
    ) -> BroadcastOutcome {
        self.broadcast_inner(instance, payload, reliability, Some(views), grid)
    }

    fn broadcast_inner(
        &self,
        instance: InstanceId,
        payload: Vec<u8>,
        reliability: Reliability,
        views: Option<InterestViews>,
        grid: orbisync_interest::UniformGrid,
    ) -> BroadcastOutcome {
        // Decode and derive spatial candidates before taking the registry lock.
        // Payload decoding and entity lookup can be expensive. Neither this
        // work nor a connection's filter may hold the cross-instance lock.
        let candidate_cells = views
            .as_ref()
            .and_then(|views| candidate_sink_ids(&payload, views, grid));
        let guard = self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!(
                event = "delivery.lock_poisoned",
                error = %e,
                instance_id = %instance,
                "delivery lock poisoned in broadcast – recovering"
            );
            e.into_inner()
        });
        let mut outcome = BroadcastOutcome::default();
        let Some(registrations) = guard.get(&instance) else {
            return outcome;
        };
        let indexed = candidate_cells.is_some();
        let targets: Vec<RegistrationTarget> = if let Some(cells) = candidate_cells.as_ref() {
            self.metrics.incr(Counter::BroadcastCellCandidatesTotal);
            cells
                .iter()
                .filter_map(|cell| registrations.cells.get(cell))
                .flat_map(|ids| ids.iter())
                .chain(registrations.unindexed.iter())
                .filter_map(|id| registrations.entries.get(id))
                .map(Registration::target)
                .collect()
        } else {
            self.metrics.incr(Counter::BroadcastFullScanTotal);
            registrations
                .entries
                .values()
                .map(Registration::target)
                .collect()
        };
        drop(guard);
        if indexed {
            self.metrics.incr(Counter::BroadcastCellCandidatesTotal);
        } else {
            self.metrics.incr(Counter::BroadcastFullScanTotal);
        }
        let mut disconnect = HashSet::new();
        for target in targets {
            match target {
                RegistrationTarget::Legacy { id, sender } => match sender.try_send(payload.clone())
                {
                    Ok(()) => outcome.delivered += 1,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        outcome.already_closed += 1;
                        disconnect.insert(id);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => match reliability {
                        Reliability::LatestWins => {
                            outcome.dropped_latest_wins += 1;
                            self.metrics.incr(Counter::StateUpdatesDroppedTotal);
                            tracing::debug!(
                                event = "delivery.latest_wins_dropped",
                                instance_id = %instance,
                                "latest-wins payload dropped for a full consumer channel"
                            );
                        }
                        Reliability::Reliable => {
                            outcome.disconnected_slow_consumers += 1;
                            disconnect.insert(id);
                            tracing::warn!(
                                event = "delivery.reliable_overflow",
                                instance_id = %instance,
                                capacity = self.capacity,
                                "reliable payload could not be queued – disconnecting slow consumer"
                            );
                        }
                    },
                },
                RegistrationTarget::Sink { id, sink } => {
                    let sink_outcome = sink.enqueue(&payload, reliability, views.as_deref());
                    outcome.delivered += sink_outcome.delivered;
                    outcome.dropped_latest_wins += sink_outcome.dropped_latest_wins;
                    outcome.disconnected_slow_consumers += sink_outcome.disconnected_slow_consumer;
                    if sink.is_closed() {
                        disconnect.insert(id);
                    }
                }
            }
        }
        if !disconnect.is_empty() {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(registrations) = guard.get_mut(&instance) {
                for id in disconnect {
                    registrations.remove(id);
                }
                if registrations.entries.is_empty() {
                    guard.remove(&instance);
                }
            }
        }
        outcome
    }

    fn unregister_sink(&self, instance: InstanceId, id: u64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(registrations) = guard.get_mut(&instance) {
            registrations.remove(id);
            if registrations.entries.is_empty() {
                guard.remove(&instance);
            }
        }
    }

    pub(crate) fn update_sink_cell(&self, instance: InstanceId, id: u64, cell: CellCoord) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Never recreate an index entry after concurrent disconnect/cleanup.
        if let Some(registrations) = guard.get_mut(&instance) {
            registrations.set_cell(id, cell);
        }
    }

    /// Removes closed connection handoffs (called on disconnect).
    pub fn cleanup_closed(&self, instance: InstanceId) {
        let targets: Vec<_> = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .get(&instance)
                .map(|r| r.entries.values().map(Registration::target).collect())
                .unwrap_or_default()
        };
        for target in targets {
            let (id, closed) = match target {
                RegistrationTarget::Legacy { id, sender } => (id, sender.is_closed()),
                RegistrationTarget::Sink { id, sink } => (id, sink.is_closed()),
            };
            if closed {
                self.unregister_sink(instance, id);
            }
        }
    }

    /// Returns the number of registered senders for `instance` (including closed).
    #[must_use]
    pub fn sender_count(&self, instance: InstanceId) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!(
                event = "delivery.lock_poisoned",
                error = %e,
                instance_id = %instance,
                "delivery lock poisoned in sender_count – recovering"
            );
            e.into_inner()
        });
        guard.get(&instance).map_or(0, |r| r.entries.len())
    }

    /// Returns total sender count across all instances.
    #[must_use]
    pub fn total_sender_count(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!(
                event = "delivery.lock_poisoned",
                error = %e,
                "delivery lock poisoned in total_sender_count – recovering"
            );
            e.into_inner()
        });
        guard.values().map(|r| r.entries.len()).sum()
    }
}

/// RAII handle for a production sink registration.
pub(crate) struct DeliveryRegistration {
    delivery: Arc<DeliveryRegistry>,
    instance: InstanceId,
    id: u64,
    cell: Mutex<Option<CellCoord>>,
}

impl DeliveryRegistration {
    pub(crate) fn update_viewer_cell(&self, cell: CellCoord) {
        let mut previous = self.cell.lock().unwrap_or_else(|e| e.into_inner());
        if *previous == Some(cell) {
            return;
        }
        self.delivery.update_sink_cell(self.instance, self.id, cell);
        *previous = Some(cell);
    }
}

impl Drop for DeliveryRegistration {
    fn drop(&mut self) {
        self.delivery.unregister_sink(self.instance, self.id);
    }
}

#[cfg(test)]
mod b3_reliability_tests {
    use super::{
        BroadcastOutcome, CellCoord, DeliveryRegistry, DeliverySink, PER_CONNECTION_CAPACITY,
        Reliability, SinkOutcome,
    };
    use orbisync_domain::InstanceId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink {
        visible: bool,
        enqueued: AtomicUsize,
    }

    impl DeliverySink for CountingSink {
        fn viewer_cell(&self) -> Option<CellCoord> {
            None
        }

        fn enqueue(
            &self,
            _payload: &[u8],
            _reliability: Reliability,
            _views: Option<&orbisync_world_runtime::InterestSnapshot>,
        ) -> SinkOutcome {
            if !self.visible {
                return SinkOutcome {
                    filtered: 1,
                    ..SinkOutcome::default()
                };
            }
            self.enqueued.fetch_add(1, Ordering::Relaxed);
            SinkOutcome {
                delivered: 1,
                ..SinkOutcome::default()
            }
        }

        fn is_closed(&self) -> bool {
            false
        }
    }

    /// Fills a consumer channel to capacity without draining it.
    fn fill(registry: &DeliveryRegistry, instance: InstanceId) {
        for _ in 0..PER_CONNECTION_CAPACITY {
            let outcome = registry.broadcast(instance, vec![0u8; 4], Reliability::LatestWins);
            assert_eq!(outcome.delivered, 1, "fill must succeed until capacity");
        }
    }

    #[tokio::test]
    async fn latest_wins_is_dropped_when_consumer_is_full() {
        let registry = DeliveryRegistry::new();
        let instance = InstanceId::generate();
        let _rx = registry.register(instance);
        fill(&registry, instance);

        let outcome = registry.broadcast(instance, vec![1u8; 4], Reliability::LatestWins);

        assert_eq!(
            outcome,
            BroadcastOutcome {
                delivered: 0,
                dropped_latest_wins: 1,
                disconnected_slow_consumers: 0,
                already_closed: 0,
            },
            "latest-wins must be dropped, not disconnect the consumer"
        );
        assert_eq!(
            registry.sender_count(instance),
            1,
            "latest-wins overflow must keep the consumer connected"
        );
    }

    #[tokio::test]
    async fn reliable_overflow_disconnects_instead_of_dropping_silently() {
        let registry = DeliveryRegistry::new();
        let instance = InstanceId::generate();
        let _rx = registry.register(instance);
        fill(&registry, instance);

        let outcome = registry.broadcast(instance, vec![2u8; 4], Reliability::Reliable);

        assert_eq!(
            outcome.disconnected_slow_consumers, 1,
            "reliable overflow must disconnect the slow consumer (B-3)"
        );
        assert_eq!(
            outcome.dropped_latest_wins, 0,
            "reliable payload must never be counted as a latest-wins drop"
        );
        assert_eq!(
            registry.sender_count(instance),
            0,
            "the slow consumer's sender must be removed so the connection ends"
        );
    }

    #[tokio::test]
    async fn reliable_is_delivered_when_consumer_has_room() {
        let registry = DeliveryRegistry::new();
        let instance = InstanceId::generate();
        let mut rx = registry.register(instance);

        let outcome = registry.broadcast(instance, vec![3u8; 4], Reliability::Reliable);

        assert_eq!(outcome.delivered, 1);
        assert_eq!(outcome.disconnected_slow_consumers, 0);
        assert_eq!(rx.try_recv().expect("payload must arrive"), vec![3u8; 4]);
    }

    #[tokio::test]
    async fn a_full_consumer_does_not_affect_a_healthy_one() {
        let registry = DeliveryRegistry::new();
        let instance = InstanceId::generate();
        let _slow = registry.register(instance);
        fill(&registry, instance);
        let mut healthy = registry.register(instance);

        let outcome = registry.broadcast(instance, vec![4u8; 4], Reliability::Reliable);

        assert_eq!(outcome.delivered, 1, "the healthy consumer must receive it");
        assert_eq!(
            outcome.disconnected_slow_consumers, 1,
            "only the slow consumer is disconnected"
        );
        assert_eq!(registry.sender_count(instance), 1);
        assert_eq!(healthy.try_recv().expect("healthy consumer"), vec![4u8; 4]);
    }

    #[tokio::test]
    async fn broadcast_filters_non_visible_connections_before_queueing() {
        let registry = Arc::new(DeliveryRegistry::new());
        let instance = InstanceId::generate();
        let visible = Arc::new(CountingSink {
            visible: true,
            enqueued: AtomicUsize::new(0),
        });
        let invisible = Arc::new(CountingSink {
            visible: false,
            enqueued: AtomicUsize::new(0),
        });
        let _visible_registration = registry.register_sink(instance, visible.clone());
        let _invisible_registration = registry.register_sink(instance, invisible.clone());

        let outcome = registry.broadcast(instance, vec![1, 2, 3], Reliability::LatestWins);

        assert_eq!(outcome.delivered, 1);
        assert_eq!(visible.enqueued.load(Ordering::Relaxed), 1);
        assert_eq!(invisible.enqueued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn slow_visibility_filter_does_not_block_another_world() {
        use std::sync::{Mutex, mpsc};
        use std::time::Duration;

        struct BlockingSink {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            calls: AtomicUsize,
        }
        impl BlockingSink {
            fn filter(&self) {
                if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.entered.send(()).expect("report entry");
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release filter");
                }
            }
        }
        impl DeliverySink for BlockingSink {
            fn viewer_cell(&self) -> Option<CellCoord> {
                None
            }
            fn enqueue(
                &self,
                _: &[u8],
                _: Reliability,
                _: Option<&orbisync_world_runtime::InterestSnapshot>,
            ) -> SinkOutcome {
                self.filter();
                SinkOutcome {
                    delivered: 1,
                    ..SinkOutcome::default()
                }
            }
            fn is_closed(&self) -> bool {
                false
            }
        }
        let registry = Arc::new(DeliveryRegistry::new());
        let slow_world = InstanceId::generate();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let sink = Arc::new(BlockingSink {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            calls: AtomicUsize::new(0),
        });
        let _registration = registry.register_sink(slow_world, sink.clone());
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| registry.broadcast(slow_world, vec![1], Reliability::LatestWins));
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("slow filter started");
            scope.spawn(|| {
                let healthy_world = InstanceId::generate();
                let mut rx = registry.register(healthy_world);
                let result = registry.broadcast(healthy_world, vec![2], Reliability::LatestWins);
                assert_eq!(result.delivered, 1);
                assert_eq!(rx.try_recv().expect("healthy payload"), vec![2]);
                done_tx.send(()).expect("report healthy completion");
            });
            let healthy_completed = done_rx.recv_timeout(Duration::from_secs(1));
            release_tx
                .send(())
                .expect("release slow filter before asserting");
            assert!(
                healthy_completed.is_ok(),
                "unrelated world must progress while a filter is blocked"
            );
        });
        assert_eq!(
            sink.calls.load(Ordering::Relaxed),
            1,
            "filter only once per delivery"
        );
    }
}
