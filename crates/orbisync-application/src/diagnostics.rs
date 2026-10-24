//! Read-only operational diagnostics exposed to authenticated operators.
//!
//! This is deliberately separate from readiness. Readiness answers whether
//! traffic may be served; diagnostics provides a compact, privileged summary
//! for investigating why a running service is unhealthy or falling behind.

use orbisync_domain::Timestamp;

use crate::ApplicationError;

/// Sampled lifecycle counts. Grace retention is expected; the discrepancy
/// fields describe ownership/membership anomalies rather than normal reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalPresenceSnapshot {
    /// Last runtime sampling time; absent before the first sample.
    pub observed_at: Option<Timestamp>,
    /// Presences owned by a connected generation.
    pub connected: u64,
    /// Disconnected presences still inside their normal grace period.
    pub grace_waiting: u64,
    /// Expired or explicitly evicted memberships awaiting Leave acknowledgement.
    pub reclaim_waiting: u64,
    /// Memberships currently counted by runtime actors.
    pub memberships: u64,
    /// Actor memberships without any lifecycle binding, including grace bindings.
    pub memberships_without_binding: u64,
    /// Lifecycle bindings whose membership is missing from the runtime.
    pub bindings_without_membership: u64,
    /// Registered joined delivery connections, excluding pre-join transports.
    pub joined_connections: u64,
    /// Absolute difference between joined connections and connected owners.
    /// Short reconnect handoffs can transiently increase this value.
    pub connection_ownership_mismatch: u64,
}

/// Work units awaiting persistence. Categories are stages, not unique commands:
/// an outcome and the checkpoint that makes it durable are separate work units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalPersistenceSnapshot {
    /// Total units across the categories below.
    pub pending: u64,
    /// Age of the oldest unit, zero when no work is pending.
    pub oldest_pending_seconds: u64,
    /// Accepted entity effects queued in actors or owned by maintenance workers.
    pub entity_events: u64,
    /// Legacy command outcomes awaiting confirmed durability (including dirty outcomes).
    pub command_outcomes: u64,
    /// Retained generation snapshots awaiting durable resolution.
    pub generation_snapshots: u64,
}

/// Exhaustive bounded counters for rejected Resume attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalResumeRejections {
    /// Hello and Resume tokens disagree or the request has no usable token.
    pub token_mismatch: u64,
    /// Token, instance, or retained history is unavailable.
    pub history_unavailable: u64,
    /// The requested history fell outside retention.
    pub revision_gap: u64,
    /// The client revision exceeds the server's revision.
    pub future_revision: u64,
    /// Current participation authorization was denied.
    pub not_authorized: u64,
    /// The presence was retired or rebound before Resume could claim it.
    pub ownership_lost: u64,
}

/// Configured queue limits included beside observed queue depths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationalQueueLimits {
    /// Capacity of one instance's control mailbox.
    pub control_per_instance: u64,
    /// Capacity of one instance's latest-wins transform mailbox.
    pub transform_per_instance: u64,
    /// Capacity of one instance's entity mailbox.
    pub entity_per_instance: u64,
    /// Capacity of one realtime connection's outbound queue.
    pub outbound_per_connection: u64,
}

/// Last observed depths for bounded runtime queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalQueueSnapshot {
    /// Last observed control mailbox depth.
    pub control: i64,
    /// Last observed transform mailbox depth.
    pub transform: i64,
    /// Last observed entity mailbox depth.
    pub entity: i64,
    /// Last observed outbound queue depth.
    pub outbound: i64,
    /// Process-lifetime outbound queue depth high-water mark.
    pub outbound_high_water: i64,
    /// Last observed outbound queue bytes.
    pub outbound_bytes: i64,
    /// Process-lifetime outbound queue byte high-water mark.
    pub outbound_bytes_high_water: i64,
}

/// Rejections grouped by the bounded rate-limit scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalRateLimitSnapshot {
    /// Per-connection rejections.
    pub connection: u64,
    /// Per-user rejections.
    pub user: u64,
    /// Per-source-IP rejections.
    pub ip: u64,
    /// Password-hash concurrency rejections.
    pub password_hash: u64,
    /// Per-instance rejections.
    pub instance: u64,
}

/// Most recent successful completion of each periodic retention job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalRetentionSnapshot {
    /// Realtime ticket cleanup.
    pub realtime_tickets: Option<Timestamp>,
    /// Idempotency record cleanup.
    pub idempotency_records: Option<Timestamp>,
    /// Privacy-sensitive audit source-IP cleanup.
    pub audit_source_ips: Option<Timestamp>,
    /// Delivered extension outbox cleanup.
    pub extension_outbox: Option<Timestamp>,
}

/// Compact operator-facing snapshot. It contains no credentials, payloads, or
/// high-cardinality identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalDiagnostics {
    /// Snapshot collection time.
    pub collected_at: Timestamp,
    /// Most recent durable checkpoint creation time across all instances.
    pub latest_checkpoint_at: Option<Timestamp>,
    /// Checkpoint save operations that returned an error.
    pub checkpoint_save_failures_total: u64,
    /// Checkpoint restore operations that returned an error.
    pub checkpoint_restore_failures_total: u64,
    /// Size-limit save rejections, retained as a failure subtype.
    pub checkpoint_save_rejected_total: u64,
    /// Size-limit restore rejections, retained as a failure subtype.
    pub checkpoint_restore_rejected_total: u64,
    /// Last observed queue values.
    pub queues: OperationalQueueSnapshot,
    /// Configured per-instance/per-connection queue capacities.
    pub queue_limits: OperationalQueueLimits,
    /// Current accepted realtime connections.
    pub active_connections: i64,
    /// Sampled Presence lifecycle and consistency counts.
    pub presence: OperationalPresenceSnapshot,
    /// Pending persistence work and its oldest age.
    pub persistence: OperationalPersistenceSnapshot,
    /// Resume rejection counts since process start.
    pub resume_rejections: OperationalResumeRejections,
    /// Rate-limit rejection totals since process start.
    pub rate_limit_rejections: OperationalRateLimitSnapshot,
    /// Pending extension outbox events.
    pub extension_outbox_pending: i64,
    /// Extension deliveries currently in flight.
    pub extension_deliveries_in_flight: i64,
    /// Retained extension dead-letter rows.
    pub extension_dead_letters: u64,
    /// Last successful retention ticks. `None` means no successful tick has
    /// completed since this process started.
    pub retention: OperationalRetentionSnapshot,
}

/// Port consumed by the HTTP adapter for protected operational diagnostics.
#[async_trait::async_trait]
pub trait OperationalDiagnosticsPort: Send + Sync + 'static {
    /// Collects a fresh read-only snapshot.
    ///
    /// # Errors
    ///
    /// Returns a redacted port failure when a required diagnostics source is
    /// unavailable. Partial values are not returned as if they were complete.
    async fn snapshot(&self) -> Result<OperationalDiagnostics, ApplicationError>;
}
