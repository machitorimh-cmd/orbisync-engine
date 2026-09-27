//! Separate simulation cadence from bounded, ordered persistence work.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use orbisync_application::metrics::{Counter, Histogram, MetricsRecorder};
use orbisync_application::{CheckpointStore, ExtensionOutboxStore, PersistentEntityStore};
use orbisync_domain::{Clock as _, InstanceId, SystemClock, Timestamp};
use orbisync_server::{command_dedup::CommandDedupStore, delivery::DeliveryRegistry};
use orbisync_world_runtime::{RuntimeRegistry, registry::InstanceEffects};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

const MAX_CONCURRENT_MAINTENANCE: usize = 8;

pub(crate) struct MaintenanceServices {
    pub registry: Arc<RuntimeRegistry>,
    pub delivery: Arc<DeliveryRegistry>,
    pub checkpoints: Arc<dyn CheckpointStore>,
    pub extensions: Arc<dyn ExtensionOutboxStore>,
    pub entities: Arc<dyn PersistentEntityStore>,
    pub dedup: Arc<CommandDedupStore>,
    pub metrics: Arc<dyn MetricsRecorder>,
    pub checkpoint_permits: Arc<Semaphore>,
}

#[derive(Clone, Copy)]
struct TickProgress {
    count: u64,
    now: Timestamp,
}

/// Owns both tasks so shutdown can stop ticks and drain accepted effects before
/// the final checkpoints and database pool closure.
pub(crate) struct RuntimeWorkers {
    stop: watch::Sender<bool>,
    ticks: JoinHandle<()>,
    maintenance: JoinHandle<()>,
}

impl Drop for RuntimeWorkers {
    fn drop(&mut self) {
        self.ticks.abort();
        self.maintenance.abort();
    }
}

impl RuntimeWorkers {
    pub(crate) fn spawn(
        services: MaintenanceServices,
        tick_hz: u32,
        checkpoint_interval: Duration,
        checkpoint_interval_ticks: u64,
    ) -> Self {
        let services = Arc::new(services);
        let (stop, mut tick_stop) = watch::channel(false);
        let mut maintenance_stop = stop.subscribe();
        // Only the latest wakeup is retained, never a growing queue of effects.
        // Effects stay in bounded actor outboxes until the worker can own them.
        let (progress_tx, mut progress_rx) = watch::channel(TickProgress {
            count: 0,
            now: SystemClock::new().now(),
        });
        let tick_services = Arc::clone(&services);
        let ticks = tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(tick_hz.max(1))));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut count = 0_u64;
            loop {
                tokio::select! {
                    biased;
                    _ = tick_stop.changed() => break,
                    _ = ticker.tick() => {}
                }
                let now = SystemClock::new().now();
                let handles = tick_services.registry.handles();
                let mut ticks = orbisync_server::runtime_tick::advance_instances(&handles, now);
                while let Some(completion) = ticks.next().await {
                    tick_services
                        .metrics
                        .observe(Histogram::TickDuration, completion.duration.as_secs_f64());
                    if completion.result.is_none() {
                        tracing::debug!(event = "runtime.tick_mailbox_unavailable", instance_id = %completion.instance_id);
                    }
                }
                count = count.saturating_add(1);
                progress_tx.send_replace(TickProgress { count, now });
            }
        });
        let maintenance = tokio::spawn(async move {
            let mut last_checkpoint = Instant::now();
            let mut last_checkpoint_tick = 0_u64;
            loop {
                tokio::select! {
                    biased;
                    _ = maintenance_stop.changed() => break,
                    changed = progress_rx.changed() => {
                        if changed.is_err() { break; }
                    }
                }
                let progress = *progress_rx.borrow_and_update();
                // A busy worker may skip wakeups, so test elapsed ticks rather
                // than requiring it to observe an exact multiple.
                let checkpoint_due = progress.count.saturating_sub(last_checkpoint_tick)
                    >= checkpoint_interval_ticks.max(1)
                    || last_checkpoint.elapsed() >= checkpoint_interval;
                if checkpoint_due {
                    last_checkpoint_tick = progress.count;
                    last_checkpoint = Instant::now();
                }
                services.run_pass(progress.now, checkpoint_due, false).await;
            }
            // Admission is closed and actors have been marked draining before
            // shutdown requests this final pass. No overlapping worker can
            // drain or reorder the effects collected here.
            services
                .run_pass(SystemClock::new().now(), false, true)
                .await;
        });
        Self {
            stop,
            ticks,
            maintenance,
        }
    }

    pub(crate) async fn drain(mut self, timeout: Duration) {
        self.stop.send_replace(true);
        let deadline = tokio::time::Instant::now() + timeout;
        match tokio::time::timeout_at(deadline, &mut self.ticks).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::error!(event = "runtime.tick_shutdown_failed", error = %error)
            }
            Err(_) => {
                tracing::error!(event = "runtime.tick_shutdown_deadline_exceeded");
                self.ticks.abort();
                let _cancelled = (&mut self.ticks).await;
            }
        }
        match tokio::time::timeout_at(deadline, &mut self.maintenance).await {
            Ok(Ok(())) => tracing::info!(event = "runtime.persistence_drained"),
            Ok(Err(error)) => {
                tracing::error!(event = "runtime.persistence_worker_failed", error = %error)
            }
            Err(error) => {
                tracing::error!(event = "runtime.persistence_drain_failed", error = %error,
                    "accepted effects may remain unsaved; final checkpoints will still be attempted");
                self.maintenance.abort();
                // Ensure no storage task can race pool closure after cancellation.
                let _cancelled = (&mut self.maintenance).await;
            }
        }
    }
}

impl MaintenanceServices {
    async fn persist_effects(&self, effects: InstanceEffects) {
        super::persist_entity_events(
            self.entities.as_ref(),
            self.metrics.as_ref(),
            effects.persistence_events,
        )
        .await;
        super::persist_extension_events(self.extensions.as_ref(), effects.events).await;
    }

    async fn run_pass(&self, now: Timestamp, checkpoint_due: bool, final_pass: bool) {
        let handles = self.registry.handles();
        let mut work = stream::iter(handles)
            .map(|handle| async move {
                let instance_id = handle.instance_id();
                if let Some(effects) = handle.drain_effects().await {
                    self.persist_effects(effects).await;
                }
                if final_pass {
                    return;
                }
                // The same worker owns drains and reaps. A later drain cannot pass
                // an earlier batch, including effects emitted between drain/reap.
                if handle.cached_member_count() == 0 && self.delivery.sender_count(instance_id) == 0
                {
                    let (events, persistence_events) = super::reap_idle_instance(
                        Arc::clone(&self.registry),
                        Arc::clone(&self.delivery),
                        Arc::clone(&self.checkpoints),
                        Arc::clone(&self.dedup),
                        Arc::clone(&self.metrics),
                        Arc::clone(&self.checkpoint_permits),
                        instance_id,
                        now,
                    )
                    .await;
                    self.persist_effects(InstanceEffects {
                        events,
                        persistence_events,
                    })
                    .await;
                } else if checkpoint_due {
                    self.save_checkpoint(instance_id, now).await;
                }
            })
            .buffer_unordered(MAX_CONCURRENT_MAINTENANCE);
        while work.next().await.is_some() {}
    }

    async fn save_checkpoint(&self, instance_id: InstanceId, now: Timestamp) {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            super::persist_instance_checkpoint(
                self.registry.as_ref(),
                self.checkpoints.as_ref(),
                self.dedup.as_ref(),
                self.checkpoint_permits.as_ref(),
                instance_id,
                now,
            ),
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error))
                if error.kind()
                    == orbisync_application::ApplicationErrorKind::CheckpointTooLarge =>
            {
                self.metrics.incr(Counter::CheckpointSaveRejectedTotal);
                tracing::error!(event = "checkpoint.save_rejected_too_large", instance_id = %instance_id,
                    limit_bytes = orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES, error = %error,
                    "periodic checkpoint exceeds the payload limit");
            }
            Ok(Err(error)) => {
                tracing::warn!(event = "checkpoint.save_failed", instance_id = %instance_id, error = %error)
            }
            Err(_) => {
                tracing::warn!(event = "checkpoint.save_deadline_exceeded", instance_id = %instance_id)
            }
        }
    }
}
