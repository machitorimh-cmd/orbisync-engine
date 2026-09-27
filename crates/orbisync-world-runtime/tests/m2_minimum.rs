#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(missing_docs)]

use orbisync_domain::{
    DomainErrorKind, Entity, EntityKind, InstanceId, PresenceId, Quaternion, Revision, Timestamp,
    Transform, UserId, Vec3, VisibilityPolicy,
};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeState,
    actor::InstanceActor,
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
    state::InstanceState,
    validation::validate_transform_update,
};

fn transform_at(x: f64) -> Transform {
    let pos = Vec3::new(x, 0.0, 0.0).expect("valid vec");
    let rot = Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid quat");
    let scale = Vec3::new(1.0, 1.0, 1.0).expect("valid scale");
    Transform::new(pos, rot, scale).expect("valid transform")
}

fn now_ts() -> Timestamp {
    Timestamp::from_unix_millis(1_000).expect("valid ts")
}

// (a) joining two different users to same InstanceState respects capacity
#[test]
fn instance_state_capacity_allows_two_different_users() {
    let instance_id = InstanceId::generate();
    let mut state = InstanceState::new(instance_id);
    let p1 = PresenceId::generate();
    let p2 = PresenceId::generate();
    let u1 = UserId::generate();
    let u2 = UserId::generate();

    assert!(state.add_member(p1, u1, 2));
    assert!(state.add_member(p2, u2, 2));
    assert_eq!(state.member_count(), 2);
    assert!(state.is_member(p1));
    assert!(state.is_member(p2));
}

#[test]
fn instance_state_capacity_rejects_when_full() {
    let instance_id = InstanceId::generate();
    let mut state = InstanceState::new(instance_id);
    let p1 = PresenceId::generate();
    let p2 = PresenceId::generate();
    let p3 = PresenceId::generate();

    assert!(state.add_member(p1, UserId::generate(), 2));
    assert!(state.add_member(p2, UserId::generate(), 2));
    // third join exceeds capacity 2
    assert!(!state.add_member(p3, UserId::generate(), 2));
    assert_eq!(state.member_count(), 2);
}

#[test]
fn actor_join_two_different_users_respects_capacity() {
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });

    let p1 = PresenceId::generate();
    let u1 = UserId::generate();
    let p2 = PresenceId::generate();
    let u2 = UserId::generate();

    let out1 = actor.handle(InstanceCommand::Join {
        presence_id: p1,
        user_id: u1,
        instance_id,
        capacity: 100,
    });
    assert!(
        matches!(out1, CommandOutcome::Applied { .. }),
        "first join should apply: {out1:?}"
    );

    let out2 = actor.handle(InstanceCommand::Join {
        presence_id: p2,
        user_id: u2,
        instance_id,
        capacity: 100,
    });
    assert!(
        matches!(out2, CommandOutcome::Applied { .. }),
        "second join should apply: {out2:?}"
    );
}

// (b) transform validation rejects NaN/Infinity and teleport (>10 distance)
#[test]
fn transform_validation_rejects_nan() {
    assert!(
        Vec3::new(f64::NAN, 0.0, 0.0).is_err(),
        "Vec3 should reject NaN"
    );
    assert!(
        Quaternion::new(0.0, 0.0, f64::NAN, 1.0).is_err(),
        "Quaternion should reject NaN"
    );
}

#[test]
fn transform_validation_rejects_infinity() {
    assert!(
        Vec3::new(f64::INFINITY, 0.0, 0.0).is_err(),
        "Vec3 should reject Infinity"
    );
    assert!(
        Vec3::new(f64::NEG_INFINITY, 0.0, 0.0).is_err(),
        "Vec3 should reject -Infinity"
    );
    assert!(
        Quaternion::new(0.0, 0.0, f64::INFINITY, 1.0).is_err(),
        "Quaternion should reject Infinity"
    );
}

#[test]
fn transform_validation_rejects_teleport() {
    let prev = transform_at(0.0);
    let next = transform_at(20.0); // distance 20 > MAX_TICK_DISTANCE 10
    let err =
        validate_transform_update(Some(prev), next, 0.05).expect_err("teleport must be rejected");
    assert_eq!(err.kind(), DomainErrorKind::InvalidValue);
    assert!(err.detail().contains("per-tick") || err.detail().contains("distance"));

    // also test via actor path: second update exceeding per-tick limit should be rejected
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    let presence = PresenceId::generate();
    let user = UserId::generate();
    let join = InstanceCommand::Join {
        presence_id: presence,
        user_id: user,
        instance_id,
        capacity: 100,
    };
    assert!(matches!(actor.handle(join), CommandOutcome::Applied { .. }));

    let entity_id = orbisync_domain::EntityId::generate();
    // first transform auto-creates at 0.0
    let first = InstanceCommand::UpdateTransform {
        entity_id,
        transform: transform_at(0.0),
        expected_revision: Revision::from_u64(1),
        user_id: user,
        now: Timestamp::from_unix_millis(1_000).expect("valid"),
        permissions: WorldPermissions::all(),
    };
    assert!(matches!(
        actor.handle(first),
        CommandOutcome::Applied { .. }
    ));

    // second transform teleports to 20.0 -> should be INVALID_TRANSFORM
    let teleport = InstanceCommand::UpdateTransform {
        entity_id,
        transform: transform_at(20.0),
        expected_revision: Revision::from_u64(1), // entity revision after create is 1
        user_id: user,
        now: Timestamp::from_unix_millis(1_500).expect("valid"),
        permissions: WorldPermissions::all(),
    };
    let outcome = actor.handle(teleport);
    match outcome {
        CommandOutcome::Rejected { code, detail } => {
            assert_eq!(code, "INVALID_TRANSFORM");
            assert!(
                detail.contains("per-tick")
                    || detail.contains("distance")
                    || detail.contains("exceeds")
            );
        }
        other => panic!("expected INVALID_TRANSFORM rejection, got {other:?}"),
    }
}

// (c) revision mismatch is rejected
#[test]
fn entity_revision_mismatch_is_rejected() {
    let mut entity = Entity::new(
        orbisync_domain::EntityId::generate(),
        InstanceId::generate(),
        EntityKind::Object,
        None,
        None,
        VisibilityPolicy::Global,
        now_ts(),
    );
    let rev = entity.revision();
    // correct revision succeeds
    entity
        .update_transform(rev, Transform::identity(), now_ts())
        .expect("correct revision should succeed");
    // stale revision should fail
    let stale = Revision::from_u64(1); // entity now at 2
    let err = entity
        .update_transform(stale, Transform::identity(), now_ts())
        .expect_err("stale revision must be rejected");
    assert_eq!(err.kind(), DomainErrorKind::RevisionMismatch);
}

#[test]
fn actor_revision_mismatch_is_rejected() {
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    let presence = PresenceId::generate();
    let user = UserId::generate();
    actor.handle(InstanceCommand::Join {
        presence_id: presence,
        user_id: user,
        instance_id,
        capacity: 100,
    });

    let entity_id = orbisync_domain::EntityId::generate();
    // auto-create
    let first = InstanceCommand::UpdateTransform {
        entity_id,
        transform: transform_at(0.0),
        expected_revision: Revision::from_u64(0),
        user_id: user,
        now: Timestamp::from_unix_millis(1_000).expect("valid"),
        permissions: WorldPermissions::all(),
    };
    let out = actor.handle(first);
    assert!(
        matches!(out, CommandOutcome::Applied { .. }),
        "auto-create should apply: {out:?}"
    );

    // entity revision is 1 after creation; try mismatched expected_revision 99
    let bad = InstanceCommand::UpdateTransform {
        entity_id,
        transform: transform_at(0.5), // small movement, within limit
        expected_revision: Revision::from_u64(99),
        user_id: user,
        now: Timestamp::from_unix_millis(1_100).expect("valid"),
        permissions: WorldPermissions::all(),
    };
    let outcome = actor.handle(bad);
    match outcome {
        CommandOutcome::Rejected { code, detail } => {
            assert_eq!(code, "REVISION_MISMATCH");
            assert!(detail.contains("99") || detail.contains("revision"));
        }
        other => panic!("expected REVISION_MISMATCH, got {other:?}"),
    }

    // also verify correct revision succeeds
    let good = InstanceCommand::UpdateTransform {
        entity_id,
        transform: transform_at(0.5),
        expected_revision: Revision::from_u64(1),
        user_id: user,
        now: Timestamp::from_unix_millis(1_100).expect("valid"),
        permissions: WorldPermissions::all(),
    };
    let outcome = actor.handle(good);
    assert!(
        matches!(outcome, CommandOutcome::Applied { .. }),
        "correct revision should apply: {outcome:?}"
    );
}

// (d) snapshot generation returns valid JSON with instance_id and revision
#[test]
fn snapshot_returns_valid_json_with_instance_id_and_revision() {
    let instance_id = InstanceId::generate();
    let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    });
    // initial snapshot
    let bytes = actor.snapshot();
    let s = String::from_utf8(bytes).expect("snapshot utf8");
    let v: serde_json::Value = serde_json::from_str(&s).expect("snapshot must be valid JSON");
    assert!(v.is_object(), "snapshot must be JSON object");
    assert_eq!(
        v.get("instance_id")
            .and_then(|x| x.as_str())
            .expect("instance_id string"),
        instance_id.to_string()
    );
    assert_eq!(
        v.get("revision")
            .and_then(|x| x.as_u64())
            .expect("revision u64"),
        0
    );
    assert!(v.get("members").is_some(), "members field must exist");

    // after join, revision increments and members increments
    let presence = PresenceId::generate();
    let user = UserId::generate();
    actor.handle(InstanceCommand::Join {
        presence_id: presence,
        user_id: user,
        instance_id,
        capacity: 100,
    });
    let bytes2 = actor.snapshot();
    let s2 = String::from_utf8(bytes2).expect("snapshot utf8");
    let v2: serde_json::Value = serde_json::from_str(&s2).expect("snapshot must be valid JSON");
    assert_eq!(
        v2.get("instance_id").and_then(|x| x.as_str()).unwrap(),
        instance_id.to_string()
    );
    assert_eq!(v2.get("revision").and_then(|x| x.as_u64()).unwrap(), 1);
    assert_eq!(v2.get("members").and_then(|x| x.as_u64()).unwrap(), 1);
}
