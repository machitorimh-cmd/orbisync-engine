#![allow(clippy::expect_used, clippy::unwrap_used, clippy::needless_update)]
#![allow(missing_docs)]

//! Benchmark for the delivery hot path (N-5).
//!
//! Measures `filter_payload_for_viewer` per-message cost at N=10 / 100 / 500
//! entities. The shared `InterestSnapshot` index is built outside measurement;
//! lookup and stale subscription checks reuse it without rebuilding a map/set.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use orbisync_domain::{EntityId, InstanceId, UserId, Vec3, VisibilityPolicy};
use orbisync_interest::UniformGrid;
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_server::realtime_ws::filter_payload_for_viewer_indexed;
use orbisync_world_runtime::actor::EntityInterestView;
use prost::Message;
use std::collections::HashMap;

/// Builds `views` and a matching `StateDelta` payload for `n` entities.
fn setup(n: usize) -> (Vec<EntityInterestView>, Vec<u8>, Vec3, UserId, UniformGrid) {
    let viewer = Vec3::new(0.0, 0.0, 0.0).expect("valid");
    let viewer_user = UserId::generate();
    let instance = InstanceId::generate();
    let _ = instance;
    let grid = UniformGrid::default();

    let mut views = Vec::with_capacity(n);
    let mut entity_states = Vec::with_capacity(n);

    for i in 0..n {
        let eid = EntityId::generate();
        // Spread along X so some are near (within 30m) and some far.
        // At default near=30, first ~15 are within range when spaced 2m.
        let x = (i as f64) * 2.0;
        let pos = Vec3::new(x, 0.0, 0.0).expect("valid");
        let view = EntityInterestView {
            id: eid,
            owner: None,
            position: Some(pos),
            visibility: VisibilityPolicy::spatial(30.0).expect("valid"),
        };
        views.push(view);

        let transform = orbisync_protocol::v1::Transform {
            position_x: x as f32,
            position_y: 0.0,
            position_z: 0.0,
            rotation_x: 0.0,
            rotation_y: 0.0,
            rotation_z: 0.0,
            rotation_w: 1.0,
            ..Default::default()
        };
        let es = orbisync_protocol::v1::EntityState {
            entity_id: eid.to_string(),
            revision: 1,
            transform: Some(transform),
            properties: None,
            velocity: None,
            animation: None,
            presence: None,
        };
        entity_states.push(es);
    }

    let delta = orbisync_protocol::v1::StateDelta {
        from_revision: 0,
        to_revision: 1,
        entities: entity_states,
    };
    let envelope = Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: 1,
        message_id: "bench".to_owned(),
        sequence: 0,
        sent_at_unix_ms: 0,
        instance_id: InstanceId::generate().to_string(),
        payload: Some(envelope::Payload::StateDelta(delta)),
    };
    let mut buf = Vec::new();
    envelope.encode(&mut buf).expect("encode");

    (views, buf, viewer, viewer_user, grid)
}

fn bench_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_payload_for_viewer");

    for n in [10usize, 100, 500] {
        let (views, payload, viewer, viewer_user, grid) = setup(n);
        // Pre-fill subscribed as if all near entities were already subscribed
        // to exercise hysteresis path (`should_retain` true branch).
        let mut subscribed: HashMap<EntityId, bool> = HashMap::new();
        for v in &views {
            // Mark first 20 as subscribed to get mix of retain vs subscribe
            let pos = v.position.expect("pos");
            let dist = UniformGrid::euclidean_distance(viewer, pos);
            if dist <= 30.0 {
                subscribed.insert(v.id, true);
            }
        }
        let sub_clone = subscribed.clone();
        let views = views
            .into_iter()
            .collect::<orbisync_world_runtime::InterestSnapshot>();

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut subs = sub_clone.clone();
                let out = filter_payload_for_viewer_indexed(
                    &payload,
                    Some(viewer),
                    Some(viewer_user),
                    &grid,
                    &views,
                    None,
                    &mut subs,
                );
                // Prevent optimizer from dropping
                std::hint::black_box(out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_filter);
criterion_main!(benches);
