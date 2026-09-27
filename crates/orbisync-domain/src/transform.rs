//! Transform value object used by world and entity placement.
//!
//! Coordinates use a right-handed, Y-up world. Quaternion components are
//! ordered `x, y, z, w`, and one world unit is one metre (specification
//! §9.7). The scalar representation is `f32` to match the public protocol.

use crate::error::{DomainError, DomainErrorKind};

fn invalid(detail: impl Into<String>) -> DomainError {
    DomainError::new(DomainErrorKind::InvalidValue, detail)
}

fn require_finite(value: f64, name: &str) -> Result<f32, DomainError> {
    if !value.is_finite() {
        return Err(invalid(format!("{name} must be finite")));
    }
    let value = value as f32;
    if !value.is_finite() {
        return Err(invalid(format!("{name} is outside f32 range")));
    }
    Ok(value)
}

/// 3-component `f32` vector in world units (1 unit = 1 meter).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Vec3 {
    /// Creates a vector, rejecting non-finite components.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when any component is NaN or infinite.
    pub fn new(
        x: impl Into<f64>,
        y: impl Into<f64>,
        z: impl Into<f64>,
    ) -> Result<Self, DomainError> {
        let x = require_finite(x.into(), "position.x")?;
        let y = require_finite(y.into(), "position.y")?;
        let z = require_finite(z.into(), "position.z")?;
        // World bounds: spec §9.7 / §11.3. Use a generous limit to prevent overflow
        // while allowing normal gameplay. Clamp to ±1_000_000 meters.
        const LIMIT: f32 = 1_000_000.0;
        if x.abs() > LIMIT || y.abs() > LIMIT || z.abs() > LIMIT {
            return Err(invalid("position component exceeds world bounds"));
        }
        Ok(Self { x, y, z })
    }

    /// Returns x.
    #[must_use]
    pub const fn x(self) -> f32 {
        self.x
    }

    /// Returns y.
    #[must_use]
    pub const fn y(self) -> f32 {
        self.y
    }

    /// Returns z.
    #[must_use]
    pub const fn z(self) -> f32 {
        self.z
    }
}

/// `f32` quaternion rotation with x,y,z,w ordering (spec §9.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quaternion {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

impl Quaternion {
    /// Creates a quaternion, rejecting non-finite components.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when any component is non-finite. The
    /// quaternion need not be normalized on input; normalization tolerance is
    /// checked separately where required.
    pub fn new(
        x: impl Into<f64>,
        y: impl Into<f64>,
        z: impl Into<f64>,
        w: impl Into<f64>,
    ) -> Result<Self, DomainError> {
        let x = require_finite(x.into(), "rotation.x")?;
        let y = require_finite(y.into(), "rotation.y")?;
        let z = require_finite(z.into(), "rotation.z")?;
        let w = require_finite(w.into(), "rotation.w")?;
        Ok(Self { x, y, z, w })
    }

    /// Returns x.
    #[must_use]
    pub const fn x(self) -> f32 {
        self.x
    }

    /// Returns y.
    #[must_use]
    pub const fn y(self) -> f32 {
        self.y
    }

    /// Returns z.
    #[must_use]
    pub const fn z(self) -> f32 {
        self.z
    }

    /// Returns w.
    #[must_use]
    pub const fn w(self) -> f32 {
        self.w
    }

    /// Returns true when the quaternion is approximately normalized.
    #[must_use]
    pub fn is_normalized(self) -> bool {
        let len_sq = self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w;
        (len_sq - 1.0).abs() < 1e-3
    }
}

/// Render-agnostic transform (spec §9.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    position: Vec3,
    rotation: Quaternion,
    scale: Vec3,
}

impl Transform {
    /// Creates a transform with position, rotation and scale.
    ///
    /// Scale is optional in the wire protocol (spec §9.7) but stored as a
    /// concrete value here; callers that do not send scale should use
    /// `Vec3::new(1.0, 1.0, 1.0)`.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when any component is non-finite or out
    /// of world bounds.
    pub fn new(position: Vec3, rotation: Quaternion, scale: Vec3) -> Result<Self, DomainError> {
        // Scale must be finite and positive; zero/negative scale is rejected.
        if scale.x <= 0.0 || scale.y <= 0.0 || scale.z <= 0.0 {
            return Err(invalid("scale components must be positive"));
        }
        // Scale magnitude cap to avoid degenerate transforms.
        const SCALE_LIMIT: f32 = 1_000.0;
        if scale.x > SCALE_LIMIT || scale.y > SCALE_LIMIT || scale.z > SCALE_LIMIT {
            return Err(invalid("scale component exceeds limit"));
        }
        Ok(Self {
            position,
            rotation,
            scale,
        })
    }

    /// Identity transform at origin.
    #[must_use]
    pub fn identity() -> Self {
        // SAFETY: constants are finite and within bounds.
        Self {
            position: Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            rotation: Quaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            scale: Vec3 {
                x: 1.0,
                y: 1.0,
                z: 1.0,
            },
        }
    }

    /// Returns position.
    #[must_use]
    pub const fn position(self) -> Vec3 {
        self.position
    }

    /// Returns rotation.
    #[must_use]
    pub const fn rotation(self) -> Quaternion {
        self.rotation
    }

    /// Returns scale.
    #[must_use]
    pub const fn scale(self) -> Vec3 {
        self.scale
    }
}

#[cfg(test)]
mod tests {
    use super::{Quaternion, Transform, Vec3};

    #[test]
    fn rejects_non_finite() {
        assert!(Vec3::new(f64::NAN, 0.0, 0.0).is_err());
        assert!(Vec3::new(f64::INFINITY, 0.0, 0.0).is_err());
        assert!(Quaternion::new(0.0, 0.0, f64::NAN, 1.0).is_err());
    }

    #[test]
    fn rejects_out_of_bounds() {
        assert!(Vec3::new(2_000_000.0, 0.0, 0.0).is_err());
    }

    #[test]
    fn rejects_non_positive_scale() {
        let pos = Vec3::new(0.0, 0.0, 0.0).expect("valid");
        let rot = Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let bad_scale = Vec3::new(0.0, 1.0, 1.0).expect("valid vec");
        assert!(Transform::new(pos, rot, bad_scale).is_err());
    }

    #[test]
    fn identity_is_valid() {
        let t = Transform::identity();
        assert_eq!(t.position().x(), 0.0);
        assert!(t.rotation().is_normalized());
    }

    #[test]
    fn coordinates_use_f32_storage() {
        let position = Vec3::new(1.0_f32 / 3.0, 0.0_f32, 0.0_f32).expect("valid position");
        let rotation = Quaternion::new(0.0_f32, 0.0_f32, 0.0_f32, 1.0_f32).expect("valid rotation");

        let position_x: f32 = position.x();
        let rotation_w: f32 = rotation.w();
        assert_eq!(position_x, 1.0_f32 / 3.0);
        assert_eq!(rotation_w, 1.0);
    }
}
