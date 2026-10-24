//! Metrics port and low-cardinality label types.
//!
//! The port defines the vocabulary that production code may use to emit
//! metrics (`observability-and-config.md` §3). High-cardinality identifiers
//! (instance_id / user_id / connection_id / entity_id) are not representable
//! as labels by construction – every label value is an `enum` with a finite
//! set of variants. Strings are never accepted as metric names or labels.

/// HTTP method label – low cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    /// GET
    Get,
    /// POST
    Post,
    /// PUT
    Put,
    /// PATCH
    Patch,
    /// DELETE
    Delete,
    /// Other method
    Other,
}

impl HttpMethod {
    /// Returns the Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Other => "OTHER",
        }
    }

    /// Maps an HTTP method string to the enum, case-insensitive.
    #[must_use]
    pub fn from_method(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            _ => Self::Other,
        }
    }
}

/// HTTP status class – low cardinality, `observability-and-config.md` §3.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpStatusClass {
    /// 1xx
    Informational,
    /// 2xx
    Success,
    /// 3xx
    Redirection,
    /// 4xx
    ClientError,
    /// 5xx
    ServerError,
    /// Other
    Other,
}

impl HttpStatusClass {
    /// Returns the Prometheus label value (`1xx` / `2xx` …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Informational => "1xx",
            Self::Success => "2xx",
            Self::Redirection => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
            Self::Other => "other",
        }
    }

    /// Maps a raw HTTP status code to the class.
    #[must_use]
    pub const fn from_status(code: u16) -> Self {
        match code {
            100..=199 => Self::Informational,
            200..=299 => Self::Success,
            300..=399 => Self::Redirection,
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            _ => Self::Other,
        }
    }
}

/// Scope for `rate_limit_rejected_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateLimitScope {
    /// Per connection
    Connection,
    /// Per user
    User,
    /// Per IP
    Ip,
    /// Password-hash worker concurrency.
    PasswordHash,
    /// Per IP and initial authentication method.
    AuthMethod,
    /// Process-wide initial authentication concurrency.
    AuthConcurrency,
    /// Process-wide temporary-subject issuance budget.
    TemporarySubjectBudget,
    /// Per instance
    Instance,
}

/// Low-cardinality instance mailbox queue label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MailboxQueue {
    /// Join, leave, administrative commands and shutdown.
    Control,
    /// Latest-wins transform inputs.
    Transform,
    /// Entity lifecycle and component commands.
    Entity,
}

impl MailboxQueue {
    /// Returns the Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Transform => "transform",
            Self::Entity => "entity",
        }
    }
}

impl RateLimitScope {
    /// Returns the Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::User => "user",
            Self::Ip => "ip",
            Self::PasswordHash => "password_hash",
            Self::AuthMethod => "auth_method",
            Self::AuthConcurrency => "auth_concurrency",
            Self::TemporarySubjectBudget => "temporary_subject_budget",
            Self::Instance => "instance",
        }
    }
}

/// Counter metrics – only the 3 variants needed for D1-A are defined here.
/// Adding more without wiring is intentionally forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// `resume_rejections_total{reason}` with a bounded vocabulary.
    ResumeRejected {
        /// Rejection category, never a credential or identifier.
        reason: ResumeRejectReason,
    },
    /// `http_requests_total{method,status}`
    HttpRequests {
        /// HTTP method
        method: HttpMethod,
        /// HTTP status class
        status: HttpStatusClass,
    },
    /// `auth_login_failures_total`
    AuthLoginFailures,
    /// `rate_limit_rejected_total{scope}`
    RateLimitRejected {
        /// Scope of the rejected request
        scope: RateLimitScope,
    },
    /// `instance_mailbox_saturated_total{queue}`.
    InstanceMailboxSaturated {
        /// Mailbox queue that rejected a send because it was full.
        queue: MailboxQueue,
    },
    /// `instance_mailbox_dropped_total{queue}`.
    InstanceMailboxDropped {
        /// Mailbox queue that dropped or coalesced an item.
        queue: MailboxQueue,
    },
    /// `extension_delivery_total{result}`.
    ExtensionDelivery {
        /// Low-cardinality delivery result.
        result: ExtensionDeliveryResult,
    },
    /// `extension_delivery_worker_failures_total`.
    ExtensionDeliveryWorkerFailure,
    /// `extension_outbox_dropped_total`.
    ExtensionOutboxDroppedTotal,
    /// `extension_outbox_deleted_total`.
    ExtensionOutboxDeletedTotal,
    /// `websocket_connections_total`.
    WebsocketConnectionsTotal,
    /// `websocket_disconnects_total{reason}`.
    WebsocketDisconnects {
        /// Low-cardinality disconnect reason.
        reason: WebsocketDisconnectReason,
    },
    /// `instance_commands_total`.
    InstanceCommandsTotal,
    /// `state_updates_dropped_total`.
    StateUpdatesDroppedTotal,
    /// `broadcast_cell_candidates_total`.
    BroadcastCellCandidatesTotal,
    /// `broadcast_full_scan_total`.
    BroadcastFullScanTotal,
    /// `resume_attempts_total`.
    ResumeAttemptsTotal,
    /// `resume_success_total`.
    ResumeSuccessTotal,
    /// `snapshot_bytes_total` (one increment per complete logical snapshot).
    SnapshotBytesTotal,
    /// `delta_bytes_total`.
    DeltaBytesTotal,
    /// `entity_persistence_failures_total`. A durable entity/component write
    /// drained from the actor outbox failed; heavier than an extension
    /// delivery miss because it risks the HIGH-002 data-loss failure mode
    /// (`state-and-runtime.md` §3.5).
    EntityPersistenceFailuresTotal,
    /// `checkpoint_save_rejected_total`: durable checkpoints refused because
    /// they exceeded the payload limit (HIGH-001 round 4 visibility).
    CheckpointSaveRejectedTotal,
    /// `checkpoint_restore_rejected_total`: a durable checkpoint row could
    /// not be restored on join because it exceeded the payload limit
    /// (HIGH-001 round 5 visibility). Kept separate from
    /// `CheckpointSaveRejectedTotal`: a save rejection leaves a live world
    /// undurable in memory. A restore rejection means an already-durable row
    /// is failing to activate on join. Operators need to distinguish those
    /// two failure shapes.
    CheckpointRestoreRejectedTotal,
    /// `checkpoint_save_failures_total`: checkpoint saves that returned an
    /// application/storage error. Size-limit rejections are included and also
    /// retain their dedicated subtype counter.
    CheckpointSaveFailuresTotal,
    /// `checkpoint_restore_failures_total`: checkpoint loads that returned an
    /// application/storage error. Size-limit rejections are included and also
    /// retain their dedicated subtype counter.
    CheckpointRestoreFailuresTotal,
}

/// Bounded reasons used by Resume rejection metrics and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResumeRejectReason {
    /// Token comparison or presence failure.
    TokenMismatch,
    /// Missing token, runtime or history.
    HistoryUnavailable,
    /// Retained history gap.
    RevisionGap,
    /// Client ahead of server.
    FutureRevision,
    /// Current authorization denied.
    NotAuthorized,
    /// Presence ownership was retired or superseded.
    OwnershipLost,
}

impl ResumeRejectReason {
    /// All supported labels, bounded independently of input.
    pub const ALL: [Self; 6] = [
        Self::TokenMismatch,
        Self::HistoryUnavailable,
        Self::RevisionGap,
        Self::FutureRevision,
        Self::NotAuthorized,
        Self::OwnershipLost,
    ];
    /// Stable Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TokenMismatch => "token_mismatch",
            Self::HistoryUnavailable => "history_unavailable",
            Self::RevisionGap => "revision_gap",
            Self::FutureRevision => "future_revision",
            Self::NotAuthorized => "not_authorized",
            Self::OwnershipLost => "ownership_lost",
        }
    }
}

/// Fixed operational gauges. No user, instance or connection IDs are labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationalGauge {
    /// Current connected Presence generations.
    PresenceConnected,
    /// Normal disconnected grace retention.
    PresenceGraceWaiting,
    /// Memberships awaiting acknowledged reclamation.
    PresenceReclaimWaiting,
    /// Runtime Memberships sampled with the binding counts.
    PresenceMemberships,
    /// Memberships lacking any lifecycle binding.
    MembershipsWithoutBinding,
    /// Lifecycle bindings lacking runtime Memberships.
    BindingsWithoutMembership,
    /// Joined delivery registrations.
    JoinedConnections,
    /// Joined connection/connected owner discrepancy.
    ConnectionOwnershipMismatch,
    /// Runtime sample Unix timestamp (seconds).
    PresenceObservedUnixSeconds,
    /// Total pending persistence work units.
    PersistencePending,
    /// Oldest pending persistence age in seconds.
    PersistenceOldestPendingSeconds,
    /// Pending entity effects.
    PersistenceEntityEvents,
    /// Pending command outcomes.
    PersistenceCommandOutcomes,
    /// Retained generation snapshots.
    PersistenceGenerationSnapshots,
    /// Latest successfully saved checkpoint creation Unix timestamp (seconds).
    LatestCheckpointUnixSeconds,
    /// Retained extension dead letters.
    ExtensionDeadLetters,
    /// Last successful diagnostics database sample Unix timestamp (seconds).
    DiagnosticsDatabaseObservedUnixSeconds,
}

impl OperationalGauge {
    /// Complete bounded vocabulary registered even before the first sample.
    pub const ALL: [Self; 17] = [
        Self::PresenceConnected,
        Self::PresenceGraceWaiting,
        Self::PresenceReclaimWaiting,
        Self::PresenceMemberships,
        Self::MembershipsWithoutBinding,
        Self::BindingsWithoutMembership,
        Self::JoinedConnections,
        Self::ConnectionOwnershipMismatch,
        Self::PresenceObservedUnixSeconds,
        Self::PersistencePending,
        Self::PersistenceOldestPendingSeconds,
        Self::PersistenceEntityEvents,
        Self::PersistenceCommandOutcomes,
        Self::PersistenceGenerationSnapshots,
        Self::LatestCheckpointUnixSeconds,
        Self::ExtensionDeadLetters,
        Self::DiagnosticsDatabaseObservedUnixSeconds,
    ];
    /// Stable metric name. The enum deliberately offers no free-form labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PresenceConnected => "presence_connected",
            Self::PresenceGraceWaiting => "presence_grace_waiting",
            Self::PresenceReclaimWaiting => "presence_reclaim_waiting",
            Self::PresenceMemberships => "presence_memberships",
            Self::MembershipsWithoutBinding => "presence_memberships_without_binding",
            Self::BindingsWithoutMembership => "presence_bindings_without_membership",
            Self::JoinedConnections => "presence_joined_connections",
            Self::ConnectionOwnershipMismatch => "presence_connection_ownership_mismatch",
            Self::PresenceObservedUnixSeconds => "presence_observed_timestamp_seconds",
            Self::PersistencePending => "persistence_pending",
            Self::PersistenceOldestPendingSeconds => "persistence_oldest_pending_seconds",
            Self::PersistenceEntityEvents => "persistence_entity_events_pending",
            Self::PersistenceCommandOutcomes => "persistence_command_outcomes_pending",
            Self::PersistenceGenerationSnapshots => "persistence_generation_snapshots_pending",
            Self::LatestCheckpointUnixSeconds => "checkpoint_latest_timestamp_seconds",
            Self::ExtensionDeadLetters => "extension_dead_letters",
            Self::DiagnosticsDatabaseObservedUnixSeconds => {
                "diagnostics_database_observed_timestamp_seconds"
            }
        }
    }
}

/// Bounded reason vocabulary for WebSocket disconnects.
///
/// This enum is deliberately finite: connection, user, instance and entity
/// identifiers must never become Prometheus label values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebsocketDisconnectReason {
    /// The peer closed the connection or the transport failed.
    Transport,
    /// The pre-authentication handshake timed out or was invalid.
    Handshake,
    /// The heartbeat deadline expired.
    HeartbeatTimeout,
    /// A reliable outbound queue overflowed.
    SlowConsumer,
    /// The server initiated a graceful shutdown.
    ServerShutdown,
    /// The protocol state machine rejected the connection.
    Protocol,
}

impl WebsocketDisconnectReason {
    /// Returns the bounded Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Handshake => "handshake",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::SlowConsumer => "slow_consumer",
            Self::ServerShutdown => "server_shutdown",
            Self::Protocol => "protocol",
        }
    }
}

/// Result label for extension delivery metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionDeliveryResult {
    /// The endpoint accepted the event.
    Success,
    /// The endpoint returned a non-success response or transport error.
    Failure,
    /// The endpoint exceeded the delivery timeout.
    Timeout,
}

impl ExtensionDeliveryResult {
    /// Returns the Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Timeout => "timeout",
        }
    }
}

/// Histogram metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Histogram {
    /// `http_request_duration_seconds{method}` – observes seconds.
    HttpRequestDuration {
        /// HTTP method
        method: HttpMethod,
    },
    /// `db_query_duration_seconds` – observes seconds.
    DbQueryDuration,
    /// `interest_visible_set_size` – observes visible entity count per viewer.
    InterestVisibleSetSize,
    /// `tick_duration_seconds` ? observes one instance tick processing duration.
    TickDuration,
    /// `realtime_application_duration_seconds` ? observes server-side realtime command processing.
    RealtimeApplicationDuration,
}

/// Gauge metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gauge {
    /// Additional bounded operational state for diagnostics and alerting.
    Operational {
        /// Fixed metric vocabulary, without identifier labels.
        kind: OperationalGauge,
    },
    /// `instance_command_queue_depth{queue}`.
    InstanceCommandQueueDepth {
        /// Mailbox queue whose pending item count is reported.
        queue: MailboxQueue,
    },
    /// `websocket_connections_current`.
    WebsocketConnectionsCurrent,
    /// `instance_members_current` across all live instances.
    InstanceMembersCurrent,
    /// Current aggregate outbound queue depth.
    OutboundQueueDepth,
    /// Process-lifetime outbound queue depth high-water mark.
    OutboundQueueDepthMax,
    /// Current aggregate outbound queue bytes.
    OutboundQueueBytes,
    /// Process-lifetime outbound queue bytes high-water mark.
    OutboundQueueBytesMax,
    /// Current number of pending extension outbox events.
    ExtensionOutboxPending,
    /// Current number of extension delivery attempts in flight.
    ExtensionDeliveryInFlight,
}

/// Port for emitting metrics.
pub trait MetricsRecorder: Send + Sync + 'static {
    /// Increments the counter by 1.
    fn incr(&self, counter: Counter);
    /// Adds `n` to the counter.
    fn add(&self, counter: Counter, n: u64);
    /// Sets the gauge to `value`.
    fn set(&self, gauge: Gauge, value: i64);
    /// Observes `value` for the histogram.
    fn observe(&self, histogram: Histogram, value: f64);
}

/// Port for exposing metrics in Prometheus text format.
pub trait MetricsExporter: Send + Sync + 'static {
    /// Returns the current metric exposition.
    fn render(&self) -> String;
}

/// No-op recorder for tests that do not need metrics.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl MetricsRecorder for NoopMetrics {
    fn incr(&self, _counter: Counter) {}
    fn add(&self, _counter: Counter, _n: u64) {}
    fn set(&self, _gauge: Gauge, _value: i64) {}
    fn observe(&self, _histogram: Histogram, _value: f64) {}
}

impl MetricsExporter for NoopMetrics {
    fn render(&self) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::RateLimitScope;

    #[test]
    fn password_hash_scope_has_a_distinct_label() {
        assert_eq!(RateLimitScope::PasswordHash.as_str(), "password_hash");
        assert_ne!(
            RateLimitScope::PasswordHash.as_str(),
            RateLimitScope::Ip.as_str()
        );
    }
}
