//! Uniform grid spatial interest.

use orbisync_domain::{RoleId, UserId, VisibilityPolicy, transform::Vec3};
use std::collections::BTreeSet;

/// Authenticated subject information used by visibility evaluation.
#[derive(Debug, Clone, Copy)]
pub struct ViewerContext<'a> {
    /// Authenticated user, if present.
    pub user: Option<UserId>,
    /// Roles loaded for the authenticated user.
    pub roles: Option<&'a BTreeSet<RoleId>>,
    /// Whether the user owns the candidate entity.
    pub is_owner: bool,
}

use crate::InterestParameters;

/// Coordinate of a uniform grid cell in the XZ plane.
///
/// The grid is aligned to world origin and `cell_size`. `y` is not part of
/// the cell key because `interest` uses an XZ uniform grid (MRIB §7.2,
/// Y-up convention in spec §9.7). The coordinate is
/// `(floor(x / cell_size), floor(z / cell_size))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellCoord {
    /// Cell index along world X.
    pub x: i64,
    /// Cell index along world Z.
    pub z: i64,
}

impl CellCoord {
    /// Creates a cell coordinate.
    #[must_use]
    pub const fn new(x: i64, z: i64) -> Self {
        Self { x, z }
    }
}

/// Uniform spatial grid view for interest calculation.
///
/// Pure, I/O-free. The grid is a derived view recomputable from
/// `instance_runtime` state (`state-and-runtime.md` §1.1). It never touches
/// sockets, queues or the gateway and depends only on `orbisync-domain`.
///
/// Default parameters follow MRIB §7.2 / §7.5 and `config/model.rs`:
/// `cell_size = 20.0`, `near_radius = 30.0`, `unsubscribe_radius = 35.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UniformGrid {
    params: InterestParameters,
}

impl UniformGrid {
    /// Default cell size in world units (meters).
    pub const DEFAULT_CELL_SIZE: f64 = 20.0;

    /// Creates a grid from validated parameters.
    #[must_use]
    pub const fn new(params: InterestParameters) -> Self {
        Self { params }
    }

    /// Returns the validated parameters backing this grid.
    #[must_use]
    pub const fn params(self) -> InterestParameters {
        self.params
    }

    /// Returns the cell size.
    #[must_use]
    pub fn cell_size(self) -> f64 {
        self.params.cell_size()
    }

    /// Returns the subscribe (near) radius.
    #[must_use]
    pub fn near_radius(self) -> f64 {
        self.params.near_radius()
    }

    /// Returns the unsubscribe radius (hysteresis).
    #[must_use]
    pub fn unsubscribe_radius(self) -> f64 {
        self.params.unsubscribe_radius()
    }

    /// Assigns a world position to its grid cell.
    ///
    /// Computed as `(floor(x / cell_size), floor(z / cell_size))` in the XZ
    /// plane. `y` is ignored for cell assignment.
    #[must_use]
    pub fn cell_for(self, position: Vec3) -> CellCoord {
        let size = self.cell_size();
        // `cell_size` is validated >0 and finite, so division is safe.
        let cx = (f64::from(position.x()) / size).floor() as i64;
        let cz = (f64::from(position.z()) / size).floor() as i64;
        CellCoord { x: cx, z: cz }
    }

    /// Returns the 9 cells consisting of `center` and its 8 neighbours.
    ///
    /// Order is deterministic row-major: `z-1` row, `z` row, `z+1` row.
    #[must_use]
    pub fn neighbour_cells(self, center: CellCoord) -> [CellCoord; 9] {
        let mut out = [CellCoord::new(0, 0); 9];
        let mut idx = 0usize;
        let mut dz = -1i64;
        while dz <= 1 {
            let mut dx = -1i64;
            while dx <= 1 {
                out[idx] = CellCoord::new(center.x + dx, center.z + dz);
                idx += 1;
                dx += 1;
            }
            dz += 1;
        }
        out
    }

    /// Returns the subscription radius in cells.
    ///
    /// Computed as `ceil(unsubscribe_radius / cell_size)` (M-9). For the
    /// defaults `35 / 20 = 1.75 -> 2`, so 5×5 cells.
    #[must_use]
    pub fn subscription_radius(self) -> i64 {
        let size = self.cell_size();
        let unsub = self.unsubscribe_radius();
        // `cell_size` is validated >0 and finite, so division is safe.
        (unsub / size).ceil() as i64
    }

    /// Returns the cells a viewer at `viewer_pos` subscribes to.
    ///
    /// This covers a square of side `2*radius+1` where `radius =
    /// ceil(unsubscribe_radius / cell_size)` (MRIB §7.2, M-9). For defaults
    /// `radius=2` so 25 cells (5×5). Earlier 3×3 was insufficient to cover
    /// `near_radius=30` in the worst case (cell edge).
    #[must_use]
    pub fn subscribed_cells(self, viewer_pos: Vec3) -> Vec<CellCoord> {
        let center = self.cell_for(viewer_pos);
        let radius = self.subscription_radius();
        let side = (2 * radius + 1) as usize;
        let mut out = Vec::with_capacity(side * side);
        let mut dz = -radius;
        while dz <= radius {
            let mut dx = -radius;
            while dx <= radius {
                out.push(CellCoord::new(center.x + dx, center.z + dz));
                dx += 1;
            }
            dz += 1;
        }
        out
    }

    /// Returns true when `entity_pos` lies in a cell subscribed by a viewer at
    /// `viewer_pos`.
    ///
    /// This is the coarse grid check. Callers typically combine it with a
    /// fine-grained distance / visibility policy check.
    #[must_use]
    pub fn is_in_subscribed_area(self, viewer_pos: Vec3, entity_pos: Vec3) -> bool {
        let entity_cell = self.cell_for(entity_pos);
        let viewer_cell = self.cell_for(viewer_pos);
        let radius = self.subscription_radius() as u64;
        // Membership in the square needs two comparisons, not an allocated
        // cell list and a linear scan. abs_diff also handles saturated cells.
        viewer_cell.x.abs_diff(entity_cell.x) <= radius
            && viewer_cell.z.abs_diff(entity_cell.z) <= radius
    }

    /// Euclidean distance between two world positions.
    #[must_use]
    pub fn euclidean_distance(a: Vec3, b: Vec3) -> f64 {
        let dx = f64::from(a.x()) - f64::from(b.x());
        let dy = f64::from(a.y()) - f64::from(b.y());
        let dz = f64::from(a.z()) - f64::from(b.z());
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// Horizontal (XZ) distance ignoring `y`.
    ///
    /// Useful when vertical separation should not affect interest.
    #[must_use]
    pub fn horizontal_distance(a: Vec3, b: Vec3) -> f64 {
        let dx = f64::from(a.x()) - f64::from(b.x());
        let dz = f64::from(a.z()) - f64::from(b.z());
        (dx * dx + dz * dz).sqrt()
    }

    /// Hysteresis predicate for interest subscription (MRIB §7.5).
    ///
    /// - When `currently_subscribed` is false, the entity is subscribed only
    ///   when `distance <= near_radius` (30 m by default).
    /// - When `currently_subscribed` is true, the subscription is retained
    ///   while `distance <= unsubscribe_radius` (35 m by default).
    ///
    /// This prevents flapping at the boundary: an entity that enters at 30 m
    /// stays subscribed until it exceeds 35 m, and must re-enter to 30 m to
    /// resubscribe (acceptance condition 17).
    #[must_use]
    pub fn should_retain(self, distance: f64, currently_subscribed: bool) -> bool {
        if !distance.is_finite() {
            return false;
        }
        if currently_subscribed {
            distance <= self.unsubscribe_radius()
        } else {
            distance <= self.near_radius()
        }
    }

    /// Evaluates a [`VisibilityPolicy`] for a candidate entity.
    ///
    /// - [`VisibilityPolicy::Global`] is always visible (spatial condition
    ///   relaxed, MRIB §7.4).
    /// - [`VisibilityPolicy::Spatial`] is visible when `distance <= radius`.
    /// - [`VisibilityPolicy::OwnerOnly`] is visible only to the owner.
    ///
    /// `is_owner` should be true when the viewing subject is the entity owner.
    /// Non-finite distances are treated as not visible.
    #[must_use]
    pub fn is_visible_by_policy(
        self,
        policy: &VisibilityPolicy,
        distance: f64,
        viewer: ViewerContext<'_>,
    ) -> bool {
        if !distance.is_finite() {
            return false;
        }
        match policy {
            VisibilityPolicy::Global => true,
            VisibilityPolicy::Spatial { radius } => {
                let r = f64::from(*radius);
                if !r.is_finite() {
                    return false;
                }
                distance <= r
            }
            VisibilityPolicy::OwnerOnly => viewer.is_owner,
            VisibilityPolicy::RoleRestricted { roles } => viewer
                .roles
                .is_some_and(|viewer_roles| roles.iter().any(|role| viewer_roles.contains(role))),
            VisibilityPolicy::Explicit { users } => {
                viewer.user.is_some_and(|user| users.contains(&user))
            }
            VisibilityPolicy::Custom { .. } => false,
        }
    }

    /// Evaluates policies that can be decided without an entity position.
    #[must_use]
    pub fn is_visible_without_position(
        self,
        policy: &VisibilityPolicy,
        viewer: ViewerContext<'_>,
    ) -> bool {
        match policy {
            VisibilityPolicy::Global => true,
            VisibilityPolicy::OwnerOnly => viewer.is_owner,
            VisibilityPolicy::RoleRestricted { roles } => viewer
                .roles
                .is_some_and(|viewer_roles| roles.iter().any(|role| viewer_roles.contains(role))),
            VisibilityPolicy::Explicit { users } => {
                viewer.user.is_some_and(|user| users.contains(&user))
            }
            VisibilityPolicy::Spatial { .. } | VisibilityPolicy::Custom { .. } => false,
        }
    }

    /// Full interest predicate combining subscription hysteresis, the coarse
    /// cell filter, and the visibility policy (MRIB §7.4).
    ///
    /// Returns true only when the entity is both in the subscribed area
    /// (or `Global` relaxes that requirement) and permitted by policy.
    /// `currently_subscribed` is the previous hysteresis state for this
    /// viewer/entity pair.
    ///
    /// # `Global` bypasses every spatial check
    ///
    /// [`VisibilityPolicy::Global`] short-circuits before distance, cell and
    /// hysteresis are evaluated (MRIB §7.4, decision D-3). Use it **only** for
    /// entities that are meant to reach every member of the instance
    /// regardless of range — never for avatars or other per-player objects,
    /// which would disable interest management for the whole instance (H-6a).
    /// Spatially bounded entities must use
    /// [`VisibilityPolicy::Spatial`].
    #[must_use]
    pub fn is_entity_visible(
        self,
        viewer_pos: Vec3,
        entity_pos: Vec3,
        policy: &VisibilityPolicy,
        viewer: ViewerContext<'_>,
        currently_subscribed: bool,
    ) -> bool {
        // Global relaxes spatial condition.
        if matches!(policy, VisibilityPolicy::Global) {
            return true;
        }
        let distance = Self::euclidean_distance(viewer_pos, entity_pos);
        if !self.should_retain(distance, currently_subscribed) {
            return false;
        }
        if !self.is_in_subscribed_area(viewer_pos, entity_pos) {
            return false;
        }
        self.is_visible_by_policy(policy, distance, viewer)
    }
}

#[allow(clippy::derivable_impls)]
impl Default for UniformGrid {
    fn default() -> Self {
        Self {
            params: InterestParameters::default(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::{CellCoord, UniformGrid};
    use orbisync_domain::{VisibilityPolicy, transform::Vec3};
    use std::collections::BTreeSet;

    fn vec3(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3::new(x, y, z).expect("valid vec3")
    }

    fn context(is_owner: bool) -> super::ViewerContext<'static> {
        super::ViewerContext {
            user: None,
            roles: Some(Box::leak(Box::new(BTreeSet::new()))),
            is_owner,
        }
    }

    #[test]
    fn test_cell_assignment_positive() {
        let grid = UniformGrid::default();
        let pos = vec3(25.0, 0.0, 25.0);
        let cell = grid.cell_for(pos);
        assert_eq!(cell, CellCoord::new(1, 1));
    }

    #[test]
    fn test_cell_assignment_negative() {
        let grid = UniformGrid::default();
        let pos = vec3(-1.0, 0.0, -1.0);
        let cell = grid.cell_for(pos);
        assert_eq!(cell, CellCoord::new(-1, -1));
    }

    #[test]
    fn test_cell_assignment_boundary() {
        let grid = UniformGrid::default();
        let pos = vec3(20.0, 0.0, 20.0);
        assert_eq!(grid.cell_for(pos), CellCoord::new(1, 1));
        let pos2 = vec3(19.999, 0.0, 19.999);
        assert_eq!(grid.cell_for(pos2), CellCoord::new(0, 0));
    }

    #[test]
    fn test_subscribed_cells_is_3x3() {
        // Historically 3×3 (9 cells). Now 5×5 (25 cells) for default params.
        // Keep this test updated for 5×5 with radius = ceil(35/20)=2.
        let grid = UniformGrid::default();
        let viewer = vec3(0.0, 0.0, 0.0);
        let cells = grid.subscribed_cells(viewer);
        assert_eq!(grid.subscription_radius(), 2);
        assert_eq!(cells.len(), 25);
        let center = grid.cell_for(viewer);
        assert!(cells.contains(&center));
        for dz in -2..=2 {
            for dx in -2..=2 {
                let expected = CellCoord::new(center.x + dx, center.z + dz);
                assert!(cells.contains(&expected), "missing cell {expected:?}");
            }
        }
    }

    #[test]
    fn test_subscribed_cells_generic_radius() {
        // cell_size 10, unsubscribe 35 => radius ceil(35/10)=4 => 9×9 =81 cells
        let params = crate::InterestParameters::new(10.0, 30.0, 35.0).expect("valid");
        let grid = UniformGrid::new(params);
        assert_eq!(grid.subscription_radius(), 4);
        let viewer = vec3(0.0, 0.0, 0.0);
        let cells = grid.subscribed_cells(viewer);
        assert_eq!(cells.len(), 81);
        // cell_size 35, unsubscribe 40 => radius 2 (ceil 40/35=2) => 25 cells
        let params2 = crate::InterestParameters::new(35.0, 30.0, 40.0).expect("valid");
        let grid2 = UniformGrid::new(params2);
        assert_eq!(grid2.subscription_radius(), 2);
        assert_eq!(grid2.subscribed_cells(viewer).len(), 25);
        // cell_size 40, unsubscribe 35 => radius 1 => 3×3 =9 cells (covers)
        let params3 = crate::InterestParameters::new(40.0, 30.0, 35.0).expect("valid");
        let grid3 = UniformGrid::new(params3);
        assert_eq!(grid3.subscription_radius(), 1);
        assert_eq!(grid3.subscribed_cells(viewer).len(), 9);
    }

    #[test]
    fn test_subscribed_cells_covers_unsubscribe_radius_at_cell_edge() {
        // M-9 regression: viewer at cell edge 19.9, entity at 45 (dist 25.1)
        // must be in subscribed area with 5×5 but would be missed with 3×3.
        let grid = UniformGrid::default(); // cell 20, radius 2
        let viewer = vec3(19.9, 0.0, 0.0); // cell 0
        let entity = vec3(45.0, 0.0, 0.0); // cell 2
        assert!(grid.is_in_subscribed_area(viewer, entity));
        // 3×3 would exclude cell 2 when viewer in cell 0 (needs radius 1 only)
        let rad1_cells = {
            let center = grid.cell_for(viewer);
            let mut out = Vec::new();
            for dz in -1..=1 {
                for dx in -1..=1 {
                    out.push(CellCoord::new(center.x + dx, center.z + dz));
                }
            }
            out
        };
        assert!(!rad1_cells.contains(&grid.cell_for(entity)));
    }

    #[test]
    fn test_hysteresis_subscribe_unsubscribe() {
        let grid = UniformGrid::default();
        // MRIB §7.5: subscribe 30, unsubscribe 35
        // not subscribed -> 30 should subscribe
        assert!(grid.should_retain(30.0, false));
        assert!(!grid.should_retain(30.1, false));
        // subscribed -> 32 stays subscribed (hysteresis)
        assert!(grid.should_retain(32.0, true));
        // 36 unsubscribes
        assert!(!grid.should_retain(36.0, true));
        // after unsubscribed, 34 does not resubscribe (needs <=30)
        assert!(!grid.should_retain(34.0, false));
        // re-enter 30 resubscribes
        assert!(grid.should_retain(29.9, false));
    }

    #[test]
    fn test_visibility_global_always() {
        let grid = UniformGrid::default();
        let far = 10_000.0;
        assert!(grid.is_visible_by_policy(&VisibilityPolicy::Global, far, context(false)));
        assert!(grid.is_visible_by_policy(&VisibilityPolicy::Global, far, context(true)));
        assert!(grid.is_visible_by_policy(&VisibilityPolicy::Global, 0.0, context(false)));
    }

    #[test]
    fn test_visibility_spatial_radius() {
        let grid = UniformGrid::default();
        let policy = VisibilityPolicy::spatial(10.0).unwrap_or(VisibilityPolicy::Global);
        assert!(grid.is_visible_by_policy(&policy, 5.0, context(false)));
        assert!(grid.is_visible_by_policy(&policy, 10.0, context(false)));
        assert!(!grid.is_visible_by_policy(&policy, 10.1, context(false)));
    }

    #[test]
    fn test_visibility_owner_only() {
        let grid = UniformGrid::default();
        let policy = VisibilityPolicy::OwnerOnly;
        assert!(grid.is_visible_by_policy(&policy, 0.0, context(true)));
        assert!(!grid.is_visible_by_policy(&policy, 0.0, context(false)));
        assert!(!grid.is_visible_by_policy(&policy, 1.0, context(false)));
    }

    #[test]
    fn test_without_position_is_fail_closed_for_custom_and_missing_roles() {
        let grid = UniformGrid::default();
        let role = orbisync_domain::RoleId::generate();
        let user = orbisync_domain::UserId::generate();
        let roles = BTreeSet::from([role]);
        let role_policy = VisibilityPolicy::role_restricted([role]).expect("role policy");
        let explicit_policy = VisibilityPolicy::explicit([user]).expect("explicit policy");
        let custom_policy = VisibilityPolicy::custom("private.data").expect("custom policy");
        let owner = super::ViewerContext {
            user: Some(user),
            roles: Some(&roles),
            is_owner: true,
        };
        let no_roles = super::ViewerContext {
            user: Some(user),
            roles: None,
            is_owner: false,
        };

        assert!(grid.is_visible_without_position(&VisibilityPolicy::Global, no_roles));
        assert!(grid.is_visible_without_position(&VisibilityPolicy::OwnerOnly, owner));
        assert!(grid.is_visible_without_position(&role_policy, owner));
        assert!(!grid.is_visible_without_position(&role_policy, no_roles));
        assert!(grid.is_visible_without_position(&explicit_policy, no_roles));
        assert!(!grid.is_visible_without_position(&custom_policy, owner));
        assert!(!grid.is_visible_by_policy(&custom_policy, 0.0, owner));
    }

    #[test]
    fn test_role_explicit_and_custom_visibility() {
        let grid = UniformGrid::default();
        let role = orbisync_domain::RoleId::generate();
        let user = orbisync_domain::UserId::generate();
        let roles = [role].into_iter().collect::<BTreeSet<_>>();
        let viewer = super::ViewerContext {
            user: Some(user),
            roles: Some(&roles),
            is_owner: false,
        };
        let role_policy = VisibilityPolicy::role_restricted([role]).expect("role policy");
        let explicit_policy = VisibilityPolicy::explicit([user]).expect("explicit policy");
        let custom_policy = VisibilityPolicy::custom("com.example.private").expect("custom policy");
        assert!(grid.is_visible_by_policy(&role_policy, 0.0, viewer));
        assert!(grid.is_visible_by_policy(&explicit_policy, 0.0, viewer));
        assert!(!grid.is_visible_by_policy(&custom_policy, 0.0, viewer));
    }

    #[test]
    fn test_is_entity_visible_combines_filters() {
        let grid = UniformGrid::default();
        let viewer = vec3(0.0, 0.0, 0.0);
        let near = vec3(5.0, 0.0, 0.0);
        let far = vec3(200.0, 0.0, 0.0);
        let spatial = VisibilityPolicy::spatial(50.0).unwrap_or(VisibilityPolicy::Global);
        // near + spatial should be visible when not yet subscribed but within near_radius
        assert!(grid.is_entity_visible(viewer, near, &spatial, context(false), false));
        // far outside subscribed area and hysteresis -> not visible
        assert!(!grid.is_entity_visible(viewer, far, &spatial, context(false), false));
        // Global always visible regardless of distance / cell
        assert!(grid.is_entity_visible(
            viewer,
            far,
            &VisibilityPolicy::Global,
            context(false),
            false
        ));
        // OwnerOnly not owner -> not visible even if near
        assert!(!grid.is_entity_visible(
            viewer,
            near,
            &VisibilityPolicy::OwnerOnly,
            context(false),
            false
        ));
        assert!(grid.is_entity_visible(
            viewer,
            near,
            &VisibilityPolicy::OwnerOnly,
            context(true),
            false
        ));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_horizontal_distance_ignores_y() {
        let a = Vec3::new(0.0, 100.0, 0.0).expect("valid");
        let b = Vec3::new(3.0, -50.0, 4.0).expect("valid");
        let h = UniformGrid::horizontal_distance(a, b);
        assert!((h - 5.0).abs() < 1e-9);
        let e = UniformGrid::euclidean_distance(a, b);
        assert!(e > h);
    }
}
