//! Bounded tick dispatch to independently owned instance tasks.

use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt, stream};
use orbisync_domain::{InstanceId, Timestamp};
use orbisync_world_runtime::registry::{InstanceHandle, InstanceTickResult};

/// Maximum outstanding tick requests in one coordinator pass.
///
/// Actor tasks already execute on Tokio workers. This bounds the futures used
/// to await their replies without spawning another task for every tick.
pub const MAX_CONCURRENT_INSTANCE_TICKS: usize = 8;

/// Completion of one independently dispatched tick.
#[derive(Debug)]
pub struct InstanceTickCompletion {
    /// Instance whose actor processed the request.
    pub instance_id: InstanceId,
    /// Absent when the actor task has stopped.
    pub result: Option<InstanceTickResult>,
    /// Time from request submission to reply, including mailbox wait.
    pub duration: Duration,
}

/// Ticks independent instances with bounded concurrency, yielding completion order.
///
/// Each actor still processes its commands serially. The caller must finish
/// draining this stream before starting the next tick pass. Set `checkpoint_due`
/// only when the returned checkpoint will be consumed; periodic durable saves
/// capture their own checkpoint under the durability guard.
pub fn tick_instances(
    handles: &[InstanceHandle],
    now: Timestamp,
    checkpoint_due: bool,
) -> impl Stream<Item = InstanceTickCompletion> + Unpin + '_ {
    dispatch_ticks(handles, now, checkpoint_due, true)
}

/// Advances independent actors while leaving their effects for the storage worker.
pub fn advance_instances(
    handles: &[InstanceHandle],
    now: Timestamp,
) -> impl Stream<Item = InstanceTickCompletion> + Unpin + '_ {
    dispatch_ticks(handles, now, false, false)
}

fn dispatch_ticks(
    handles: &[InstanceHandle],
    now: Timestamp,
    checkpoint_due: bool,
    drain_effects: bool,
) -> impl Stream<Item = InstanceTickCompletion> + Unpin + '_ {
    stream::iter(handles)
        .map(move |handle| async move {
            let started = Instant::now();
            let result = if drain_effects {
                handle.tick(now, checkpoint_due).await
            } else {
                handle.advance_tick(now).await
            };
            InstanceTickCompletion {
                instance_id: handle.instance_id(),
                result,
                duration: started.elapsed(),
            }
        })
        .buffer_unordered(MAX_CONCURRENT_INSTANCE_TICKS)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use orbisync_application::metrics::{Counter, Gauge, Histogram, MetricsRecorder, NoopMetrics};
    use orbisync_domain::{EntityId, EntityKind, Revision, UserId, VisibilityPolicy};
    use orbisync_world_runtime::actor::InstanceActor;
    use orbisync_world_runtime::command::{InstanceCommand, WorldPermissions};
    use orbisync_world_runtime::{
        InstanceRuntimeDescriptor, MailboxConfig, RuntimeRegistry, RuntimeState,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    fn actor(metrics: Arc<dyn MetricsRecorder>) -> InstanceActor {
        InstanceActor::with_mailbox_config(
            InstanceRuntimeDescriptor {
                instance_id: InstanceId::generate(),
                state: RuntimeState::Running,
                revision: Revision::INITIAL,
            },
            30.0,
            256,
            100.0,
            100.0,
            MailboxConfig::default(),
            metrics,
        )
    }

    struct BlockingMetrics {
        enabled: AtomicBool,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl MetricsRecorder for BlockingMetrics {
        fn incr(&self, _: Counter) {}
        fn add(&self, _: Counter, _: u64) {}
        fn observe(&self, _: Histogram, _: f64) {}
        fn set(&self, _: Gauge, _: i64) {
            if self.enabled.swap(false, Ordering::AcqRel) {
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release actor");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_actor_does_not_prevent_another_tick_from_completing() {
        let (release_tx, release_rx) = mpsc::channel();
        let metrics = Arc::new(BlockingMetrics {
            enabled: AtomicBool::new(false),
            release: Mutex::new(release_rx),
        });
        let registry = RuntimeRegistry::new();
        let slow = registry.ensure_instance(actor(metrics.clone()));
        let healthy = registry.ensure_instance(actor(Arc::new(NoopMetrics)));
        let healthy_id = healthy.instance_id();
        let handles = [slow, healthy];
        metrics.enabled.store(true, Ordering::Release);
        let mut ticks = tick_instances(&handles, Timestamp::from_unix_millis(1).unwrap(), false);
        let first = tokio::time::timeout(Duration::from_secs(1), ticks.next()).await;
        release_tx.send(()).expect("unblock before assertions");
        let first = first
            .expect("healthy tick must complete without waiting for the blocked actor")
            .expect("completion");
        assert_eq!(first.instance_id, healthy_id);
        assert!(first.result.is_some());
        assert!(
            ticks
                .next()
                .await
                .expect("slow completion")
                .result
                .is_some()
        );
        assert!(ticks.next().await.is_none());
    }

    #[tokio::test]
    async fn empty_and_stopped_instances_do_not_lose_healthy_results() {
        let now = Timestamp::from_unix_millis(1).unwrap();
        assert!(tick_instances(&[], now, false).next().await.is_none());
        let registry = RuntimeRegistry::new();
        let stopped = registry.ensure_instance(actor(Arc::new(NoopMetrics)));
        assert!(stopped.reap_if_idle(now).await.is_some());
        let healthy = registry.ensure_instance(actor(Arc::new(NoopMetrics)));
        let healthy_id = healthy.instance_id();
        let handles = [stopped, healthy];
        let completed = tick_instances(&handles, now, false)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(completed.len(), 2);
        assert!(
            completed
                .iter()
                .find(|entry| entry.instance_id == healthy_id)
                .unwrap()
                .result
                .is_some()
        );
        assert_eq!(
            completed
                .iter()
                .filter(|entry| entry.result.is_none())
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual bounded performance measurement"]
    async fn engine_tick_performance_probe() {
        const WORLDS: usize = 4;
        const ENTITIES: usize = 256;
        const PASSES: usize = 16;
        let registry = RuntimeRegistry::new();
        let now = Timestamp::from_unix_millis(1).unwrap();
        let mut handles = Vec::new();
        for _ in 0..WORLDS {
            let mut actor = actor(Arc::new(NoopMetrics));
            let requester = UserId::generate();
            for _ in 0..ENTITIES {
                let outcome = actor.handle(InstanceCommand::SpawnEntity {
                    command_id: None,
                    entity_id: EntityId::generate(),
                    kind: EntityKind::Object,
                    owner: Some(requester),
                    transform: None,
                    visibility: VisibilityPolicy::Global,
                    requester,
                    permissions: WorldPermissions::all(),
                });
                assert!(matches!(
                    outcome,
                    orbisync_world_runtime::command::CommandOutcome::Applied { .. }
                ));
            }
            handles.push(registry.ensure_instance(actor));
        }
        for checkpoint_due in [false, true] {
            for concurrent in [false, true] {
                let mut samples = Vec::new();
                for sample in 0..6 {
                    let started = Instant::now();
                    for _ in 0..PASSES {
                        if concurrent {
                            let mut ticks = tick_instances(&handles, now, checkpoint_due);
                            while let Some(completion) = ticks.next().await {
                                assert!(completion.result.is_some());
                            }
                        } else {
                            for handle in &handles {
                                assert!(handle.tick(now, checkpoint_due).await.is_some());
                            }
                        }
                    }
                    if sample > 0 {
                        samples.push(started.elapsed().as_micros());
                    }
                }
                samples.sort_unstable();
                eprintln!(
                    "ENGINE_TICK parallel={concurrent} checkpoint={checkpoint_due} worlds={WORLDS} entities={ENTITIES} passes={PASSES} median_us={} samples_us={samples:?}",
                    samples[2]
                );
            }
        }
    }
}
