//! Prometheus metrics implementation.
//!
//! Implements `orbisync_application::metrics` ports via `prometheus-client`.

use orbisync_application::metrics::{
    Counter, Gauge, Histogram, HttpMethod, MailboxQueue, MetricsExporter, MetricsRecorder,
    RateLimitScope,
};
use prometheus_client::encoding::text::{encode_eof, encode_registry};
use prometheus_client::metrics::counter::Counter as Pcounter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge as Pgauge;
use prometheus_client::metrics::histogram::Histogram as Phistogram;
use prometheus_client::registry::Registry;
#[cfg(windows)]
use std::mem::size_of;
use std::sync::RwLock;

/// Buckets for latency histograms – Prometheus default.
const LATENCY_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Buckets for the number of entities visible to one viewer.
const VISIBLE_SET_SIZE_BUCKETS: [f64; 10] =
    [0.0, 1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

fn latency_histogram() -> Phistogram {
    Phistogram::new(LATENCY_BUCKETS.into_iter())
}

fn visible_set_size_histogram() -> Phistogram {
    Phistogram::new(VISIBLE_SET_SIZE_BUCKETS.into_iter())
}

/// Concrete Prometheus metrics recorder and exporter.
#[allow(clippy::type_complexity)]
pub struct PrometheusMetrics {
    registry: RwLock<Registry>,
    http_requests: Family<Vec<(String, String)>, Pcounter>,
    http_duration: Family<Vec<(String, String)>, Phistogram, fn() -> Phistogram>,
    auth_failures: Pcounter,
    db_duration: Phistogram,
    interest_visible_set_size: Phistogram,
    tick_duration: Phistogram,
    realtime_application_duration: Phistogram,
    rate_limit_rejected: Family<Vec<(String, String)>, Pcounter>,
    mailbox_saturated: Family<Vec<(String, String)>, Pcounter>,
    mailbox_dropped: Family<Vec<(String, String)>, Pcounter>,
    mailbox_depth: Family<Vec<(String, String)>, Pgauge>,
    extension_delivery: Family<Vec<(String, String)>, Pcounter>,
    extension_delivery_worker_failures: Pcounter,
    extension_outbox_dropped_total: Pcounter,
    extension_outbox_deleted_total: Pcounter,
    websocket_disconnects: Family<Vec<(String, String)>, Pcounter>,
    websocket_connections_total: Pcounter,
    instance_commands_total: Pcounter,
    state_updates_dropped_total: Pcounter,
    broadcast_cell_candidates_total: Pcounter,
    broadcast_full_scan_total: Pcounter,
    resume_attempts_total: Pcounter,
    resume_success_total: Pcounter,
    snapshot_bytes_total: Pcounter,
    delta_bytes_total: Pcounter,
    entity_persistence_failures_total: Pcounter,
    checkpoint_save_rejected_total: Pcounter,
    checkpoint_restore_rejected_total: Pcounter,
    checkpoint_save_failures_total: Pcounter,
    checkpoint_restore_failures_total: Pcounter,
    websocket_connections_current: Pgauge,
    instance_members_current: Pgauge,
    outbound_queue_depth: Pgauge,
    outbound_queue_depth_max: Pgauge,
    outbound_queue_bytes: Pgauge,
    outbound_queue_bytes_max: Pgauge,
    extension_outbox_pending: Pgauge,
    extension_delivery_in_flight: Pgauge,
}

/// Typed, low-cardinality subset used by the protected operational
/// diagnostics endpoint. Prometheus remains the full metrics interface; this
/// snapshot avoids reparsing its text exposition inside the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationalMetricsSnapshot {
    /// Checkpoint save errors since process start.
    pub checkpoint_save_failures_total: u64,
    /// Checkpoint restore errors since process start.
    pub checkpoint_restore_failures_total: u64,
    /// Checkpoint save size-limit rejections since process start.
    pub checkpoint_save_rejected_total: u64,
    /// Checkpoint restore size-limit rejections since process start.
    pub checkpoint_restore_rejected_total: u64,
    /// Last observed control mailbox depth.
    pub control_queue_depth: i64,
    /// Last observed transform mailbox depth.
    pub transform_queue_depth: i64,
    /// Last observed entity mailbox depth.
    pub entity_queue_depth: i64,
    /// Current accepted realtime connections.
    pub active_connections: i64,
    /// Last observed outbound queue depth.
    pub outbound_queue_depth: i64,
    /// Process-lifetime outbound queue depth high-water mark.
    pub outbound_queue_depth_high_water: i64,
    /// Last observed outbound queue bytes.
    pub outbound_queue_bytes: i64,
    /// Process-lifetime outbound queue byte high-water mark.
    pub outbound_queue_bytes_high_water: i64,
    /// Pending extension outbox events.
    pub extension_outbox_pending: i64,
    /// Extension deliveries currently in flight.
    pub extension_deliveries_in_flight: i64,
    /// Per-connection rate-limit rejections.
    pub rate_limit_connection: u64,
    /// Per-user rate-limit rejections.
    pub rate_limit_user: u64,
    /// Per-source-IP rate-limit rejections.
    pub rate_limit_ip: u64,
    /// Password-hash concurrency rejections.
    pub rate_limit_password_hash: u64,
    /// Per-instance rate-limit rejections.
    pub rate_limit_instance: u64,
}

impl Default for PrometheusMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl PrometheusMetrics {
    /// Builds the registry and metrics.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let http_requests = Family::<Vec<(String, String)>, Pcounter>::default();
        registry.register(
            "http_requests",
            "Number of HTTP requests by method and status",
            http_requests.clone(),
        );

        let http_duration =
            Family::<Vec<(String, String)>, Phistogram, fn() -> Phistogram>::new_with_constructor(
                latency_histogram,
            );
        registry.register(
            "http_request_duration_seconds",
            "HTTP request duration in seconds by method",
            http_duration.clone(),
        );

        let auth_failures = Pcounter::default();
        registry.register(
            "auth_login_failures",
            "Number of failed login attempts",
            auth_failures.clone(),
        );

        let db_duration = Phistogram::new(LATENCY_BUCKETS.into_iter());
        registry.register(
            "db_query_duration_seconds",
            "Database query duration in seconds",
            db_duration.clone(),
        );

        let interest_visible_set_size = visible_set_size_histogram();
        registry.register(
            "interest_visible_set_size",
            "Number of entities visible to one viewer",
            interest_visible_set_size.clone(),
        );

        let tick_duration = Phistogram::new(LATENCY_BUCKETS.into_iter());
        registry.register(
            "tick_duration_seconds",
            "Instance tick processing duration in seconds",
            tick_duration.clone(),
        );
        let realtime_application_duration = Phistogram::new(LATENCY_BUCKETS.into_iter());
        registry.register(
            "realtime_application_duration_seconds",
            "Realtime application processing duration in seconds",
            realtime_application_duration.clone(),
        );

        let rate_limit_rejected = Family::<Vec<(String, String)>, Pcounter>::default();
        registry.register(
            "rate_limit_rejected",
            "Number of requests rejected by rate limiter by scope",
            rate_limit_rejected.clone(),
        );

        let mailbox_saturated = Family::<Vec<(String, String)>, Pcounter>::default();
        registry.register(
            "instance_mailbox_saturated",
            "Number of instance mailbox sends rejected because a queue was full",
            mailbox_saturated.clone(),
        );

        let mailbox_dropped = Family::<Vec<(String, String)>, Pcounter>::default();
        registry.register(
            "instance_mailbox_dropped",
            "Number of instance mailbox items dropped or coalesced",
            mailbox_dropped.clone(),
        );

        let mailbox_depth = Family::<Vec<(String, String)>, Pgauge>::default();
        registry.register(
            "instance_command_queue_depth",
            "Current depth of instance mailbox queues",
            mailbox_depth.clone(),
        );

        let extension_delivery = Family::<Vec<(String, String)>, Pcounter>::default();
        registry.register(
            "extension_delivery",
            "Number of extension webhook deliveries by result",
            extension_delivery.clone(),
        );
        let extension_delivery_worker_failures = Pcounter::default();
        registry.register(
            "extension_delivery_worker_failures",
            "Number of extension delivery worker task failures",
            extension_delivery_worker_failures.clone(),
        );

        let extension_outbox_dropped_total = Pcounter::default();
        registry.register(
            "extension_outbox_dropped",
            "Number of oldest extension outbox events dropped at the safety cap",
            extension_outbox_dropped_total.clone(),
        );
        let extension_outbox_deleted_total = Pcounter::default();
        registry.register(
            "extension_outbox_deleted",
            "Number of delivered extension outbox events deleted by retention",
            extension_outbox_deleted_total.clone(),
        );

        let websocket_connections_total = Pcounter::default();
        registry.register(
            "websocket_connections",
            "Total accepted WebSocket connections",
            websocket_connections_total.clone(),
        );
        let websocket_disconnects = Family::<Vec<(String, String)>, Pcounter>::default();
        registry.register(
            "websocket_disconnects",
            "WebSocket disconnects by bounded reason",
            websocket_disconnects.clone(),
        );
        let instance_commands_total = Pcounter::default();
        registry.register(
            "instance_commands",
            "Instance commands received",
            instance_commands_total.clone(),
        );
        let state_updates_dropped_total = Pcounter::default();
        registry.register(
            "state_updates_dropped",
            "Latest-wins state updates dropped",
            state_updates_dropped_total.clone(),
        );
        let broadcast_cell_candidates_total = Pcounter::default();
        registry.register(
            "broadcast_cell_candidates",
            "Broadcasts narrowed by the spatial cell index",
            broadcast_cell_candidates_total.clone(),
        );
        let broadcast_full_scan_total = Pcounter::default();
        registry.register(
            "broadcast_full_scan",
            "Broadcasts requiring a full connection scan",
            broadcast_full_scan_total.clone(),
        );
        let resume_attempts_total = Pcounter::default();
        registry.register(
            "resume_attempts",
            "Resume attempts",
            resume_attempts_total.clone(),
        );
        let resume_success_total = Pcounter::default();
        registry.register(
            "resume_success",
            "Successful resume attempts",
            resume_success_total.clone(),
        );
        let snapshot_bytes_total = Pcounter::default();
        registry.register(
            "snapshot_bytes",
            "Bytes in complete logical snapshots",
            snapshot_bytes_total.clone(),
        );
        let delta_bytes_total = Pcounter::default();
        registry.register(
            "delta_bytes",
            "Bytes in state delta envelopes",
            delta_bytes_total.clone(),
        );
        let entity_persistence_failures_total = Pcounter::default();
        registry.register(
            "entity_persistence_failures",
            "Durable entity/component writes drained from the actor outbox that failed",
            entity_persistence_failures_total.clone(),
        );

        let checkpoint_save_rejected_total = Pcounter::default();
        registry.register(
            "checkpoint_save_rejected",
            "Durable checkpoints refused because they exceeded the payload limit",
            checkpoint_save_rejected_total.clone(),
        );
        let checkpoint_restore_rejected_total = Pcounter::default();
        registry.register(
            "checkpoint_restore_rejected",
            "Durable checkpoint rows refused on restore because they exceeded the payload limit",
            checkpoint_restore_rejected_total.clone(),
        );
        let checkpoint_save_failures_total = Pcounter::default();
        registry.register(
            "checkpoint_save_failures",
            "Checkpoint saves that returned an application or storage error",
            checkpoint_save_failures_total.clone(),
        );
        let checkpoint_restore_failures_total = Pcounter::default();
        registry.register(
            "checkpoint_restore_failures",
            "Checkpoint restores that returned an application or storage error",
            checkpoint_restore_failures_total.clone(),
        );

        let websocket_connections_current = Pgauge::default();
        registry.register(
            "websocket_connections_current",
            "Current accepted WebSocket connections",
            websocket_connections_current.clone(),
        );
        let instance_members_current = Pgauge::default();
        registry.register(
            "instance_members_current",
            "Current members across all instances",
            instance_members_current.clone(),
        );
        let outbound_queue_depth = Pgauge::default();
        registry.register(
            "outbound_queue_depth",
            "Current outbound queue depth",
            outbound_queue_depth.clone(),
        );
        let outbound_queue_depth_max = Pgauge::default();
        registry.register(
            "outbound_queue_depth_max",
            "Process-lifetime outbound queue depth high-water mark",
            outbound_queue_depth_max.clone(),
        );
        let outbound_queue_bytes = Pgauge::default();
        registry.register(
            "outbound_queue_bytes",
            "Current outbound queue bytes",
            outbound_queue_bytes.clone(),
        );
        let outbound_queue_bytes_max = Pgauge::default();
        registry.register(
            "outbound_queue_bytes_max",
            "Process-lifetime outbound queue bytes high-water mark",
            outbound_queue_bytes_max.clone(),
        );
        let extension_outbox_pending = Pgauge::default();
        registry.register(
            "extension_outbox_pending",
            "Current number of pending extension outbox events",
            extension_outbox_pending.clone(),
        );
        let extension_delivery_in_flight = Pgauge::default();
        registry.register(
            "extension_delivery_in_flight",
            "Current number of extension delivery attempts in flight",
            extension_delivery_in_flight.clone(),
        );

        Self {
            registry: RwLock::new(registry),
            http_requests,
            http_duration,
            auth_failures,
            db_duration,
            interest_visible_set_size,
            tick_duration,
            realtime_application_duration,
            rate_limit_rejected,
            mailbox_saturated,
            mailbox_dropped,
            mailbox_depth,
            extension_delivery,
            extension_delivery_worker_failures,
            extension_outbox_dropped_total,
            extension_outbox_deleted_total,
            websocket_disconnects,
            websocket_connections_total,
            instance_commands_total,
            state_updates_dropped_total,
            broadcast_cell_candidates_total,
            broadcast_full_scan_total,
            resume_attempts_total,
            resume_success_total,
            snapshot_bytes_total,
            delta_bytes_total,
            entity_persistence_failures_total,
            checkpoint_save_rejected_total,
            checkpoint_restore_rejected_total,
            checkpoint_save_failures_total,
            checkpoint_restore_failures_total,
            websocket_connections_current,
            instance_members_current,
            outbound_queue_depth,
            outbound_queue_depth_max,
            outbound_queue_bytes,
            outbound_queue_bytes_max,
            extension_outbox_pending,
            extension_delivery_in_flight,
        }
    }

    fn http_duration_histogram_for(&self, method: HttpMethod) -> Phistogram {
        let labels = vec![("method".to_string(), method.as_str().to_string())];
        self.http_duration.get_or_create(&labels).clone()
    }

    /// Returns the bounded metric subset used by operational diagnostics.
    #[must_use]
    pub fn operational_snapshot(&self) -> OperationalMetricsSnapshot {
        let mailbox = |queue: MailboxQueue| {
            let labels = vec![("queue".to_owned(), queue.as_str().to_owned())];
            self.mailbox_depth.get_or_create(&labels).get()
        };
        let rejected = |scope: RateLimitScope| {
            let labels = vec![("scope".to_owned(), scope.as_str().to_owned())];
            self.rate_limit_rejected.get_or_create(&labels).get()
        };
        OperationalMetricsSnapshot {
            checkpoint_save_failures_total: self.checkpoint_save_failures_total.get(),
            checkpoint_restore_failures_total: self.checkpoint_restore_failures_total.get(),
            checkpoint_save_rejected_total: self.checkpoint_save_rejected_total.get(),
            checkpoint_restore_rejected_total: self.checkpoint_restore_rejected_total.get(),
            control_queue_depth: mailbox(MailboxQueue::Control),
            transform_queue_depth: mailbox(MailboxQueue::Transform),
            entity_queue_depth: mailbox(MailboxQueue::Entity),
            active_connections: self.websocket_connections_current.get(),
            outbound_queue_depth: self.outbound_queue_depth.get(),
            outbound_queue_depth_high_water: self.outbound_queue_depth_max.get(),
            outbound_queue_bytes: self.outbound_queue_bytes.get(),
            outbound_queue_bytes_high_water: self.outbound_queue_bytes_max.get(),
            extension_outbox_pending: self.extension_outbox_pending.get(),
            extension_deliveries_in_flight: self.extension_delivery_in_flight.get(),
            rate_limit_connection: rejected(RateLimitScope::Connection),
            rate_limit_user: rejected(RateLimitScope::User),
            rate_limit_ip: rejected(RateLimitScope::Ip),
            rate_limit_password_hash: rejected(RateLimitScope::PasswordHash),
            rate_limit_instance: rejected(RateLimitScope::Instance),
        }
    }
}

impl MetricsRecorder for PrometheusMetrics {
    fn incr(&self, counter: Counter) {
        self.add(counter, 1);
    }

    fn add(&self, counter: Counter, n: u64) {
        match counter {
            Counter::HttpRequests { method, status } => {
                let labels = vec![
                    ("method".to_string(), method.as_str().to_string()),
                    ("status".to_string(), status.as_str().to_string()),
                ];
                self.http_requests.get_or_create(&labels).inc_by(n);
            }
            Counter::AuthLoginFailures => {
                self.auth_failures.inc_by(n);
            }
            Counter::RateLimitRejected { scope } => {
                let labels = vec![("scope".to_string(), scope.as_str().to_string())];
                self.rate_limit_rejected.get_or_create(&labels).inc_by(n);
            }
            Counter::InstanceMailboxSaturated { queue } => {
                let labels = vec![("queue".to_string(), queue.as_str().to_string())];
                self.mailbox_saturated.get_or_create(&labels).inc_by(n);
            }
            Counter::InstanceMailboxDropped { queue } => {
                let labels = vec![("queue".to_string(), queue.as_str().to_string())];
                self.mailbox_dropped.get_or_create(&labels).inc_by(n);
            }
            Counter::ExtensionDelivery { result } => {
                let labels = vec![("result".to_owned(), result.as_str().to_owned())];
                self.extension_delivery.get_or_create(&labels).inc_by(n);
            }
            Counter::ExtensionDeliveryWorkerFailure => {
                self.extension_delivery_worker_failures.inc_by(n);
            }
            Counter::ExtensionOutboxDroppedTotal => {
                self.extension_outbox_dropped_total.inc_by(n);
            }
            Counter::ExtensionOutboxDeletedTotal => {
                self.extension_outbox_deleted_total.inc_by(n);
            }
            Counter::WebsocketConnectionsTotal => {
                self.websocket_connections_total.inc_by(n);
            }
            Counter::WebsocketDisconnects { reason } => {
                let labels = vec![("reason".to_owned(), reason.as_str().to_owned())];
                self.websocket_disconnects.get_or_create(&labels).inc_by(n);
            }
            Counter::InstanceCommandsTotal => {
                self.instance_commands_total.inc_by(n);
            }
            Counter::StateUpdatesDroppedTotal => {
                self.state_updates_dropped_total.inc_by(n);
            }
            Counter::BroadcastCellCandidatesTotal => {
                self.broadcast_cell_candidates_total.inc_by(n);
            }
            Counter::BroadcastFullScanTotal => {
                self.broadcast_full_scan_total.inc_by(n);
            }
            Counter::ResumeAttemptsTotal => {
                self.resume_attempts_total.inc_by(n);
            }
            Counter::ResumeSuccessTotal => {
                self.resume_success_total.inc_by(n);
            }
            Counter::SnapshotBytesTotal => {
                self.snapshot_bytes_total.inc_by(n);
            }
            Counter::DeltaBytesTotal => {
                self.delta_bytes_total.inc_by(n);
            }
            Counter::EntityPersistenceFailuresTotal => {
                self.entity_persistence_failures_total.inc_by(n);
            }
            Counter::CheckpointSaveRejectedTotal => {
                self.checkpoint_save_rejected_total.inc_by(n);
            }
            Counter::CheckpointRestoreRejectedTotal => {
                self.checkpoint_restore_rejected_total.inc_by(n);
            }
            Counter::CheckpointSaveFailuresTotal => {
                self.checkpoint_save_failures_total.inc_by(n);
            }
            Counter::CheckpointRestoreFailuresTotal => {
                self.checkpoint_restore_failures_total.inc_by(n);
            }
        }
    }

    fn set(&self, gauge: Gauge, value: i64) {
        match gauge {
            Gauge::InstanceCommandQueueDepth { queue } => {
                let labels = vec![("queue".to_string(), queue.as_str().to_string())];
                self.mailbox_depth.get_or_create(&labels).set(value);
            }
            Gauge::WebsocketConnectionsCurrent => {
                self.websocket_connections_current.set(value);
            }
            Gauge::InstanceMembersCurrent => {
                self.instance_members_current.set(value);
            }
            Gauge::OutboundQueueDepth => {
                self.outbound_queue_depth.set(value);
            }
            Gauge::OutboundQueueDepthMax => {
                let current = self.outbound_queue_depth_max.get();
                if value > current {
                    self.outbound_queue_depth_max.set(value);
                }
            }
            Gauge::OutboundQueueBytes => {
                self.outbound_queue_bytes.set(value);
            }
            Gauge::OutboundQueueBytesMax => {
                let current = self.outbound_queue_bytes_max.get();
                if value > current {
                    self.outbound_queue_bytes_max.set(value);
                }
            }
            Gauge::ExtensionOutboxPending => {
                self.extension_outbox_pending.set(value);
            }
            Gauge::ExtensionDeliveryInFlight => {
                self.extension_delivery_in_flight.set(value);
            }
        }
    }

    fn observe(&self, histogram: Histogram, value: f64) {
        match histogram {
            Histogram::HttpRequestDuration { method } => {
                let h = self.http_duration_histogram_for(method);
                h.observe(value);
            }
            Histogram::DbQueryDuration => {
                self.db_duration.observe(value);
            }
            Histogram::InterestVisibleSetSize => {
                self.interest_visible_set_size.observe(value);
            }
            Histogram::TickDuration => {
                self.tick_duration.observe(value);
            }
            Histogram::RealtimeApplicationDuration => {
                self.realtime_application_duration.observe(value);
            }
        }
    }
}

impl MetricsExporter for PrometheusMetrics {
    fn render(&self) -> String {
        let mut buf = String::new();
        {
            let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
            let res = encode_registry(&mut buf, &reg);
            if let Err(err) = res {
                tracing::warn!(event = "metrics.encode_failed", error = %err);
            }
        }
        let cpu = process_cpu_seconds();
        let rss = process_resident_memory_bytes();
        buf.push_str(
            "# HELP process_cpu_seconds_total Total user and system CPU time spent in seconds.\n",
        );
        buf.push_str("# TYPE process_cpu_seconds_total counter\n");
        buf.push_str(&format!("process_cpu_seconds_total {cpu}\n"));
        buf.push_str("# HELP process_resident_memory_bytes Resident memory size in bytes.\n");
        buf.push_str("# TYPE process_resident_memory_bytes gauge\n");
        if let Some(rss) = rss {
            buf.push_str(&format!("process_resident_memory_bytes {rss}\n"));
        }
        let res = encode_eof(&mut buf);
        if let Err(err) = res {
            tracing::warn!(event = "metrics.encode_eof_failed", error = %err);
        }
        buf
    }
}

/// No-op metrics that discards all observations and renders process metrics only.
#[derive(Debug, Default)]
pub struct NoopExporter;

impl MetricsRecorder for NoopExporter {
    fn incr(&self, _counter: Counter) {}
    fn add(&self, _counter: Counter, _n: u64) {}
    fn set(&self, _gauge: Gauge, _value: i64) {}
    fn observe(&self, _histogram: Histogram, _value: f64) {}
}

impl MetricsExporter for NoopExporter {
    fn render(&self) -> String {
        let mut buf = String::new();
        let cpu = process_cpu_seconds();
        let rss = process_resident_memory_bytes();
        buf.push_str(
            "# HELP process_cpu_seconds_total Total user and system CPU time spent in seconds.\n",
        );
        buf.push_str("# TYPE process_cpu_seconds_total counter\n");
        buf.push_str(&format!("process_cpu_seconds_total {cpu}\n"));
        buf.push_str("# HELP process_resident_memory_bytes Resident memory size in bytes.\n");
        buf.push_str("# TYPE process_resident_memory_bytes gauge\n");
        if let Some(rss) = rss {
            buf.push_str(&format!("process_resident_memory_bytes {rss}\n"));
        }
        buf.push_str("# EOF\n");
        buf
    }
}

#[cfg(windows)]
fn process_cpu_seconds() -> f64 {
    0.0
}

#[cfg(not(windows))]
fn process_cpu_seconds() -> f64 {
    let Ok(content) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(pos) = content.rfind(')') else {
        return 0.0;
    };
    let after = &content[pos + 1..];
    let parts: Vec<&str> = after.split_whitespace().collect();
    if parts.len() < 13 {
        return 0.0;
    }
    let utime: f64 = parts[11].parse::<f64>().unwrap_or(0.0);
    let stime: f64 = parts[12].parse::<f64>().unwrap_or(0.0);
    let ticks = utime + stime;
    // clk_tck is 100 on Linux: USER_HZ is fixed to 100 as part of the glibc user-space ABI
    // (see `sysconf(_SC_CLK_TCK)` always returns 100 on Linux), so hardcoding 100 is correct.
    let clk_tck = 100.0;
    ticks / clk_tck
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn process_resident_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
    };
    // The Windows API is the supported source for WorkingSetSize.  A failed
    // query is deliberately None: exporting zero would turn a missing
    // measurement into a false observation.
    let result = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (result != 0).then_some(counters.WorkingSetSize as u64)
}

#[cfg(not(windows))]
fn process_resident_memory_bytes() -> Option<u64> {
    // Use VmRSS from /proc/self/status (kB) to avoid page_size dependence
    // (arm64 Linux may use 16 KiB pages, statm + 4096 would be 1/4 of real RSS).
    let Ok(content) = std::fs::read_to_string("/proc/self/status") else {
        return None;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:\t  12345 kB"
            let mut parts = rest.split_whitespace();
            if let Some(value_str) = parts.next()
                && let Ok(kb) = value_str.parse::<u64>()
            {
                return Some(kb * 1024);
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::PrometheusMetrics;
    use orbisync_application::metrics::{
        Counter, ExtensionDeliveryResult, Gauge, Histogram, HttpMethod, HttpStatusClass,
        MailboxQueue, MetricsExporter, MetricsRecorder, RateLimitScope, WebsocketDisconnectReason,
    };

    #[test]
    fn operational_snapshot_reads_typed_counters_and_gauges() {
        let metrics = PrometheusMetrics::new();
        metrics.add(Counter::CheckpointSaveFailuresTotal, 2);
        metrics.add(Counter::CheckpointRestoreFailuresTotal, 3);
        metrics.incr(Counter::CheckpointSaveRejectedTotal);
        metrics.incr(Counter::CheckpointRestoreRejectedTotal);
        metrics.add(
            Counter::RateLimitRejected {
                scope: RateLimitScope::Ip,
            },
            4,
        );
        metrics.set(
            Gauge::InstanceCommandQueueDepth {
                queue: MailboxQueue::Entity,
            },
            5,
        );
        metrics.set(Gauge::WebsocketConnectionsCurrent, 6);
        metrics.set(Gauge::OutboundQueueDepth, 7);
        metrics.set(Gauge::OutboundQueueDepthMax, 8);
        metrics.set(Gauge::ExtensionOutboxPending, 9);

        let snapshot = metrics.operational_snapshot();
        assert_eq!(snapshot.checkpoint_save_failures_total, 2);
        assert_eq!(snapshot.checkpoint_restore_failures_total, 3);
        assert_eq!(snapshot.checkpoint_save_rejected_total, 1);
        assert_eq!(snapshot.checkpoint_restore_rejected_total, 1);
        assert_eq!(snapshot.rate_limit_ip, 4);
        assert_eq!(snapshot.entity_queue_depth, 5);
        assert_eq!(snapshot.active_connections, 6);
        assert_eq!(snapshot.outbound_queue_depth, 7);
        assert_eq!(snapshot.outbound_queue_depth_high_water, 8);
        assert_eq!(snapshot.extension_outbox_pending, 9);
    }

    #[test]
    fn render_contains_all_d1a_metrics_and_process() {
        let m = PrometheusMetrics::new();
        // Ensure families have at least one sample so the metric name appears in exposition
        m.incr(Counter::HttpRequests {
            method: HttpMethod::Get,
            status: HttpStatusClass::Success,
        });
        m.observe(
            Histogram::HttpRequestDuration {
                method: HttpMethod::Get,
            },
            0.05,
        );
        m.incr(Counter::AuthLoginFailures);
        m.observe(Histogram::DbQueryDuration, 0.01);
        m.observe(Histogram::InterestVisibleSetSize, 3.0);
        m.incr(Counter::RateLimitRejected {
            scope: RateLimitScope::User,
        });
        m.incr(Counter::RateLimitRejected {
            scope: RateLimitScope::PasswordHash,
        });
        m.incr(Counter::ExtensionDelivery {
            result: ExtensionDeliveryResult::Success,
        });
        m.incr(Counter::ExtensionOutboxDroppedTotal);
        m.incr(Counter::ExtensionOutboxDeletedTotal);
        m.set(Gauge::ExtensionOutboxPending, 3);
        let text = m.render();
        assert!(
            text.contains("http_requests_total"),
            "missing http_requests_total"
        );
        assert!(
            text.contains("http_request_duration_seconds"),
            "missing http_request_duration_seconds"
        );
        assert!(
            text.contains("auth_login_failures_total"),
            "missing auth_login_failures_total"
        );
        assert!(
            text.contains("db_query_duration_seconds"),
            "missing db_query_duration_seconds"
        );
        assert!(
            text.contains("interest_visible_set_size"),
            "missing interest_visible_set_size"
        );
        assert!(
            text.contains("rate_limit_rejected_total"),
            "missing rate_limit_rejected_total"
        );
        assert!(
            text.contains("rate_limit_rejected_total{scope=\"password_hash\"}"),
            "missing password hash rate-limit scope"
        );
        assert!(
            text.contains("extension_delivery_total"),
            "missing extension_delivery_total"
        );
        assert!(
            text.contains("extension_outbox_dropped_total"),
            "missing extension_outbox_dropped_total"
        );
        assert!(
            text.contains("extension_outbox_deleted_total"),
            "missing extension_outbox_deleted_total"
        );
        assert!(
            text.contains("extension_outbox_pending 3"),
            "missing extension_outbox_pending"
        );
        assert!(
            text.contains("process_cpu_seconds_total"),
            "missing process_cpu_seconds_total"
        );
        assert!(
            text.contains("process_resident_memory_bytes"),
            "missing process_resident_memory_bytes"
        );
        assert!(text.contains("# HELP"), "missing HELP");
        assert!(text.contains("# TYPE"), "missing TYPE");
        assert!(text.contains("# EOF"), "missing EOF");
    }

    #[test]
    fn histogram_buckets_present_with_default_buckets() {
        let m = PrometheusMetrics::new();
        m.observe(
            Histogram::HttpRequestDuration {
                method: HttpMethod::Get,
            },
            0.01,
        );
        let text = m.render();
        assert!(
            text.contains("http_request_duration_seconds_bucket"),
            "missing bucket"
        );
        // Check a few bucket boundaries – encoding may be "10" or "10.0"
        assert!(text.contains("le=\"0.005\""), "missing bucket 0.005");
        assert!(text.contains("le=\"0.01\""), "missing bucket 0.01");
        assert!(
            text.contains("le=\"10\"") || text.contains("le=\"10.0\""),
            "missing bucket 10"
        );
        assert!(text.contains("_sum"), "missing _sum");
        assert!(text.contains("_count"), "missing _count");
    }

    #[test]
    fn counter_increments_are_visible() {
        let m = PrometheusMetrics::new();
        m.incr(Counter::AuthLoginFailures);
        m.add(
            Counter::RateLimitRejected {
                scope: RateLimitScope::User,
            },
            2,
        );
        m.incr(Counter::HttpRequests {
            method: HttpMethod::Post,
            status: HttpStatusClass::Success,
        });
        let text = m.render();
        assert!(
            text.contains("auth_login_failures_total 1"),
            "auth increment"
        );
        assert!(
            text.contains("rate_limit_rejected_total{scope=\"user\"} 2"),
            "rate limit increment"
        );
        assert!(
            text.contains("http_requests_total{method=\"POST\",status=\"2xx\"} 1")
                || text.contains("http_requests_total{status=\"2xx\",method=\"POST\"} 1"),
            "http_requests increment: {text}"
        );
    }

    #[test]
    fn db_histogram_observe_works() {
        let m = PrometheusMetrics::new();
        m.observe(Histogram::DbQueryDuration, 0.05);
        let text = m.render();
        assert!(text.contains("db_query_duration_seconds_bucket"));
    }

    #[test]
    fn visible_set_size_histogram_uses_entity_count_buckets() {
        let m = PrometheusMetrics::new();
        m.observe(Histogram::InterestVisibleSetSize, 50.0);
        let text = m.render();
        assert!(text.contains("interest_visible_set_size_bucket"));
        assert!(text.contains("interest_visible_set_size_bucket{le=\"50.0\"} 1"));
        assert!(text.contains("interest_visible_set_size_sum 50.0"));
        assert!(text.contains("interest_visible_set_size_count 1"));
    }

    #[test]
    fn d1b_metrics_render_without_high_cardinality_labels() {
        let m = PrometheusMetrics::new();
        m.incr(Counter::WebsocketConnectionsTotal);
        m.incr(Counter::WebsocketDisconnects {
            reason: WebsocketDisconnectReason::SlowConsumer,
        });
        m.add(Counter::InstanceCommandsTotal, 2);
        m.incr(Counter::StateUpdatesDroppedTotal);
        m.incr(Counter::BroadcastCellCandidatesTotal);
        m.incr(Counter::BroadcastFullScanTotal);
        m.incr(Counter::ResumeAttemptsTotal);
        m.incr(Counter::ResumeSuccessTotal);
        m.add(Counter::SnapshotBytesTotal, 123);
        m.add(Counter::DeltaBytesTotal, 45);
        m.incr(Counter::CheckpointSaveRejectedTotal);
        m.incr(Counter::CheckpointRestoreRejectedTotal);
        m.set(Gauge::WebsocketConnectionsCurrent, 1);
        m.set(Gauge::InstanceMembersCurrent, 3);
        m.set(Gauge::OutboundQueueDepth, 2);
        m.set(Gauge::OutboundQueueDepthMax, 2);
        m.set(Gauge::OutboundQueueBytes, 64);
        m.set(Gauge::OutboundQueueBytesMax, 64);
        let text = m.render();
        for name in [
            "websocket_connections_total",
            "websocket_disconnects_total",
            "instance_members_current",
            "instance_commands_total",
            "state_updates_dropped_total",
            "broadcast_cell_candidates_total",
            "broadcast_full_scan_total",
            "resume_attempts_total",
            "resume_success_total",
            "snapshot_bytes_total",
            "delta_bytes_total",
            "checkpoint_save_rejected_total",
            "checkpoint_restore_rejected_total",
            "outbound_queue_depth",
            "outbound_queue_depth_max",
            "outbound_queue_bytes",
            "outbound_queue_bytes_max",
            "extension_outbox_pending",
        ] {
            assert!(text.contains(name), "missing {name}");
        }
        assert!(text.contains("reason=\"slow_consumer\""));
        for forbidden in ["instance_id=", "user_id=", "connection_id=", "entity_id="] {
            assert!(
                !text.contains(forbidden),
                "high-cardinality label {forbidden}"
            );
        }
    }

    #[test]
    fn queue_high_water_marks_are_monotonic() {
        let m = PrometheusMetrics::new();
        m.set(Gauge::OutboundQueueDepthMax, 4);
        m.set(Gauge::OutboundQueueDepthMax, 1);
        m.set(Gauge::OutboundQueueBytesMax, 128);
        m.set(Gauge::OutboundQueueBytesMax, 8);
        let text = m.render();
        assert!(text.contains("outbound_queue_depth_max 4"));
        assert!(text.contains("outbound_queue_bytes_max 128"));
    }
}
