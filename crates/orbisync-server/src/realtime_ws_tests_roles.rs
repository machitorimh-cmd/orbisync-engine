#[tokio::test]
async fn authenticated_viewer_roles_reach_role_restricted_visibility() {
    use std::collections::BTreeSet;

    use orbisync_domain::{EntityId, Role, RoleId, UserId, Vec3, VisibilityPolicy};
    use orbisync_testkit::FakeIdentityRepository;
    use orbisync_world_runtime::actor::EntityInterestView;

    let role_id = RoleId::generate();
    let role = Role::new(role_id, "moderator", None, []).expect("role");
    let repository = FakeIdentityRepository::new();
    let role_holder = UserId::generate();
    let no_role_user = UserId::generate();
    repository.set_roles(role_holder, vec![role]);

    // This is the authentication-stage result used by the live connection
    // before it calls the role-aware realtime visibility filter.
    let role_holder_roles = resolve_viewer_roles(Some(&repository), role_holder)
        .await
        .expect("repository result");
    let no_role_user_roles = resolve_viewer_roles(Some(&repository), no_role_user)
        .await
        .expect("repository result");

    let entity_id = EntityId::generate();
    let role_restricted = VisibilityPolicy::role_restricted([role_id]).expect("policy");
    let view = EntityInterestView {
        id: entity_id,
        owner: None,
        position: Some(Vec3::new(0.0, 0.0, 0.0).expect("position")),
        visibility: role_restricted,
    };
    let grid = orbisync_interest::UniformGrid::default();

    let visible_to_role_holder = crate::interest_filter::filter_visible_views_with_roles(
        Some(Vec3::new(0.0, 0.0, 0.0).expect("viewer position")),
        std::slice::from_ref(&view),
        &grid,
        Some(role_holder),
        Some(role_holder_roles.as_ref()),
        None,
    );
    let visible_to_user_without_role = crate::interest_filter::filter_visible_views_with_roles(
        Some(Vec3::new(0.0, 0.0, 0.0).expect("viewer position")),
        std::slice::from_ref(&view),
        &grid,
        Some(no_role_user),
        Some(no_role_user_roles.as_ref()),
        None,
    );

    assert_eq!(visible_to_role_holder, vec![entity_id]);
    assert!(visible_to_user_without_role.is_empty());
    assert_eq!(role_holder_roles.as_ref(), &BTreeSet::from([role_id]));
    assert_eq!(no_role_user_roles.as_ref(), &BTreeSet::new());
}
