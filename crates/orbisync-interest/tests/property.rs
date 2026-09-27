//! Property tests for spatial grid cell assignment.

use orbisync_domain::Vec3;
use orbisync_interest::{CellCoord, InterestParameters, UniformGrid};
use proptest::prelude::*;

proptest! {
    #[test]
    fn spatial_grid_cell_contains_the_position(
        x in -1_000_000.0_f32..=1_000_000.0_f32,
        y in -1_000_000.0_f32..=1_000_000.0_f32,
        z in -1_000_000.0_f32..=1_000_000.0_f32,
    ) {
        let position = Vec3::new(x, y, z).expect("bounded position is valid");
        let grid = UniformGrid::default();
        let expected = CellCoord::new(
            (f64::from(position.x()) / grid.cell_size()).floor() as i64,
            (f64::from(position.z()) / grid.cell_size()).floor() as i64,
        );
        prop_assert_eq!(grid.cell_for(position), expected);
        prop_assert!(grid.is_in_subscribed_area(position, position));
    }

    #[test]
    fn coarse_membership_matches_enumerated_cells(
        x in -10_000.0_f32..=10_000.0_f32,
        z in -10_000.0_f32..=10_000.0_f32,
        dx in -120.0_f32..=120.0_f32,
        dz in -120.0_f32..=120.0_f32,
        radius in 1_u32..=10,
    ) {
        let grid = UniformGrid::new(
            InterestParameters::new(10.0, 1.0, f64::from(radius) * 10.0).unwrap(),
        );
        let viewer = Vec3::new(x, 0.0, z).unwrap();
        let entity = Vec3::new(x + dx, 100.0, z + dz).unwrap();
        prop_assert_eq!(
            grid.is_in_subscribed_area(viewer, entity),
            grid.subscribed_cells(viewer).contains(&grid.cell_for(entity)),
        );
    }
}
