//! Interest filtering helper for the composition root.
//!
//! `world-runtime` is forbidden to depend on `interest`
//! (`repo-crate-conventions.md` §3.2, §8 condition 4), so it performs no
//! interest filtering at all. The single `UniformGrid + VisibilityPolicy`
//! implementation lives here in `server`, which may depend on every crate
//! (composition root, `architecture.md` §4.3). The actor's former 35 m
//! distance helpers were removed in N-7 to keep exactly one implementation.
//!
//! This module is a pure, I/O-free helper: given a viewer position and a
//! slice of `Entity` or `EntityInterestView`, it returns the `EntityId`s
//! that the viewer should receive, honoring the hysteresis radius, coarse
//! cell check and the entity's `VisibilityPolicy` (MRIB §7.4, §7.5).
//! The `EntityInterestView` path (N-5) avoids cloning `Entity.components`.

use std::collections::BTreeSet;
use std::collections::HashMap;

use orbisync_domain::{Entity, EntityId, UserId, Vec3};
use orbisync_interest::UniformGrid;
use orbisync_world_runtime::actor::EntityInterestView;

/// Filters `entities` to those visible to a viewer at `viewer_pos`.
///
/// - When `viewer_pos` is `None`, all entities are returned (fallback for
///   spectators or tests that have no position yet). **This bypasses not only
///   the distance check but also the `VisibilityPolicy` check (`OwnerOnly`
///   would become visible to everyone), so it must never be used on the
///   live, authenticated client path. Live connections must always provide
///   `Some(viewer_pos)` seeded from `World::default_spawn` (N-4).**
/// - Otherwise an entity is visible when `grid.is_entity_visible` returns
///   true. That predicate combines:
///   1. hysteresis (`should_retain` with `near_radius` / `unsubscribe_radius`,
///      default 30 m / 35 m),
///   2. coarse cell check (`is_in_subscribed_area`, 5×5 for defaults, `2*radius+1` generic, M-9), and
///   3. `VisibilityPolicy` (`Global` always, `Spatial` within radius,
///      `OwnerOnly` only for the owner).
///
/// `viewer_user` is used to evaluate `OwnerOnly` ( `entity.owner() == viewer_user` ).
/// `subscribed` carries the previous hysteresis state per entity; when `None`
/// or missing, `false` (not yet subscribed) is assumed.
///
/// Non-finite distances are treated as not visible. Entities without a
/// transform are visible only when their policy permits it without a position
/// (`Global` or owned `OwnerOnly`); otherwise they are excluded because no
/// distance can be computed.
#[must_use]
pub fn filter_visible_entities(
    viewer_pos: Option<Vec3>,
    entities: &[Entity],
    grid: &UniformGrid,
    viewer_user: Option<UserId>,
    subscribed: Option<&HashMap<EntityId, bool>>,
) -> Vec<EntityId> {
    let Some(viewer) = viewer_pos else {
        return entities.iter().map(|e| e.id()).collect();
    };

    let mut out = Vec::new();
    let roles = BTreeSet::new();
    for entity in entities {
        let is_owner = match (viewer_user, entity.owner()) {
            (Some(v), Some(o)) => v == o,
            _ => false,
        };
        let currently_subscribed = subscribed
            .and_then(|m| m.get(&entity.id()).copied())
            .unwrap_or(false);
        let viewer_context = orbisync_interest::ViewerContext {
            user: viewer_user,
            roles: Some(&roles),
            is_owner,
        };

        let Some(transform) = entity.transform() else {
            // No position: visible only when policy does not require distance.
            // Global is always visible; OwnerOnly is visible only to the owner.
            // Spatial without a position cannot be evaluated, so exclude.
            let visible_without_pos =
                grid.is_visible_without_position(entity.visibility(), viewer_context);
            if visible_without_pos {
                out.push(entity.id());
            }
            continue;
        };

        let pos = transform.position();
        if grid.is_entity_visible(
            viewer,
            pos,
            entity.visibility(),
            viewer_context,
            currently_subscribed,
        ) {
            out.push(entity.id());
        }
    }
    out
}

/// Simplified helper that matches the task description: given a viewer `Vec3`
/// and a slice of `Entity`, returns the filtered `Vec<EntityId>` using the
/// supplied `UniformGrid` and each entity's `VisibilityPolicy`.
///
/// This is a convenience wrapper around [`filter_visible_entities`] that
/// assumes:
/// - viewer is at `viewer_pos` (not `None`),
/// - `currently_subscribed == false` for every entity (M2: fresh subscription),
/// - `viewer_user == None` (anonymous) except for the owner check per entity
///   where an entity with `Some(owner)` will only match when the caller
///   passes an explicit `viewer_user` via the full helper.
///
/// For the common server use-case (authenticated viewer), prefer the full
/// helper so `OwnerOnly` can be evaluated correctly.
#[must_use]
pub fn filter_entities_for_viewer(
    viewer_pos: Vec3,
    entities: &[Entity],
    grid: &UniformGrid,
) -> Vec<EntityId> {
    filter_visible_entities(Some(viewer_pos), entities, grid, None, None)
}

/// Variant that accepts owned `Vec<Entity>` (ergonomic for call sites that
/// already own the snapshot). Borrows and delegates to
/// [`filter_entities_for_viewer`].
#[must_use]
pub fn filter_owned_entities_for_viewer(
    viewer_pos: Vec3,
    entities: Vec<Entity>,
    grid: &UniformGrid,
) -> Vec<EntityId> {
    filter_visible_entities(Some(viewer_pos), &entities, grid, None, None)
}

/// Filters a delta (changed entities) to what a viewer should receive.
///
/// Thin wrapper around [`filter_visible_entities`] with the same hysteresis
/// semantics; kept as a named entry point so `realtime_ws.rs` can call
/// `interest_filter::filter_delta_for_viewer` explicitly.
#[must_use]
pub fn filter_delta_for_viewer(
    viewer_pos: Option<Vec3>,
    changed: &[Entity],
    grid: &UniformGrid,
    viewer_user: Option<UserId>,
    subscribed: Option<&HashMap<EntityId, bool>>,
) -> Vec<EntityId> {
    filter_visible_entities(viewer_pos, changed, grid, viewer_user, subscribed)
}

/// Filters lightweight `EntityInterestView` slices (N-5).
///
/// Same semantics as [`filter_visible_entities`] but avoids cloning
/// `Entity.components`. Used on the delivery hot path where each receiver
/// reuses the actor's shared `InterestSnapshot` instead of cloning entities.
#[must_use]
pub fn filter_visible_views(
    viewer_pos: Option<Vec3>,
    views: &[EntityInterestView],
    grid: &UniformGrid,
    viewer_user: Option<UserId>,
    subscribed: Option<&HashMap<EntityId, bool>>,
) -> Vec<EntityId> {
    filter_visible_views_with_roles(viewer_pos, views, grid, viewer_user, None, subscribed)
}

/// Role-aware variant used by authenticated realtime connections.
#[must_use]
pub fn filter_visible_views_with_roles<'a>(
    viewer_pos: Option<Vec3>,
    views: impl IntoIterator<Item = &'a EntityInterestView>,
    grid: &UniformGrid,
    viewer_user: Option<UserId>,
    roles: Option<&BTreeSet<orbisync_domain::RoleId>>,
    subscribed: Option<&HashMap<EntityId, bool>>,
) -> Vec<EntityId> {
    let Some(viewer) = viewer_pos else {
        return views.into_iter().map(|v| v.id).collect();
    };

    let mut out = Vec::new();
    for view in views {
        let is_owner = match (viewer_user, view.owner) {
            (Some(v), Some(o)) => v == o,
            _ => false,
        };
        let currently_subscribed = subscribed
            .and_then(|m| m.get(&view.id).copied())
            .unwrap_or(false);
        let viewer_context = orbisync_interest::ViewerContext {
            user: viewer_user,
            roles,
            is_owner,
        };

        let Some(pos) = view.position else {
            let visible_without_pos =
                grid.is_visible_without_position(&view.visibility, viewer_context);
            if visible_without_pos {
                out.push(view.id);
            }
            continue;
        };

        if grid.is_entity_visible(
            viewer,
            pos,
            &view.visibility,
            viewer_context,
            currently_subscribed,
        ) {
            out.push(view.id);
        }
    }
    out
}

/// Convenience wrapper for `filter_visible_views` with a concrete `Vec3` viewer.
#[must_use]
pub fn filter_views_for_viewer(
    viewer_pos: Vec3,
    views: &[EntityInterestView],
    grid: &UniformGrid,
) -> Vec<EntityId> {
    filter_visible_views(Some(viewer_pos), views, grid, None, None)
}

#[cfg(test)]
mod tests {
    use super::{
        filter_entities_for_viewer, filter_visible_entities, filter_visible_views_with_roles,
    };
    use orbisync_domain::{
        Entity, EntityId, EntityKind, InstanceId, Timestamp, Vec3, VisibilityPolicy,
    };
    use orbisync_interest::UniformGrid;
    use std::collections::BTreeSet;

    fn vec3(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3::new(x, y, z).expect("valid vec3")
    }

    fn entity_at(
        id: EntityId,
        instance: InstanceId,
        pos: Option<Vec3>,
        visibility: VisibilityPolicy,
    ) -> Entity {
        let transform = pos.map(|p| {
            let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
            let scale = Vec3::new(1.0, 1.0, 1.0).expect("valid");
            orbisync_domain::Transform::new(p, rot, scale).expect("valid")
        });
        Entity::new(
            id,
            instance,
            EntityKind::Object,
            None,
            transform,
            visibility,
            Timestamp::from_unix_millis(0).expect("valid"),
        )
    }

    #[test]
    fn no_viewer_includes_all() {
        let grid = UniformGrid::default();
        let instance = InstanceId::generate();
        let e1 = entity_at(
            EntityId::generate(),
            instance,
            Some(vec3(1000.0, 0.0, 1000.0)),
            VisibilityPolicy::Global,
        );
        let e2 = entity_at(
            EntityId::generate(),
            instance,
            None,
            VisibilityPolicy::Global,
        );
        let out = filter_visible_entities(None, &[e1.clone(), e2.clone()], &grid, None, None);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&e1.id()));
        assert!(out.contains(&e2.id()));
    }

    #[test]
    fn distance_and_visibility_policy() {
        let grid = UniformGrid::default(); // near 30, unsub 35
        let instance = InstanceId::generate();
        let viewer = vec3(0.0, 0.0, 0.0);
        let near = entity_at(
            EntityId::generate(),
            instance,
            Some(vec3(5.0, 0.0, 0.0)),
            VisibilityPolicy::spatial(50.0).unwrap(),
        );
        let far = entity_at(
            EntityId::generate(),
            instance,
            Some(vec3(200.0, 0.0, 200.0)),
            VisibilityPolicy::spatial(50.0).unwrap(),
        );
        let global_far = entity_at(
            EntityId::generate(),
            instance,
            Some(vec3(200.0, 0.0, 200.0)),
            VisibilityPolicy::Global,
        );

        // Near spatial should be visible, far spatial not, global far always visible.
        let out = filter_entities_for_viewer(
            viewer,
            &[near.clone(), far.clone(), global_far.clone()],
            &grid,
        );
        assert!(out.contains(&near.id()));
        assert!(!out.contains(&far.id()));
        assert!(out.contains(&global_far.id()));
    }

    #[test]
    fn owner_only_requires_owner() {
        let grid = UniformGrid::default();
        let instance = InstanceId::generate();
        let viewer = vec3(0.0, 0.0, 0.0);
        let user = orbisync_domain::UserId::generate();
        let other = orbisync_domain::UserId::generate();
        let owned_visible = {
            let e = entity_at(
                EntityId::generate(),
                instance,
                Some(vec3(5.0, 0.0, 0.0)),
                VisibilityPolicy::OwnerOnly,
            );
            // Entity with owner = user
            let pos = vec3(5.0, 0.0, 0.0);
            let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
            let scale = Vec3::new(1.0, 1.0, 1.0).expect("valid");
            let t = orbisync_domain::Transform::new(pos, rot, scale).expect("valid");
            let ts = Timestamp::from_unix_millis(0).expect("valid");
            Entity::new(
                e.id(),
                instance,
                EntityKind::Object,
                Some(user),
                Some(t),
                VisibilityPolicy::OwnerOnly,
                ts,
            )
        };
        let out_owner = filter_visible_entities(
            Some(viewer),
            std::slice::from_ref(&owned_visible),
            &grid,
            Some(user),
            None,
        );
        assert!(out_owner.contains(&owned_visible.id()));
        let out_other = filter_visible_entities(
            Some(viewer),
            std::slice::from_ref(&owned_visible),
            &grid,
            Some(other),
            None,
        );
        assert!(!out_other.contains(&owned_visible.id()));
    }

    #[test]
    fn role_restricted_snapshot_and_delta_filter_uses_loaded_roles() {
        let grid = UniformGrid::default();
        let role = orbisync_domain::RoleId::generate();
        let entity = entity_at(
            EntityId::generate(),
            InstanceId::generate(),
            Some(vec3(5.0, 0.0, 0.0)),
            VisibilityPolicy::role_restricted([role]).unwrap(),
        );
        let view = orbisync_world_runtime::actor::EntityInterestView::from_entity(&entity);
        let roles = BTreeSet::from([role]);
        assert_eq!(
            filter_visible_views_with_roles(
                Some(vec3(0.0, 0.0, 0.0)),
                std::slice::from_ref(&view),
                &grid,
                None,
                Some(&roles),
                None,
            ),
            vec![entity.id()]
        );
        assert!(
            filter_visible_views_with_roles(
                Some(vec3(0.0, 0.0, 0.0)),
                std::slice::from_ref(&view),
                &grid,
                None,
                None,
                None,
            )
            .is_empty()
        );
    }
}
