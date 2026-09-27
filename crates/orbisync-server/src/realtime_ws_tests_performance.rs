//! Small, explicitly requested performance probes; not a capacity/stress test.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use orbisync_world_runtime::actor::EntityInterestView;

#[test]
#[ignore = "manual bounded performance measurement"]
fn engine_delivery_performance_probe() {
    const WORLDS: usize = 2;
    const RECEIVERS: usize = 16;
    const ENTITIES: usize = 128;
    const BROADCASTS: usize = 64;
    let delivery = Arc::new(DeliveryRegistry::new());
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::new(RuntimeRegistry::new()),
            Arc::clone(&delivery),
            Arc::new(orbisync_domain::SystemClock::new()),
        )
        .build(),
    );
    let grid = orbisync_interest::UniformGrid::default();
    let mut registrations = Vec::new();
    let mut fixtures = Vec::new();
    for _ in 0..WORLDS {
        let instance = InstanceId::generate();
        let mut views = Vec::new();
        let mut entities = Vec::new();
        for index in 0..ENTITIES {
            let id = EntityId::generate();
            views.push(EntityInterestView {
                id,
                owner: None,
                position: Some(Vec3::new((index % 10) as f64, 0.0, 0.0).unwrap()),
                visibility: VisibilityPolicy::Global,
            });
            // A typical update changes one entity in a larger world view.
            if index == ENTITIES - 1 {
                entities.push(orbisync_protocol::v1::EntityState {
                    entity_id: id.to_string(),
                    revision: 1,
                    ..Default::default()
                });
            }
        }
        let payload = Envelope {
            payload: Some(envelope::Payload::StateDelta(
                orbisync_protocol::v1::StateDelta {
                    from_revision: 0,
                    to_revision: 1,
                    entities,
                },
            )),
            ..Default::default()
        }
        .encode_to_vec();
        for _ in 0..RECEIVERS {
            let sink = Arc::new(RealtimeDeliverySink::new(
                Arc::clone(&state),
                instance,
                Vec3::new(0.0, 0.0, 0.0).unwrap(),
                UserId::generate(),
                None,
                std::collections::HashMap::new(),
            ));
            registrations.push(delivery.register_sink(instance, sink));
        }
        fixtures.push((
            instance,
            Arc::new(
                views
                    .into_iter()
                    .collect::<orbisync_world_runtime::InterestSnapshot>(),
            ),
            payload,
        ));
    }
    for concurrent in [false, true] {
        let mut samples = Vec::new();
        for sample in 0..6 {
            let started = Instant::now();
            let run = |fixture: &(
                InstanceId,
                Arc<orbisync_world_runtime::InterestSnapshot>,
                Vec<u8>,
            )| {
                for _ in 0..BROADCASTS {
                    let outcome = delivery.broadcast_with_views(
                        fixture.0,
                        fixture.2.clone(),
                        Reliability::LatestWins,
                        Arc::clone(&fixture.1),
                        grid,
                    );
                    assert_eq!(outcome.delivered, RECEIVERS);
                }
            };
            if concurrent {
                std::thread::scope(|scope| {
                    for fixture in &fixtures {
                        scope.spawn(|| run(fixture));
                    }
                });
            } else {
                for fixture in &fixtures {
                    run(fixture);
                }
            }
            if sample > 0 {
                samples.push(started.elapsed().as_micros());
            }
        }
        samples.sort_unstable();
        eprintln!(
            "ENGINE_DELIVERY parallel={concurrent} worlds={WORLDS} receivers={RECEIVERS} entities={ENTITIES} broadcasts_per_world={BROADCASTS} median_us={} samples_us={samples:?}",
            samples[2]
        );
    }
    drop(registrations);
}
