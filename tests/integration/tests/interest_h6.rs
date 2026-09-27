//! H-6 verification: interest culling.
//!
//! - two clients 50 m apart do NOT receive each other's `StateDelta`
//! - `OwnerOnly` entity delta not received by non-owner
//!
//! Uses `InstanceActor` + `UniformGrid` + `filter_visible_entities` /
//! `filter_delta_for_viewer` to simulate interest filtering deterministically.
//! Positions use `Transform` / `Vec3`; distances are Euclidean (MRIB §7).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use orbisync_domain::{
    EntityId, EntityKind, InstanceId, Quaternion, Transform, Vec3, VisibilityPolicy,
};
use orbisync_domain::{Revision, UserId};
use orbisync_interest::UniformGrid;
use orbisync_server::interest_filter::{filter_delta_for_viewer, filter_visible_entities};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeState,
    actor::InstanceActor,
    command::{InstanceCommand, WorldPermissions},
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn vec3(x: f64, y: f64, z: f64) -> Vec3 {
    Vec3::new(x, y, z).expect("valid vec3")
}

fn transform_at(pos: Vec3) -> Transform {
    let rot = Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid quat");
    let scale = Vec3::new(1.0, 1.0, 1.0).expect("valid scale");
    Transform::new(pos, rot, scale).expect("valid transform")
}

fn new_actor() -> (InstanceActor, InstanceId) {
    let instance_id = InstanceId::generate();
    let actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    (actor, instance_id)
}

// ---------------------------------------------------------------------------
// H-6: 50 m apart => no cross-delta
// ---------------------------------------------------------------------------

#[test]
fn h6_two_clients_50m_apart_do_not_receive_each_other_state_delta() {
    // Two viewers 50 m apart on X. Default near=30, unsub=35, so 50 > 35 => no interest.
    let (mut actor, _instance_id) = new_actor();
    let grid = UniformGrid::default();

    let user_a = UserId::generate();
    let user_b = UserId::generate();

    let entity_a = EntityId::generate();
    let entity_b = EntityId::generate();

    let pos_a = vec3(0.0, 0.0, 0.0);
    let pos_b = vec3(50.0, 0.0, 0.0);

    // Spawn two avatar-like entities at the two viewer positions.
    // Visibility Spatial(30) matches `actor.rs` auto-create default.
    let vis_a = VisibilityPolicy::spatial(30.0).expect("valid");
    let vis_b = VisibilityPolicy::spatial(30.0).expect("valid");
    actor
        .handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id: entity_a,
            kind: EntityKind::Avatar,
            owner: Some(user_a),
            transform: Some(transform_at(pos_a)),
            visibility: vis_a,
            requester: user_a,
            permissions: WorldPermissions::all(),
        })
        .expect_applied();
    actor
        .handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id: entity_b,
            kind: EntityKind::Avatar,
            owner: Some(user_b),
            transform: Some(transform_at(pos_b)),
            visibility: vis_b,
            requester: user_b,
            permissions: WorldPermissions::all(),
        })
        .expect_applied();

    let entities = actor.entities_snapshot();
    assert_eq!(entities.len(), 2, "both entities must be present");

    // Viewers at same positions as their avatars.
    let viewer_a_pos = pos_a;
    let viewer_b_pos = pos_b;

    // Full snapshot filtering: each viewer sees only its own nearby entity.
    let visible_to_a =
        filter_visible_entities(Some(viewer_a_pos), &entities, &grid, Some(user_a), None);
    let visible_to_b =
        filter_visible_entities(Some(viewer_b_pos), &entities, &grid, Some(user_b), None);

    assert!(
        visible_to_a.contains(&entity_a),
        "viewer A must see own entity at 0m"
    );
    assert!(
        !visible_to_a.contains(&entity_b),
        "viewer A must NOT see entity B 50m away"
    );
    assert!(
        visible_to_b.contains(&entity_b),
        "viewer B must see own entity at 0m"
    );
    assert!(
        !visible_to_b.contains(&entity_a),
        "viewer B must NOT see entity A 50m away"
    );

    // Simulate StateDelta: only entity_a changed -> viewer B must not receive.
    let delta_a: Vec<_> = entities
        .iter()
        .filter(|e| e.id() == entity_a)
        .cloned()
        .collect();
    assert_eq!(delta_a.len(), 1);
    let delta_for_b =
        filter_delta_for_viewer(Some(viewer_b_pos), &delta_a, &grid, Some(user_b), None);
    assert!(
        !delta_for_b.contains(&entity_a),
        "StateDelta for entity A must NOT be delivered to B 50m away"
    );
    let delta_for_a =
        filter_delta_for_viewer(Some(viewer_a_pos), &delta_a, &grid, Some(user_a), None);
    assert!(
        delta_for_a.contains(&entity_a),
        "StateDelta for entity A must be delivered to A (owner/viewer at same pos)"
    );

    // And vice versa: delta for B not visible to A.
    let delta_b: Vec<_> = entities
        .iter()
        .filter(|e| e.id() == entity_b)
        .cloned()
        .collect();
    let delta_for_a2 =
        filter_delta_for_viewer(Some(viewer_a_pos), &delta_b, &grid, Some(user_a), None);
    assert!(
        !delta_for_a2.contains(&entity_b),
        "StateDelta for entity B must NOT be delivered to A 50m away"
    );
}

// ---------------------------------------------------------------------------
// H-6: OwnerOnly not received by non-owner
// ---------------------------------------------------------------------------

#[test]
fn h6_owner_only_entity_delta_not_received_by_non_owner() {
    let (mut actor, _instance_id) = new_actor();
    let grid = UniformGrid::default();

    let owner = UserId::generate();
    let non_owner = UserId::generate();

    let owner_entity = EntityId::generate();
    // Place OwnerOnly entity 2 m from viewers (well within 30 m near radius).
    let entity_pos = vec3(2.0, 0.0, 0.0);
    let viewer_pos = vec3(0.0, 0.0, 0.0);

    actor
        .handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id: owner_entity,
            kind: EntityKind::Object,
            owner: Some(owner),
            transform: Some(transform_at(entity_pos)),
            visibility: VisibilityPolicy::OwnerOnly,
            requester: owner,
            permissions: WorldPermissions::all(),
        })
        .expect_applied();

    let entities = actor.entities_snapshot();
    assert_eq!(entities.len(), 1);

    // Snapshot filtering: owner sees, non-owner does not, even at same position.
    let visible_owner =
        filter_visible_entities(Some(viewer_pos), &entities, &grid, Some(owner), None);
    let visible_non_owner =
        filter_visible_entities(Some(viewer_pos), &entities, &grid, Some(non_owner), None);

    assert!(
        visible_owner.contains(&owner_entity),
        "OwnerOnly entity must be visible to owner"
    );
    assert!(
        !visible_non_owner.contains(&owner_entity),
        "OwnerOnly entity must NOT be visible to non-owner"
    );

    // Delta filtering: same ownership check.
    let delta = entities.clone();
    let delta_owner = filter_delta_for_viewer(Some(viewer_pos), &delta, &grid, Some(owner), None);
    let delta_non_owner =
        filter_delta_for_viewer(Some(viewer_pos), &delta, &grid, Some(non_owner), None);

    assert!(
        delta_owner.contains(&owner_entity),
        "OwnerOnly delta must be delivered to owner"
    );
    assert!(
        !delta_non_owner.contains(&owner_entity),
        "OwnerOnly delta must NOT be delivered to non-owner"
    );

    // Anonymous viewer (None) must also not see OwnerOnly.
    let delta_anon = filter_delta_for_viewer(Some(viewer_pos), &delta, &grid, None, None);
    assert!(
        !delta_anon.contains(&owner_entity),
        "OwnerOnly delta must NOT be delivered to anonymous viewer"
    );
}

// ---------------------------------------------------------------------------
// local trait for ergonomic expect
// ---------------------------------------------------------------------------

trait ExpectApplied {
    fn expect_applied(self);
}

impl ExpectApplied for orbisync_world_runtime::command::CommandOutcome {
    fn expect_applied(self) {
        match self {
            orbisync_world_runtime::command::CommandOutcome::Applied { .. } => {}
            orbisync_world_runtime::command::CommandOutcome::Rejected { code, detail } => {
                panic!("expected Applied, got Rejected {code}: {detail}")
            }
        }
    }
}
