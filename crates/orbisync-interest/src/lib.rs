//! Interest management.
//!
//! This crate computes which recipients receive which state updates. It is a
//! pure function of the spatial view: it performs no I/O and never touches a
//! connection queue, a socket or the gateway (`architecture.md` §3, MRIB §7.1).
//!
//! # Dependency rule
//!
//! `interest` depends on `domain` only (`repo-crate-conventions.md` §3.2) and
//! never on `realtime` (§8 acceptance condition 5).
//!
//! Milestone 5 implements the uniform grid itself; Milestone 0 fixes the
//! parameter type and its hysteresis invariant.

use orbisync_domain::{DomainError, DomainErrorKind};

/// Space-partitioning grid and subscription logic.
pub mod spatial_index;
/// Uniform grid cell assignment and interest predicates.
pub mod uniform_grid;

pub use spatial_index::SpatialIndex;
pub use uniform_grid::{CellCoord, UniformGrid, ViewerContext};

/// Validated parameters of the uniform grid strategy (MRIB §7.2, §7.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InterestParameters {
    cell_size: f64,
    near_radius: f64,
    unsubscribe_radius: f64,
}

impl InterestParameters {
    /// Validates and stores the grid parameters.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidValue`] when a value is not finite or
    /// not positive, or when `unsubscribe_radius` does not exceed
    /// `near_radius`. The gap between the two radii is the hysteresis that
    /// prevents subscribe and unsubscribe flapping at the boundary (MRIB §7.5).
    pub fn new(
        cell_size: f64,
        near_radius: f64,
        unsubscribe_radius: f64,
    ) -> Result<Self, DomainError> {
        for (label, value) in [
            ("cell_size", cell_size),
            ("near_radius", near_radius),
            ("unsubscribe_radius", unsubscribe_radius),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(DomainError::new(
                    DomainErrorKind::InvalidValue,
                    format!("{label} must be a finite value greater than 0"),
                ));
            }
        }
        if unsubscribe_radius <= near_radius {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "unsubscribe_radius must exceed near_radius to provide hysteresis",
            ));
        }
        // M-9: ensure subscribed cells cover unsubscribe_radius.
        // Required radius = ceil(unsubscribe_radius / cell_size), side = 2*radius+1.
        // For defaults 35/20=1.75 -> radius 2 -> 5×5, which guarantees
        // R*cell_size >= unsubscribe_radius even at cell edge.
        let radius = (unsubscribe_radius / cell_size).ceil() as i64;
        if radius < 1 || (radius as f64) * cell_size < unsubscribe_radius {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "cell_size too small or unsubscribe_radius too large: subscribed cells must cover unsubscribe_radius (need radius=ceil(unsubscribe/cell_size))",
            ));
        }
        // Guard against degenerate huge grids (e.g. cell_size 0.1 with 35 -> radius 350).
        // Limit radius to 10 (21×21) to keep candidate sets bounded.
        if radius > 10 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "unsubscribe_radius / cell_size too large: would require >21×21 subscribed cells",
            ));
        }
        Ok(Self {
            cell_size,
            near_radius,
            unsubscribe_radius,
        })
    }

    /// Returns the spatial grid cell size.
    #[must_use]
    pub const fn cell_size(self) -> f64 {
        self.cell_size
    }

    /// Returns the radius at which an entity becomes visible.
    #[must_use]
    pub const fn near_radius(self) -> f64 {
        self.near_radius
    }

    /// Returns the radius at which an entity is unsubscribed.
    #[must_use]
    pub const fn unsubscribe_radius(self) -> f64 {
        self.unsubscribe_radius
    }

    /// Returns the recommended default parameters (20.0 / 30.0 / 35.0).
    ///
    /// This mirrors `config/model.rs` and MRIB §7.5.
    #[must_use]
    pub const fn recommended() -> Self {
        Self {
            cell_size: 20.0,
            near_radius: 30.0,
            unsubscribe_radius: 35.0,
        }
    }
}

impl Default for InterestParameters {
    fn default() -> Self {
        Self::recommended()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::InterestParameters;
    use orbisync_domain::DomainErrorKind;

    #[test]
    fn test_recommended_parameters_are_accepted() {
        let parameters = InterestParameters::new(20.0, 30.0, 35.0).expect("MRIB defaults");
        assert!((parameters.cell_size() - 20.0).abs() < f64::EPSILON);
        assert!((parameters.unsubscribe_radius() - 35.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_missing_hysteresis_is_rejected() {
        let error = InterestParameters::new(20.0, 30.0, 30.0).expect_err("no hysteresis");
        assert_eq!(error.kind(), DomainErrorKind::InvalidValue);
    }

    #[test]
    fn test_non_finite_values_are_rejected() {
        assert_eq!(
            InterestParameters::new(f64::NAN, 30.0, 35.0)
                .expect_err("NaN must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
        assert_eq!(
            InterestParameters::new(20.0, f64::INFINITY, 35.0)
                .expect_err("infinity must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
    }

    #[test]
    fn test_default_matches_recommended() {
        let params = InterestParameters::default();
        assert!((params.cell_size() - 20.0).abs() < f64::EPSILON);
        assert!((params.near_radius() - 30.0).abs() < f64::EPSILON);
        assert!((params.unsubscribe_radius() - 35.0).abs() < f64::EPSILON);
        assert_eq!(params, InterestParameters::recommended());
    }

    #[test]
    fn test_recommended_is_valid() {
        let params = InterestParameters::recommended();
        let rebuilt = InterestParameters::new(
            params.cell_size(),
            params.near_radius(),
            params.unsubscribe_radius(),
        )
        .expect("recommended must be valid");
        assert_eq!(params, rebuilt);
    }
}
