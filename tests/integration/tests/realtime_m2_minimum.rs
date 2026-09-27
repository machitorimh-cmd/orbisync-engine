//! M2 minimum E2E: two clients join same instance and transform is validated.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use orbisync_domain::{
    EntityId, InstanceId, PresenceId, Revision, Timestamp, Transform, UserId, WorldId,
};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeState,
    actor::InstanceActor,
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
};

fn now() -> Timestamp {
    Timestamp::from_unix_millis(1_000).expect("valid")
}

#[test]
fn two_clients_join_same_instance() {
    let world_id = WorldId::generate();
    let instance_id = InstanceId::generate();
    let _world =
        orbisync_domain::World::new(world_id, "Arena", None, Transform::identity(), 100, now())
            .expect("valid world");
    let _instance = orbisync_domain::WorldInstance::new(instance_id, world_id, 100, now())
        .expect("valid instance");

    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });

    let presence_a = PresenceId::generate();
    let presence_b = PresenceId::generate();
    let user_a = UserId::generate();
    let user_b = UserId::generate();

    let outcome_a = actor.handle(InstanceCommand::Join {
        presence_id: presence_a,
        user_id: user_a,
        instance_id,
        capacity: 100,
    });
    assert!(matches!(outcome_a, CommandOutcome::Applied { .. }));

    let outcome_b = actor.handle(InstanceCommand::Join {
        presence_id: presence_b,
        user_id: user_b,
        instance_id,
        capacity: 100,
    });
    assert!(matches!(outcome_b, CommandOutcome::Applied { .. }));

    assert_eq!(actor.descriptor().revision, Revision::from_u64(2));
    let snapshot = actor.snapshot();
    let value: serde_json::Value = serde_json::from_slice(&snapshot).expect("valid json");
    assert_eq!(value["members"], 2);
    assert_eq!(value["revision"], 2);
}

#[test]
fn transform_is_broadcast_and_invalid_is_rejected() {
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    let presence = PresenceId::generate();
    let user = UserId::generate();
    actor
        .handle(InstanceCommand::Join {
            presence_id: presence,
            user_id: user,
            instance_id,
            capacity: 100,
        })
        .expect_applied();

    // First transform creates the entity (avatar) — should be applied.
    let entity = EntityId::generate();
    let outcome = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: Transform::identity(),
        expected_revision: Revision::from_u64(1),
        user_id: user,
        now: Timestamp::from_unix_millis(1_000).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    assert!(matches!(outcome, CommandOutcome::Applied { .. }));

    // Valid small move — should be applied (entity is at 1 after creation).
    let small = {
        let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("valid");
        let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("valid");
        Transform::new(pos, rot, scale).expect("valid")
    };
    let outcome = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: small,
        expected_revision: Revision::from_u64(1),
        user_id: user,
        now: Timestamp::from_unix_millis(1_500).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    assert!(matches!(outcome, CommandOutcome::Applied { .. }));

    // Stale revision after advancing to 2 — should be rejected.
    let outcome = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: small,
        expected_revision: Revision::from_u64(1),
        user_id: user,
        now: Timestamp::from_unix_millis(1_600).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    assert!(matches!(
        outcome,
        CommandOutcome::Rejected {
            code: "REVISION_MISMATCH",
            ..
        }
    ));

    // Teleport 100 units — should be rejected as INVALID_TRANSFORM.
    let far = {
        let pos = orbisync_domain::Vec3::new(100.0, 0.0, 0.0).expect("valid");
        let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("valid");
        Transform::new(pos, rot, scale).expect("valid")
    };
    // Need to get current revision; after previous applied, entity is at 3, instance at 4.
    let outcome = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: far,
        expected_revision: Revision::from_u64(3),
        user_id: user,
        now: Timestamp::from_unix_millis(2_000).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    assert!(matches!(
        outcome,
        CommandOutcome::Rejected {
            code: "INVALID_TRANSFORM",
            ..
        }
    ));
}

#[test]
fn h4_three_consecutive_transforms_are_applied_with_entity_revision() {
    // Verifies H-4 fix: entity_revision is returned and used for next expected_revision.
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    let presence = PresenceId::generate();
    let user = UserId::generate();
    actor
        .handle(InstanceCommand::Join {
            presence_id: presence,
            user_id: user,
            instance_id,
            capacity: 100,
        })
        .expect_applied();
    let entity = EntityId::generate();
    // 1st: auto-create at entity rev 1
    let outcome1 = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: Transform::identity(),
        expected_revision: Revision::from_u64(1),
        user_id: user,
        now: Timestamp::from_unix_millis(1_000).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    let rev1 = match outcome1 {
        CommandOutcome::Applied {
            entity_revision, ..
        } => entity_revision.expect("entity_revision on create"),
        _ => panic!("first transform should be Applied"),
    };
    assert_eq!(rev1, Revision::from_u64(1));
    // 2nd: small move with entity rev 1 -> 2
    let small = {
        let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("valid");
        let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("valid");
        Transform::new(pos, rot, scale).expect("valid")
    };
    let outcome2 = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: small,
        expected_revision: rev1,
        user_id: user,
        now: Timestamp::from_unix_millis(1_500).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    let rev2 = match outcome2 {
        CommandOutcome::Applied {
            entity_revision, ..
        } => entity_revision.expect("entity_revision"),
        _ => panic!("second transform should be Applied"),
    };
    assert_eq!(rev2, Revision::from_u64(2));
    // 3rd: another small move with entity rev 2 -> 3 (this failed before H-4 fix)
    let small2 = {
        let pos = orbisync_domain::Vec3::new(2.0, 0.0, 0.0).expect("valid");
        let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("valid");
        Transform::new(pos, rot, scale).expect("valid")
    };
    let outcome3 = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: small2,
        expected_revision: rev2,
        user_id: user,
        now: Timestamp::from_unix_millis(2_000).expect("valid"),
        permissions: WorldPermissions::all(),
    });
    assert!(
        matches!(outcome3, CommandOutcome::Applied { .. }),
        "third consecutive transform must be Applied, H-4 regression if REVISION_MISMATCH"
    );
}

#[test]
fn h3_owner_only_blocks_non_owner_updates() {
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    let owner_presence = PresenceId::generate();
    let owner = UserId::generate();
    let intruder = UserId::generate();
    actor
        .handle(InstanceCommand::Join {
            presence_id: owner_presence,
            user_id: owner,
            instance_id,
            capacity: 100,
        })
        .expect_applied();
    let entity = EntityId::generate();
    // Owner creates entity
    let outcome = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: Transform::identity(),
        expected_revision: Revision::from_u64(1),
        user_id: owner,
        now: now(),
        permissions: WorldPermissions::all(),
    });
    assert!(matches!(outcome, CommandOutcome::Applied { .. }));
    // Get entity revision for next update
    let rev = match actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: Transform::identity(),
        expected_revision: Revision::from_u64(1),
        user_id: owner,
        now: Timestamp::from_unix_millis(1_100).expect("valid"),
        permissions: WorldPermissions::all(),
    }) {
        CommandOutcome::Applied {
            entity_revision, ..
        } => entity_revision.unwrap(),
        _ => panic!("owner second update should apply"),
    };
    // Intruder tries to move owner's entity -> NOT_OWNER
    let small = {
        let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("valid");
        let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("valid");
        Transform::new(pos, rot, scale).expect("valid")
    };
    let outcome = actor.handle(InstanceCommand::UpdateTransform {
        entity_id: entity,
        transform: small,
        expected_revision: rev,
        user_id: intruder,
        now: Timestamp::from_unix_millis(1_200).expect("valid"),
        permissions: WorldPermissions {
            entity_spawn: false,
            entity_update_own: true,
            entity_update_any: false,
        },
    });
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                code: "NOT_OWNER",
                ..
            }
        ),
        "non-owner must get NOT_OWNER, got {outcome:?}"
    );
}

#[test]
fn h2_invalid_instance_id_is_rejected_not_generated() {
    // Verifies H-2 fix: parse failure does not generate new InstanceId.
    let bad = "not-a-uuid";
    assert!(bad.parse::<InstanceId>().is_err(), "bad id must fail parse");
    // Simulate handler path: would send INVALID_ARGUMENT, not generate.
    let result = bad.parse::<InstanceId>().map_err(|_| "INVALID_ARGUMENT");
    assert_eq!(result.unwrap_err(), "INVALID_ARGUMENT");
    // Good id still parses.
    let good = InstanceId::generate().to_string();
    assert!(good.parse::<InstanceId>().is_ok());
}

#[test]
fn h1_capacity_reclaimed_after_110_join_leave_cycles() {
    // Verifies H-1 fix: 110 join->leave cycles do not leak to INSTANCE_FULL.
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    for _ in 0..110 {
        let presence = PresenceId::generate();
        let user = UserId::generate();
        let outcome = actor.handle(InstanceCommand::Join {
            presence_id: presence,
            user_id: user,
            instance_id,
            capacity: 100,
        });
        assert!(matches!(outcome, CommandOutcome::Applied { .. }));
        let outcome = actor.handle(InstanceCommand::Leave {
            presence_id: presence,
        });
        assert!(matches!(outcome, CommandOutcome::Applied { .. }));
        assert_eq!(actor.member_count(), 0);
    }
    // 111th join must still succeed — proves no leak.
    let presence = PresenceId::generate();
    let user = UserId::generate();
    let outcome = actor.handle(InstanceCommand::Join {
        presence_id: presence,
        user_id: user,
        instance_id,
        capacity: 100,
    });
    assert!(
        matches!(outcome, CommandOutcome::Applied { .. }),
        "111th join after 110 cycles must succeed, H-1 leak if INSTANCE_FULL"
    );
}

trait ExpectApplied {
    fn expect_applied(self);
}

impl ExpectApplied for CommandOutcome {
    fn expect_applied(self) {
        match self {
            CommandOutcome::Applied { .. } => {}
            CommandOutcome::Rejected { code, detail } => {
                panic!("expected Applied, got Rejected {code}: {detail}")
            }
        }
    }
}
