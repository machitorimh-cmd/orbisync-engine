//! Property tests for domain transform validation.

use orbisync_domain::{Quaternion, Transform, Vec3};
use proptest::prelude::*;

proptest! {
    #[test]
    fn finite_bounded_transform_is_accepted(
        px in -1_000_000.0_f32..=1_000_000.0_f32,
        py in -1_000_000.0_f32..=1_000_000.0_f32,
        pz in -1_000_000.0_f32..=1_000_000.0_f32,
        rx in -1_000_000.0_f32..=1_000_000.0_f32,
        ry in -1_000_000.0_f32..=1_000_000.0_f32,
        rz in -1_000_000.0_f32..=1_000_000.0_f32,
        rw in -1_000_000.0_f32..=1_000_000.0_f32,
        sx in 0.001_f32..=1_000.0_f32,
        sy in 0.001_f32..=1_000.0_f32,
        sz in 0.001_f32..=1_000.0_f32,
    ) {
        let position = Vec3::new(px, py, pz);
        let rotation = Quaternion::new(rx, ry, rz, rw);
        prop_assert!(position.is_ok());
        prop_assert!(rotation.is_ok());

        let transform = Transform::new(
            position.expect("bounded position is valid"),
            rotation.expect("bounded rotation is valid"),
            Vec3::new(sx, sy, sz).expect("positive bounded scale is valid"),
        );
        prop_assert!(transform.is_ok());
    }

    #[test]
    fn non_finite_transform_component_is_rejected(
        component in 0usize..7,
        use_infinity in any::<bool>(),
    ) {
        let invalid = if use_infinity { f64::INFINITY } else { f64::NAN };
        let position = match component {
            0 => Vec3::new(invalid, 0.0, 0.0),
            1 => Vec3::new(0.0, invalid, 0.0),
            2 => Vec3::new(0.0, 0.0, invalid),
            _ => Vec3::new(0.0, 0.0, 0.0),
        };
        let rotation = match component {
            3 => Quaternion::new(invalid, 0.0, 0.0, 1.0),
            4 => Quaternion::new(0.0, invalid, 0.0, 1.0),
            5 => Quaternion::new(0.0, 0.0, invalid, 1.0),
            6 => Quaternion::new(0.0, 0.0, 0.0, invalid),
            _ => Quaternion::new(0.0, 0.0, 0.0, 1.0),
        };
        prop_assert!(position.is_err() || rotation.is_err());
    }
}
