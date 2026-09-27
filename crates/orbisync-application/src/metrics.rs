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
            Self::Instance => "instance",
        }
    }
}

/// Counter metrics – only the 3 variants needed for D1-A are defined here.
/// Adding more without wiring is intentionally forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
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
