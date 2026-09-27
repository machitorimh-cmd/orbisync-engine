//! Concurrency regression test for per-instance runtime task ownership.

use std::sync::Barrier;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use orbisync_application::metrics::{Counter, Gauge, Histogram, MetricsRecorder};
use orbisync_domain::{InstanceId, PresenceId, Revision, UserId};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, MailboxConfig, RuntimeRegistry, RuntimeState,
    actor::InstanceActor,
    command::{CommandOutcome, InstanceCommand},
};

struct BarrierMetrics {
    enabled: Arc<AtomicBool>,
    barrier: Arc<Barrier>,
}

impl MetricsRecorder for BarrierMetrics {
    fn incr(&self, _counter: Counter) {}

    fn add(&self, _counter: Counter, _n: u64) {}

    fn set(&self, _gauge: Gauge, _value: i64) {
        if self.enabled.load(Ordering::Acquire) {
            self.barrier.wait();
        }
    }

    fn observe(&self, _histogram: Histogram, _value: f64) {}
}

fn actor(id: InstanceId, metrics: Arc<dyn MetricsRecorder>) -> InstanceActor {
    let mut actor = InstanceActor::with_mailbox_config(
        InstanceRuntimeDescriptor {
            instance_id: id,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        },
        30.0,
        256,
        100.0,
        100.0,
        MailboxConfig::default(),
        metrics,
    );
    actor.start();
    actor
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_tasks_process_independently() {
    let enabled = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(BarrierMetrics {
        enabled: Arc::clone(&enabled),
        barrier: Arc::new(Barrier::new(2)),
    });
    let registry = RuntimeRegistry::new();
    let first_id = InstanceId::generate();
    let second_id = InstanceId::generate();
    let first = registry.ensure_instance(actor(
        first_id,
        Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
    ));
    let second = registry.ensure_instance(actor(
        second_id,
        Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
    ));
    assert_ne!(
        first.instance_id(),
        second.instance_id(),
        "each instance must have its own task handle"
    );

    enabled.store(true, Ordering::Release);
    let first_join = first.submit(InstanceCommand::Join {
        presence_id: PresenceId::generate(),
        user_id: UserId::generate(),
        instance_id: first_id,
        capacity: 10,
    });
    let second_join = second.submit(InstanceCommand::Join {
        presence_id: PresenceId::generate(),
        user_id: UserId::generate(),
        instance_id: second_id,
        capacity: 10,
    });
    let joined = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        tokio::join!(first_join, second_join)
    })
    .await;
    assert!(
        joined.is_ok(),
        "instance tasks must make progress independently"
    );
    let (first_result, second_result) = joined.expect("timeout");
    assert!(matches!(first_result, Ok(CommandOutcome::Applied { .. })));
    assert!(matches!(second_result, Ok(CommandOutcome::Applied { .. })));
}
