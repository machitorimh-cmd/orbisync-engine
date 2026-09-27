//! Composition adapters for privileged operational diagnostics.

use std::sync::Arc;

use orbisync_application::metrics::{Counter, MetricsRecorder};
use orbisync_application::{
    AppCheckpoint, ApplicationError, CheckpointSaveReceipt, CheckpointStore,
    OperationalDiagnostics, OperationalDiagnosticsPort, OperationalQueueLimits,
    OperationalQueueSnapshot, OperationalRateLimitSnapshot,
};
use orbisync_domain::{Clock, InstanceId, Timestamp};
use orbisync_observability::PrometheusMetrics;
use orbisync_storage_postgres::PgCheckpointStore;
use sqlx::PgPool;

use crate::retention::RetentionStatus;

/// Adds error counters around the durable checkpoint adapter without leaking
/// metrics concerns into the PostgreSQL implementation.
pub struct MeteredCheckpointStore {
    inner: PgCheckpointStore,
    metrics: Arc<dyn MetricsRecorder>,
}

impl MeteredCheckpointStore {
    /// Wraps the PostgreSQL checkpoint store.
    #[must_use]
    pub fn new(inner: PgCheckpointStore, metrics: Arc<dyn MetricsRecorder>) -> Self {
        Self { inner, metrics }
    }
}

#[async_trait::async_trait]
impl CheckpointStore for MeteredCheckpointStore {
    async fn save_checkpoint(
        &self,
        checkpoint: AppCheckpoint,
    ) -> Result<Vec<CheckpointSaveReceipt>, ApplicationError> {
        let result = self.inner.save_checkpoint(checkpoint).await;
        if result.is_err() {
            self.metrics.incr(Counter::CheckpointSaveFailuresTotal);
        }
        result
    }

    async fn load_latest(
        &self,
        instance_id: InstanceId,
    ) -> Result<Option<AppCheckpoint>, ApplicationError> {
        let result = self.inner.load_latest(instance_id).await;
        if result.is_err() {
            self.metrics.incr(Counter::CheckpointRestoreFailuresTotal);
        }
        result
    }
}

/// Read-only diagnostics aggregator owned by the composition root.
pub struct RuntimeOperationalDiagnostics {
    pool: PgPool,
    metrics: Arc<PrometheusMetrics>,
    retention: Arc<RetentionStatus>,
    clock: Arc<dyn Clock>,
    queue_limits: OperationalQueueLimits,
}

impl RuntimeOperationalDiagnostics {
    /// Creates an aggregator from already-wired runtime dependencies.
    #[must_use]
    pub fn new(
        pool: PgPool,
        metrics: Arc<PrometheusMetrics>,
        retention: Arc<RetentionStatus>,
        clock: Arc<dyn Clock>,
        queue_limits: OperationalQueueLimits,
    ) -> Self {
        Self {
            pool,
            metrics,
            retention,
            clock,
            queue_limits,
        }
    }
}

#[async_trait::async_trait]
impl OperationalDiagnosticsPort for RuntimeOperationalDiagnostics {
    async fn snapshot(&self) -> Result<OperationalDiagnostics, ApplicationError> {
        let latest_checkpoint = sqlx::query_scalar::<_, Option<time::OffsetDateTime>>(
            "SELECT max(created_at) FROM instance_checkpoints",
        )
        .fetch_one(&self.pool);
        let dead_letters =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM extension_dead_letters")
                .fetch_one(&self.pool);
        let (latest_checkpoint, dead_letters) = tokio::try_join!(latest_checkpoint, dead_letters)
            .map_err(|error| {
            tracing::warn!(
                event = "operational_diagnostics.database_failed",
                error = %error,
                "failed to collect protected operational diagnostics"
            );
            ApplicationError::port_failure("operational diagnostics database query failed")
        })?;

        let metrics = self.metrics.operational_snapshot();
        Ok(OperationalDiagnostics {
            collected_at: self.clock.now(),
            latest_checkpoint_at: latest_checkpoint.map(Timestamp::from_offset_date_time),
            checkpoint_save_failures_total: metrics.checkpoint_save_failures_total,
            checkpoint_restore_failures_total: metrics.checkpoint_restore_failures_total,
            checkpoint_save_rejected_total: metrics.checkpoint_save_rejected_total,
            checkpoint_restore_rejected_total: metrics.checkpoint_restore_rejected_total,
            queues: OperationalQueueSnapshot {
                control: metrics.control_queue_depth,
                transform: metrics.transform_queue_depth,
                entity: metrics.entity_queue_depth,
                outbound: metrics.outbound_queue_depth,
                outbound_high_water: metrics.outbound_queue_depth_high_water,
                outbound_bytes: metrics.outbound_queue_bytes,
                outbound_bytes_high_water: metrics.outbound_queue_bytes_high_water,
            },
            queue_limits: self.queue_limits,
            active_connections: metrics.active_connections,
            rate_limit_rejections: OperationalRateLimitSnapshot {
                connection: metrics.rate_limit_connection,
                user: metrics.rate_limit_user,
                ip: metrics.rate_limit_ip,
                password_hash: metrics.rate_limit_password_hash,
                instance: metrics.rate_limit_instance,
            },
            extension_outbox_pending: metrics.extension_outbox_pending,
            extension_deliveries_in_flight: metrics.extension_deliveries_in_flight,
            extension_dead_letters: u64::try_from(dead_letters).unwrap_or(u64::MAX),
            retention: self.retention.snapshot(),
        })
    }
}
