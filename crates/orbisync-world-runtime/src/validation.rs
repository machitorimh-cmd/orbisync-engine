//! Input validation for instance runtime (spec §11.2, `state-and-runtime.md` §2.2).

use orbisync_domain::{Transform, transform::Vec3};

/// Maximum speed in units per second (spec §11.3 initial value).
pub const DEFAULT_MAX_SPEED: f64 = 50.0;
/// Default movement acceleration in units per second squared.
pub const DEFAULT_MAX_ACCELERATION: f64 = 10.0;

/// Maximum distance per tick at 20Hz (prevent teleport).
const MAX_TICK_DISTANCE: f64 = 10.0;

/// Lower bound for the elapsed time used in the speed check, in seconds.
///
/// Callers derive `delta_seconds` from wall-clock timestamps, which can be
/// zero (two updates inside the same millisecond) or negative (clock stepped
/// backwards). Both would make `distance / delta_seconds` meaningless, so the
/// caller clamps to this floor.
///
/// A *smaller* floor is *stricter*, not looser: at `0.001` s, `MAX_SPEED`
/// permits only `50.0 * 0.001 = 0.05` m of movement for a burst sender, far
/// below `MAX_TICK_DISTANCE`. The speed check is therefore the binding
/// constraint for rapid senders and no teleport slips through — a client
/// moving the full 10 m within one millisecond is rejected at
/// `10 / 0.001 = 10000` m/s (D-6).
///
/// Raising this floor would *weaken* the check, so it stays at 0.001.
pub const MIN_DELTA_SECONDS: f64 = 0.001;

/// Validates a transform update against server-authoritative rules.
///
/// Checks:
/// - finite, bounds and scale already validated by `Transform::new`
/// - speed limit between `previous` and `next` given `delta_seconds`
/// - per-tick distance limit
///
/// # Errors
///
/// Returns domain invalid-value when the update is not allowed.
pub fn validate_transform_update(
    previous: Option<Transform>,
    next: Transform,
    delta_seconds: f64,
) -> Result<(), orbisync_domain::DomainError> {
    validate_transform_update_with_limits(
        previous,
        next,
        delta_seconds,
        DEFAULT_MAX_SPEED,
        DEFAULT_MAX_ACCELERATION,
        true,
    )
}

/// Validates movement using configured speed and acceleration limits.
///
/// `speed_acceleration_check_enabled` gates only the speed/acceleration
/// comparison below (`world.speed_acceleration_check_enabled`, ADR-024).
/// `max_acceleration` has no standalone check of its own — it only widens the
/// speed ceiling for the interval — so there is nothing further to gate when
/// this flag is `false`. The per-tick teleport distance check, the
/// `delta_seconds` finiteness check, and the scale finiteness check are Core
/// invariants and are never affected by this flag.
pub fn validate_transform_update_with_limits(
    previous: Option<Transform>,
    next: Transform,
    delta_seconds: f64,
    max_speed: f64,
    max_acceleration: f64,
    speed_acceleration_check_enabled: bool,
) -> Result<(), orbisync_domain::DomainError> {
    if delta_seconds <= 0.0 || !delta_seconds.is_finite() {
        return Err(orbisync_domain::DomainError::new(
            orbisync_domain::DomainErrorKind::InvalidValue,
            "delta_seconds must be finite positive",
        ));
    }
    if let Some(prev) = previous {
        let dx = f64::from(next.position().x()) - f64::from(prev.position().x());
        let dy = f64::from(next.position().y()) - f64::from(prev.position().y());
        let dz = f64::from(next.position().z()) - f64::from(prev.position().z());
        let distance = (dx * dx + dy * dy + dz * dz).sqrt();
        if distance > MAX_TICK_DISTANCE {
            return Err(orbisync_domain::DomainError::new(
                orbisync_domain::DomainErrorKind::InvalidValue,
                "movement distance exceeds per-tick limit",
            ));
        }
        if speed_acceleration_check_enabled {
            let speed = distance / delta_seconds;
            let speed_limit = max_speed + max_acceleration * delta_seconds;
            if speed > speed_limit {
                return Err(orbisync_domain::DomainError::new(
                    orbisync_domain::DomainErrorKind::InvalidValue,
                    "speed exceeds limit",
                ));
            }
        }
        // Also validate scale wasn't smuggled via movement (scale is not updated via TransformInput in M2).
        // Previously `let _ =` ignored the Result, so an invalid scale (e.g. non-finite) was silently accepted.
        // Propagate the validation error so the update is rejected (A-3 fix).
        Vec3::new(next.scale().x(), next.scale().y(), next.scale().z())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_transform_update, validate_transform_update_with_limits};
    use orbisync_domain::{Quaternion, Vec3, transform::Transform};

    fn transform_at(x: f64) -> Transform {
        let pos = Vec3::new(x, 0.0, 0.0).expect("valid");
        let rot = Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = Vec3::new(1.0, 1.0, 1.0).expect("valid");
        Transform::new(pos, rot, scale).expect("valid")
    }

    #[test]
    fn allows_normal_movement() {
        let prev = transform_at(0.0);
        let next = transform_at(1.0);
        assert!(validate_transform_update(Some(prev), next, 0.1).is_ok());
    }

    #[test]
    fn rejects_teleport() {
        let prev = transform_at(0.0);
        let next = transform_at(100.0);
        assert!(validate_transform_update(Some(prev), next, 0.1).is_err());
    }

    /// D-6: at the clamp floor the speed limit — not the distance limit — is
    /// what rejects a burst sender. Raising `MIN_DELTA_SECONDS` above 0.2 or
    /// removing the speed check in `validate_transform_update` turns this red.
    #[test]
    fn speed_check_binds_at_min_delta() {
        use super::MIN_DELTA_SECONDS;
        let prev = transform_at(0.0);
        // 1 m in one millisecond is under MAX_TICK_DISTANCE (10 m) but is
        // 1000 m/s, twenty times MAX_SPEED.
        let next = transform_at(1.0);
        assert!(validate_transform_update(Some(prev), next, MIN_DELTA_SECONDS).is_err());
        // The same 1 m over a second violates neither limit.
        assert!(validate_transform_update(Some(prev), next, 1.0).is_ok());
    }

    /// A client sending at 5 Hz must not be rejected for ordinary movement.
    /// Guards against "fix" attempts that raise the floor to one server tick.
    #[test]
    fn low_rate_sender_is_not_rejected() {
        let prev = transform_at(0.0);
        let next = transform_at(3.0); // 3 m in 0.2 s = 15 m/s, under MAX_SPEED
        assert!(validate_transform_update(Some(prev), next, 0.2).is_ok());
    }

    /// world.speed_acceleration_check_enabled = false (ADR-024, GP-item1):
    /// a speed that would be rejected when the check is enabled is accepted
    /// when it is disabled, using the same inputs as `speed_check_binds_at_min_delta`.
    #[test]
    fn disabling_the_check_allows_speed_that_would_otherwise_be_rejected() {
        use super::MIN_DELTA_SECONDS;
        let prev = transform_at(0.0);
        let next = transform_at(1.0);
        assert!(
            validate_transform_update_with_limits(
                Some(prev),
                next,
                MIN_DELTA_SECONDS,
                super::DEFAULT_MAX_SPEED,
                super::DEFAULT_MAX_ACCELERATION,
                false,
            )
            .is_ok()
        );
    }

    /// Disabling the speed/acceleration check must not weaken the teleport
    /// distance check, `delta_seconds` finiteness, or scale finiteness — these
    /// are Core invariants, not use-case policy (ADR-024).
    #[test]
    fn disabling_the_check_still_rejects_teleport_and_invalid_delta() {
        let prev = transform_at(0.0);
        let teleport = transform_at(100.0);
        assert!(
            validate_transform_update_with_limits(
                Some(prev),
                teleport,
                0.1,
                super::DEFAULT_MAX_SPEED,
                super::DEFAULT_MAX_ACCELERATION,
                false,
            )
            .is_err()
        );
        let next = transform_at(1.0);
        assert!(
            validate_transform_update_with_limits(
                Some(prev),
                next,
                0.0,
                super::DEFAULT_MAX_SPEED,
                super::DEFAULT_MAX_ACCELERATION,
                false,
            )
            .is_err()
        );
    }
}
