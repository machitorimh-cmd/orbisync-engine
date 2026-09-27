//! Bounded priority outbound queue per connection.
//!
//! Implements `mobile-resume-interest-backpressure.md` §5 (latest-wins) and
//! §8 (slow consumer / backpressure) and the mailbox priority analogy from
//! `state-and-runtime.md` §3.2.
//!
//! Each realtime connection owns one queue. Capacity is bounded and comes from
//! [`orbisync_config::RealtimeConfig::outbound_queue_capacity`] (default 256,
//! `observability-and-config.md` §6.5, `mobile-resume-interest-backpressure.md`
//! §8.2 MRIB-08). The queue is split into four logical lanes with strict
//! priority:
//!
//! ```text
//! Control (ServerHello, ErrorMessage, HeartbeatAck, ResyncRequired) – highest
//! Reliable event lane (EntityCommand, DomainEvent) – middle (never silently dropped)
//! Reliable bulk lane (Snapshot chunk) – middle-low (deprioritized vs event lane so
//!   a large Snapshot does not starve Control/event; MRIB-08)
//! Latest-wins (StateDelta / TransformInput delivery) – normal, same key overwrites older
//! ```
//!
//! Invariants:
//! - Latest-wins aggregation key is `(entity_id, component)` per §5.1. The
//!   simplified helper `push_latest` uses `entity_id` alone as the key (the
//!   caller can use `push_latest_with_component` for per-component aggregation).
//!   An existing key is overwritten without increasing `len()`; the old value
//!   counts as a `state_updates_dropped_total` increment.
//! - Reliable lanes never silently drop: if `len() >= capacity` the enqueue
//!   returns [`QueueError::ReliableOverflow`] / [`QueueError::BulkOverflow`]
//!   and the caller must warn and disconnect (spec §18.2, §8.2). This matches
//!   the runbook: flush failures are recorded, not hidden.
//! - Latest-wins enqueues on a full queue drop the incoming update (or, when
//!   possible, evict the oldest latest-wins entry) and increment the dropped
//!   counter, but never cause a disconnect (spec §8.3).
//! - `pop()` / `drain()` respect priority `Control > Reliable(event) >
//!   Reliable(bulk) > Latest-wins` and preserve FIFO within each lane.
//!
//! Dependency rule (`repo-crate-conventions.md` §3.2): this module depends only
//! on `orbisync_config` and std. It is used by `realtime_delivery` and
//! `realtime_gateway` but does not call them.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use orbisync_application::metrics::{Counter, Gauge, MetricsRecorder, NoopMetrics};
use orbisync_config::RealtimeConfig;

/// Default capacity that matches `RealtimeConfig::default().outbound_queue_capacity`.
pub const DEFAULT_CAPACITY: usize = 256;

/// Error returned when a reliable enqueue would exceed the bounded capacity.
///
/// The caller must treat this as a slow-consumer disconnect signal: log a warning
/// (`reliable_queue_overflow_total` ++), notify the connection, and close it.
/// The payload is **not** silently dropped (spec §8.2, §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum QueueError {
    /// Event lane (EntityCommand / DomainEvent) overflow.
    ReliableOverflow,
    /// Bulk lane (Snapshot chunk) overflow.
    BulkOverflow,
    /// Control lane overflow – indicates a connection anomaly.
    ControlOverflow,
    /// A revisioned key cannot be retained without losing another key.
    LatestOverflow,
    /// Different content claimed the same committed field boundary.
    RevisionConflict,
}

impl core::fmt::Display for QueueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReliableOverflow => write!(f, "reliable queue overflow (capacity exceeded)"),
            Self::BulkOverflow => write!(f, "reliable bulk queue overflow (capacity exceeded)"),
            Self::ControlOverflow => write!(f, "control queue overflow (capacity exceeded)"),
            Self::LatestOverflow => write!(f, "revisioned latest queue overflow"),
            Self::RevisionConflict => write!(f, "conflicting committed field revision"),
        }
    }
}

impl std::error::Error for QueueError {}

/// Result of a latest-wins enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatestEnqueueResult {
    /// Inserted as a new key.
    Inserted,
    /// Overwrote an existing key; the old value was dropped (counts toward
    /// `state_updates_dropped_total`).
    Replaced,
    /// Dropped because the queue was full and no latest-wins slot could be
    /// reclaimed (also counts as dropped).
    DroppedFull,
}

/// Result of a commit-aware enqueue. Stale and duplicate input preserve the
/// existing queued value and do not consume a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionedEnqueueResult {
    /// A previously absent field was queued.
    Inserted,
    /// A newer revision replaced this field's queued value.
    Replaced,
    /// An older revision was ignored.
    Stale,
    /// Identical content at the same revision was already queued.
    Duplicate,
}

#[derive(Debug)]
struct LatestEntry {
    payload: Vec<u8>,
    committed: Option<(u64, Vec<u8>)>,
}
impl LatestEntry {
    fn bytes(&self) -> usize {
        self.payload.len()
            + self
                .committed
                .as_ref()
                .map_or(0, |(_, content)| content.len() + 8)
    }
}

/// Bounded priority queue per connection.
pub struct OutboundQueue {
    capacity: usize,
    control: VecDeque<Vec<u8>>,
    reliable: VecDeque<Vec<u8>>,
    reliable_bulk: VecDeque<Vec<u8>>,
    /// Latest-wins map: key -> payload. Insertion order is tracked in
    /// `latest_order` so `pop()` can emit in FIFO order of first insertion
    /// (overwrites keep the original position, mirroring §5.1 "上書き").
    latest: HashMap<String, LatestEntry>,
    latest_order: VecDeque<String>,
    /// Number of latest-wins updates dropped (overwrites + capacity drops).
    dropped_latest: u64,
    /// Number of reliable overflow events (for `reliable_queue_overflow_total`).
    reliable_overflow: u64,
    /// Number of bulk overflow events.
    bulk_overflow: u64,
    /// Number of control overflow events.
    control_overflow: u64,
    queued_depth_max: usize,
    queued_bytes: usize,
    queued_bytes_max: usize,
    metrics: Arc<dyn MetricsRecorder>,
}

impl std::fmt::Debug for OutboundQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundQueue")
            .field("capacity", &self.capacity)
            .field("control", &self.control)
            .field("reliable", &self.reliable)
            .field("reliable_bulk", &self.reliable_bulk)
            .field("latest", &self.latest)
            .field("latest_order", &self.latest_order)
            .field("dropped_latest", &self.dropped_latest)
            .field("reliable_overflow", &self.reliable_overflow)
            .field("bulk_overflow", &self.bulk_overflow)
            .field("control_overflow", &self.control_overflow)
            .field("queued_depth_max", &self.queued_depth_max)
            .field("queued_bytes", &self.queued_bytes)
            .field("queued_bytes_max", &self.queued_bytes_max)
            .finish_non_exhaustive()
    }
}

impl OutboundQueue {
    /// Creates a queue with the given bounded capacity.
    ///
    /// `capacity` must be >= 1; use [`DEFAULT_CAPACITY`] (256) when coming from
    /// validated config.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::new_with_metrics(capacity, Arc::new(NoopMetrics))
    }

    /// Creates a queue and attaches the process metrics recorder.
    #[must_use]
    pub fn new_with_metrics(capacity: usize, metrics: Arc<dyn MetricsRecorder>) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            control: VecDeque::new(),
            reliable: VecDeque::new(),
            reliable_bulk: VecDeque::new(),
            latest: HashMap::new(),
            latest_order: VecDeque::new(),
            dropped_latest: 0,
            reliable_overflow: 0,
            bulk_overflow: 0,
            control_overflow: 0,
            queued_depth_max: 0,
            queued_bytes: 0,
            queued_bytes_max: 0,
            metrics,
        }
    }

    /// Creates a queue from validated [`RealtimeConfig`].
    ///
    /// Reads `outbound_queue_capacity` (default 256). The config is already
    /// validated to be >= 1, so this cannot fail.
    #[must_use]
    pub fn from_config(config: &RealtimeConfig) -> Self {
        Self::new(config.outbound_queue_capacity as usize)
    }

    /// Creates a queue from validated config with metrics instrumentation.
    #[must_use]
    pub fn from_config_with_metrics(
        config: &RealtimeConfig,
        metrics: Arc<dyn MetricsRecorder>,
    ) -> Self {
        Self::new_with_metrics(config.outbound_queue_capacity as usize, metrics)
    }

    /// Returns the configured capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns total number of enqueued items across all lanes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.control.len() + self.reliable.len() + self.reliable_bulk.len() + self.latest.len()
    }

    /// Returns `true` when no item is enqueued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` when `len() >= capacity`.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }

    /// Remaining slots.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.len())
    }

    /// Number of latest-wins updates dropped (overwrites + full-queue drops).
    ///
    /// Maps to `state_updates_dropped_total` (spec §8.3, §9.1).
    #[must_use]
    pub const fn dropped_latest(&self) -> u64 {
        self.dropped_latest
    }

    /// Number of reliable overflow events (`reliable_queue_overflow_total`).
    #[must_use]
    pub const fn reliable_overflow_count(&self) -> u64 {
        self.reliable_overflow
    }

    /// Number of bulk overflow events.
    #[must_use]
    pub const fn bulk_overflow_count(&self) -> u64 {
        self.bulk_overflow
    }

    /// Enqueues a Control message (highest priority).
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::ControlOverflow`] when the queue is full. The
    /// caller should treat a control overflow as a connection anomaly and
    /// disconnect after warning.
    pub fn push_control(&mut self, payload: Vec<u8>) -> Result<(), QueueError> {
        if self.is_full() {
            self.control_overflow += 1;
            return Err(QueueError::ControlOverflow);
        }
        self.enqueue_bytes(payload.len());
        self.control.push_back(payload);
        self.record_depth();
        Ok(())
    }

    /// Enqueues a reliable event (EntityCommand / DomainEvent).
    ///
    /// Never silently drops. On capacity exhaustion returns
    /// [`QueueError::ReliableOverflow`] so the caller can warn and disconnect
    /// (spec §8.2: "警告後に接続切断（silent drop しない）").
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::ReliableOverflow`] when `len() >= capacity`.
    pub fn push_reliable(&mut self, payload: Vec<u8>) -> Result<(), QueueError> {
        if self.is_full() {
            self.reliable_overflow += 1;
            return Err(QueueError::ReliableOverflow);
        }
        self.enqueue_bytes(payload.len());
        self.reliable.push_back(payload);
        self.record_depth();
        Ok(())
    }

    /// Enqueues a reliable bulk message (Snapshot chunk).
    ///
    /// Bulk lane is deprioritized versus the event lane so a large Snapshot
    /// does not starve Control/event (MRIB-08).
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::BulkOverflow`] when full.
    pub fn push_reliable_bulk(&mut self, payload: Vec<u8>) -> Result<(), QueueError> {
        if self.is_full() {
            self.bulk_overflow += 1;
            return Err(QueueError::BulkOverflow);
        }
        self.enqueue_bytes(payload.len());
        self.reliable_bulk.push_back(payload);
        self.record_depth();
        Ok(())
    }

    /// Enqueues a latest-wins update keyed by `entity_id`.
    ///
    /// Same `entity_id` overwrites the older value without increasing `len()`
    /// (spec §5.1). Overwrites increment [`Self::dropped_latest`].
    /// If the queue is full and the key is new, the incoming update is dropped
    /// (or, when latest-wins entries exist, the oldest latest-wins entry is
    /// evicted to make room) and the dropped counter is incremented. This never
    /// returns an error – latest-wins drops are expected and do not cause a
    /// disconnect (spec §8.3).
    pub fn push_latest(
        &mut self,
        entity_id: impl Into<String>,
        payload: Vec<u8>,
    ) -> LatestEnqueueResult {
        self.push_latest_with_component(entity_id, "", payload)
    }

    /// Enqueues a latest-wins update keyed by `(entity_id, component)`.
    ///
    /// `component` may be empty to mean "default component". Key is
    /// `entity_id` when component is empty, otherwise `"{entity_id}:{component}"`
    /// per §5.1.
    pub fn push_latest_with_component(
        &mut self,
        entity_id: impl Into<String>,
        component: impl AsRef<str>,
        payload: Vec<u8>,
    ) -> LatestEnqueueResult {
        let entity_id = entity_id.into();
        let component = component.as_ref();
        let key = if component.is_empty() {
            entity_id
        } else {
            format!("{entity_id}:{component}")
        };

        if self.latest.contains_key(&key) {
            let old_len = self.latest.get(&key).map_or(0, LatestEntry::bytes);
            self.queued_bytes = self.queued_bytes.saturating_sub(old_len);
            self.enqueue_bytes(payload.len());
            if let Some(existing) = self.latest.get_mut(&key) {
                *existing = LatestEntry {
                    payload,
                    committed: None,
                };
            }
            self.dropped_latest += 1;
            self.metrics.incr(Counter::StateUpdatesDroppedTotal);
            self.record_depth();
            return LatestEnqueueResult::Replaced;
        }

        // New key: need capacity.
        if self.is_full() {
            // Try to evict oldest latest-wins entry to make room (keeps queue
            // bounded without dropping a reliable message).
            if let Some(oldest_key) = self.latest_order.front().cloned() {
                // Evict oldest latest
                self.latest_order.pop_front();
                if let Some(oldest) = self.latest.remove(&oldest_key) {
                    self.queued_bytes = self.queued_bytes.saturating_sub(oldest.bytes());
                }
                self.dropped_latest += 1; // the evicted entry counts as dropped
                self.metrics.incr(Counter::StateUpdatesDroppedTotal);
                self.enqueue_bytes(payload.len());
                self.latest.insert(
                    key.clone(),
                    LatestEntry {
                        payload,
                        committed: None,
                    },
                );
                self.latest_order.push_back(key);
                self.record_depth();
                return LatestEnqueueResult::Inserted;
            }
            // No latest entry to evict (queue is all reliable/control): drop incoming
            self.dropped_latest += 1;
            self.metrics.incr(Counter::StateUpdatesDroppedTotal);
            return LatestEnqueueResult::DroppedFull;
        }

        self.enqueue_bytes(payload.len());
        self.latest.insert(
            key.clone(),
            LatestEntry {
                payload,
                committed: None,
            },
        );
        self.latest_order.push_back(key);
        self.record_depth();
        LatestEnqueueResult::Inserted
    }

    /// Commit-aware latest lane. Keys must represent one independent entity
    /// field. Never evicts a distinct key: losing its only value requires resync.
    /// Canonical content excludes volatile transport headers and counts against
    /// the same aggregate byte budget as queued payloads.
    pub fn push_latest_revisioned(
        &mut self,
        key: String,
        revision: u64,
        canonical: Vec<u8>,
        payload: Vec<u8>,
        max_bytes: usize,
    ) -> Result<RevisionedEnqueueResult, QueueError> {
        let previous = self.latest.get(&key);
        if let Some((stamp, content)) = previous.and_then(|entry| entry.committed.as_ref()) {
            if revision < *stamp {
                return Ok(RevisionedEnqueueResult::Stale);
            }
            if revision == *stamp {
                return if *content == canonical {
                    Ok(RevisionedEnqueueResult::Duplicate)
                } else {
                    Err(QueueError::RevisionConflict)
                };
            }
        }
        let old_bytes = previous.map_or(0, LatestEntry::bytes);
        let replaced = previous.is_some();
        if !replaced && self.is_full() {
            return Err(QueueError::LatestOverflow);
        }
        let entry = LatestEntry {
            payload,
            committed: Some((revision, canonical)),
        };
        if entry.bytes() > max_bytes.saturating_sub(self.queued_bytes.saturating_sub(old_bytes)) {
            return Err(QueueError::LatestOverflow);
        }
        self.queued_bytes = self.queued_bytes.saturating_sub(old_bytes);
        self.enqueue_bytes(entry.bytes());
        if !replaced {
            self.latest_order.push_back(key.clone());
        }
        self.latest.insert(key, entry);
        if replaced {
            self.dropped_latest += 1;
            self.metrics.incr(Counter::StateUpdatesDroppedTotal);
        }
        self.record_depth();
        Ok(if replaced {
            RevisionedEnqueueResult::Replaced
        } else {
            RevisionedEnqueueResult::Inserted
        })
    }

    /// Returns the next payload respecting priority
    /// `Control > Reliable(event) > Reliable(bulk) > Latest-wins`.
    ///
    /// Within each lane FIFO is preserved. For latest-wins, order follows
    /// `latest_order` which is insertion order; overwritten keys keep their
    /// original position.
    #[must_use]
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        if let Some(payload) = self.control.pop_front() {
            self.dequeue_bytes(payload.len());
            return Some(payload);
        }
        if let Some(payload) = self.reliable.pop_front() {
            self.dequeue_bytes(payload.len());
            return Some(payload);
        }
        if let Some(payload) = self.reliable_bulk.pop_front() {
            self.dequeue_bytes(payload.len());
            return Some(payload);
        }
        if let Some(key) = self.latest_order.pop_front() {
            let payload = self.latest.remove(&key)?;
            self.dequeue_bytes(payload.bytes());
            return Some(payload.payload);
        }
        None
    }

    /// Drains up to `limit` items in priority order.
    #[must_use]
    pub fn drain_batch(&mut self, limit: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for _ in 0..limit {
            match self.pop() {
                Some(p) => out.push(p),
                None => break,
            }
        }
        out
    }

    /// Drains all items in priority order.
    #[must_use]
    pub fn drain_all(&mut self) -> Vec<Vec<u8>> {
        let len = self.len();
        self.drain_batch(len)
    }

    /// Clears all lanes and resets metrics that are per-connection counters.
    ///
    /// Metrics `dropped_latest` / overflow counts are retained so the caller
    /// can still report them before clearing; use `clear_metrics` to reset them
    /// separately if needed.
    pub fn clear(&mut self) {
        self.control.clear();
        self.reliable.clear();
        self.reliable_bulk.clear();
        self.latest.clear();
        self.latest_order.clear();
        self.queued_bytes = 0;
        self.record_depth();
    }

    /// Returns queue depth per lane (for `outbound_queue_depth` metric).
    #[must_use]
    pub fn depth(&self) -> QueueDepth {
        QueueDepth {
            control: self.control.len(),
            reliable: self.reliable.len(),
            reliable_bulk: self.reliable_bulk.len(),
            latest: self.latest.len(),
            total: self.len(),
            capacity: self.capacity,
            bytes: self.queued_bytes,
            bytes_max: self.queued_bytes_max,
        }
    }

    fn enqueue_bytes(&mut self, bytes: usize) {
        self.queued_bytes = self.queued_bytes.saturating_add(bytes);
        self.queued_bytes_max = self.queued_bytes_max.max(self.queued_bytes);
    }

    fn dequeue_bytes(&mut self, bytes: usize) {
        self.queued_bytes = self.queued_bytes.saturating_sub(bytes);
        self.record_depth();
    }

    fn record_depth(&mut self) {
        let depth = i64::try_from(self.len()).unwrap_or(i64::MAX);
        let bytes = i64::try_from(self.queued_bytes).unwrap_or(i64::MAX);
        self.queued_depth_max = self.queued_depth_max.max(self.len());
        let depth_max = i64::try_from(self.queued_depth_max).unwrap_or(i64::MAX);
        let bytes_max = i64::try_from(self.queued_bytes_max).unwrap_or(i64::MAX);
        self.metrics.set(Gauge::OutboundQueueDepth, depth);
        self.metrics.set(Gauge::OutboundQueueDepthMax, depth_max);
        self.metrics.set(Gauge::OutboundQueueBytes, bytes);
        self.metrics.set(Gauge::OutboundQueueBytesMax, bytes_max);
    }
}

/// Snapshot of queue depths for metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDepth {
    /// Control lane depth.
    pub control: usize,
    /// Reliable event lane depth.
    pub reliable: usize,
    /// Reliable bulk lane depth.
    pub reliable_bulk: usize,
    /// Latest-wins lane depth (unique keys).
    pub latest: usize,
    /// Total depth.
    pub total: usize,
    /// Configured capacity.
    pub capacity: usize,
    /// Total payload bytes currently queued.
    pub bytes: usize,
    /// Process-lifetime high-water mark for payload bytes.
    pub bytes_max: usize,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    #[test]
    fn revisioned_latest_preserves_newer_content_and_distinct_keys() {
        use super::RevisionedEnqueueResult::*;
        let mut q = queue(2);
        let mut put = |key: &str, rev, value: u8| {
            q.push_latest_revisioned(key.into(), rev, vec![value], vec![value], 100)
        };
        assert_eq!(put("a", 102, 2), Ok(Inserted));
        assert_eq!(put("b", 101, 3), Ok(Inserted));
        assert_eq!(put("a", 101, 1), Ok(Stale));
        assert_eq!(put("a", 102, 2), Ok(Duplicate));
        assert_eq!(put("a", 102, 4), Err(QueueError::RevisionConflict));
        assert_eq!(put("c", 103, 5), Err(QueueError::LatestOverflow));
        assert_eq!(q.drain_all(), vec![vec![2], vec![3]]);
        assert_eq!(q.depth().bytes, 0);
    }

    #[test]
    fn revisioned_latest_counts_metadata_and_all_lanes_in_byte_budget() {
        let mut q = queue(4);
        q.push_reliable(vec![0; 4]).unwrap();
        // Four reliable bytes plus one canonical byte, one payload byte and u64.
        q.push_latest_revisioned("a".into(), 1, vec![1], vec![1], 14)
            .unwrap();
        assert_eq!(q.depth().bytes, 14);
        assert_eq!(
            q.push_latest_revisioned("a".into(), 2, vec![2; 2], vec![2], 14),
            Err(QueueError::LatestOverflow)
        );
        assert_eq!(q.drain_all(), vec![vec![0; 4], vec![1]]);
        assert_eq!(q.depth().bytes, 0);
    }
    use super::{DEFAULT_CAPACITY, LatestEnqueueResult, OutboundQueue, QueueError};
    use orbisync_config::Config;

    fn queue(capacity: usize) -> OutboundQueue {
        OutboundQueue::new(capacity)
    }

    #[test]
    fn test_default_capacity_from_config_is_256() {
        let config = Config::default();
        assert_eq!(config.realtime.outbound_queue_capacity, 256);
        let q = OutboundQueue::from_config(&config.realtime);
        assert_eq!(q.capacity(), 256);
        assert_eq!(q.capacity(), DEFAULT_CAPACITY);
    }

    #[test]
    fn test_latest_wins_overwrites_same_entity_id() {
        let mut q = queue(10);
        let r1 = q.push_latest("entity-1", b"x=1.0".to_vec());
        assert_eq!(r1, LatestEnqueueResult::Inserted);
        assert_eq!(q.len(), 1);
        assert_eq!(q.dropped_latest(), 0);

        let r2 = q.push_latest("entity-1", b"x=1.3".to_vec());
        assert_eq!(r2, LatestEnqueueResult::Replaced);
        // overwriting does not increase len, but counts as dropped (old value)
        assert_eq!(q.len(), 1);
        assert_eq!(q.dropped_latest(), 1);

        let popped = q.pop().expect("must have one entry");
        assert_eq!(
            popped,
            b"x=1.3".to_vec(),
            "latest-wins must keep only newest value"
        );
        assert!(q.is_empty());
    }

    #[test]
    fn test_latest_wins_same_entity_overwrites_preserves_len_and_updates_payload() {
        let mut q = queue(10);
        q.push_latest("e1", b"v1".to_vec());
        q.push_latest("e1", b"v2".to_vec());
        q.push_latest("e1", b"v3".to_vec());
        // x=1.0 -> x=1.1 -> x=1.2 -> x=1.3 with queue stalled should keep only x=1.3
        assert_eq!(q.len(), 1);
        assert_eq!(q.dropped_latest(), 2);
        q.push_latest("e1", b"x=1.3".to_vec());
        assert_eq!(q.len(), 1);
        let payload = q.pop().unwrap();
        assert_eq!(payload, b"x=1.3".to_vec());
    }

    #[test]
    fn test_latest_wins_with_component_key_is_distinct_per_component() {
        let mut q = queue(10);
        q.push_latest_with_component("entity-1", "transform", b"pos-a".to_vec());
        q.push_latest_with_component("entity-1", "custom:health", b"hp-100".to_vec());
        assert_eq!(
            q.len(),
            2,
            "different components must be distinct keys per §5.1"
        );
        // Same entity+component overwrites
        let r = q.push_latest_with_component("entity-1", "transform", b"pos-b".to_vec());
        assert_eq!(r, LatestEnqueueResult::Replaced);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn test_latest_wins_multiple_entities_independent() {
        let mut q = queue(10);
        q.push_latest("e1", b"a".to_vec());
        q.push_latest("e2", b"b".to_vec());
        assert_eq!(q.len(), 2);
        q.push_latest("e1", b"a2".to_vec());
        assert_eq!(q.len(), 2);
        // Drain order: FIFO of insertion (e1 inserted first)
        let mut drained = q.drain_all();
        // Both e1 and e2 present; latest for e1 is a2
        assert_eq!(drained.len(), 2);
        // First popped should be e1's latest (insertion order), second e2
        // We verify both payloads present (order is FIFO of first-seen key)
        drained.sort();
        let mut expected = vec![b"a2".to_vec(), b"b".to_vec()];
        expected.sort();
        assert_eq!(drained, expected);
    }

    #[test]
    fn test_reliable_overflow_returns_error_and_never_silently_drops() {
        let mut q = queue(3);
        assert_eq!(q.push_reliable(b"evt1".to_vec()), Ok(()));
        assert_eq!(q.push_reliable(b"evt2".to_vec()), Ok(()));
        assert_eq!(q.push_reliable(b"evt3".to_vec()), Ok(()));
        assert_eq!(q.len(), 3);
        assert!(q.is_full());

        let err = q
            .push_reliable(b"evt4".to_vec())
            .expect_err("must overflow");
        assert_eq!(err, QueueError::ReliableOverflow);
        assert_eq!(q.reliable_overflow_count(), 1);
        // Queue must remain at capacity, evt4 not inserted, no silent drop
        assert_eq!(q.len(), 3);
        assert_eq!(q.depth().reliable, 3);

        // Another overflow increments counter again
        let err2 = q
            .push_reliable(b"evt5".to_vec())
            .expect_err("overflow again");
        assert_eq!(err2, QueueError::ReliableOverflow);
        assert_eq!(q.reliable_overflow_count(), 2);
    }

    #[test]
    fn queue_depth_tracks_payload_bytes_and_high_water_marks() {
        let mut q = queue(4);
        q.push_reliable(vec![0; 3]).unwrap();
        q.push_reliable_bulk(vec![0; 5]).unwrap();
        let depth = q.depth();
        assert_eq!(depth.total, 2);
        assert_eq!(depth.bytes, 8);
        assert_eq!(depth.bytes_max, 8);

        assert_eq!(q.pop().map(|payload| payload.len()), Some(3));
        let depth = q.depth();
        assert_eq!(depth.total, 1);
        assert_eq!(depth.bytes, 5);
        assert_eq!(depth.bytes_max, 8);
    }

    #[test]
    fn test_reliable_bulk_overflow_never_silently_drops() {
        let mut q = queue(2);
        q.push_reliable_bulk(b"chunk1".to_vec()).unwrap();
        q.push_reliable_bulk(b"chunk2".to_vec()).unwrap();
        let err = q
            .push_reliable_bulk(b"chunk3".to_vec())
            .expect_err("bulk overflow");
        assert_eq!(err, QueueError::BulkOverflow);
        assert_eq!(q.bulk_overflow_count(), 1);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn test_reliable_overflow_does_not_affect_latest_wins_aggregation_of_existing_key() {
        let mut q = queue(2);
        q.push_reliable(b"evt1".to_vec()).unwrap();
        q.push_latest("e1", b"pos1".to_vec());
        assert_eq!(q.len(), 2);
        assert!(q.is_full());

        // Reliable overflow: disconnect signal
        assert_eq!(
            q.push_reliable(b"evt2".to_vec()),
            Err(QueueError::ReliableOverflow)
        );
        // Overwriting existing latest key must still succeed even though queue is full,
        // because it does not increase len (§5.1 overwriting).
        let r = q.push_latest("e1", b"pos2".to_vec());
        assert_eq!(r, LatestEnqueueResult::Replaced);
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped_latest(), 1);
    }

    #[test]
    fn test_latest_wins_new_key_when_full_evicts_oldest_latest() {
        let mut q = queue(2);
        q.push_latest("e1", b"a".to_vec());
        q.push_latest("e2", b"b".to_vec());
        assert!(q.is_full());
        // New latest key while full: should evict oldest latest (e1) and insert e3
        let r = q.push_latest("e3", b"c".to_vec());
        assert_eq!(r, LatestEnqueueResult::Inserted);
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped_latest(), 1);
        // e1 should have been evicted, e2 and e3 remain
        let mut drained = q.drain_all();
        drained.sort();
        let mut expected = vec![b"b".to_vec(), b"c".to_vec()];
        expected.sort();
        assert_eq!(drained, expected);
    }

    #[test]
    fn test_latest_wins_new_key_when_full_of_reliable_drops_incoming() {
        let mut q = queue(2);
        q.push_reliable(b"evt1".to_vec()).unwrap();
        q.push_reliable(b"evt2".to_vec()).unwrap();
        assert!(q.is_full());
        // No latest-wins slot to reclaim; new latest is dropped
        let r = q.push_latest("e1", b"pos".to_vec());
        assert_eq!(r, LatestEnqueueResult::DroppedFull);
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped_latest(), 1);
        assert_eq!(q.depth().latest, 0);
        assert_eq!(q.depth().reliable, 2);
    }

    #[test]
    fn test_priority_order_control_before_reliable_before_latest() {
        let mut q = queue(10);
        q.push_latest("e1", b"latest".to_vec());
        q.push_reliable(b"reliable".to_vec()).unwrap();
        q.push_reliable_bulk(b"bulk".to_vec()).unwrap();
        q.push_control(b"control".to_vec()).unwrap();

        assert_eq!(q.pop().unwrap(), b"control".to_vec());
        assert_eq!(q.pop().unwrap(), b"reliable".to_vec());
        assert_eq!(q.pop().unwrap(), b"bulk".to_vec());
        assert_eq!(q.pop().unwrap(), b"latest".to_vec());
        assert!(q.pop().is_none());
    }

    #[test]
    fn test_total_capacity_enforced_across_all_lanes() {
        let mut q = queue(4);
        q.push_control(b"c1".to_vec()).unwrap();
        q.push_reliable(b"r1".to_vec()).unwrap();
        q.push_reliable_bulk(b"b1".to_vec()).unwrap();
        q.push_latest("e1", b"l1".to_vec());
        assert_eq!(q.len(), 4);
        assert!(q.is_full());
        assert_eq!(q.remaining(), 0);

        // Any new reliable or control must overflow
        assert_eq!(
            q.push_reliable(b"r2".to_vec()),
            Err(QueueError::ReliableOverflow)
        );
        assert_eq!(
            q.push_control(b"c2".to_vec()),
            Err(QueueError::ControlOverflow)
        );
        // Latest overwrite of existing key still works
        assert_eq!(
            q.push_latest("e1", b"l2".to_vec()),
            LatestEnqueueResult::Replaced
        );
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn test_control_overflow_tracks_metric() {
        let mut q = queue(1);
        q.push_control(b"c1".to_vec()).unwrap();
        let err = q
            .push_control(b"c2".to_vec())
            .expect_err("control overflow");
        assert_eq!(err, QueueError::ControlOverflow);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn test_drain_batch_respects_limit_and_priority() {
        let mut q = queue(10);
        q.push_reliable(b"r1".to_vec()).unwrap();
        q.push_reliable(b"r2".to_vec()).unwrap();
        q.push_latest("e1", b"l1".to_vec());
        q.push_latest("e2", b"l2".to_vec());
        let batch = q.drain_batch(2);
        assert_eq!(batch, vec![b"r1".to_vec(), b"r2".to_vec()]);
        assert_eq!(q.len(), 2);
        let remainder = q.drain_all();
        assert_eq!(remainder.len(), 2);
    }
}
