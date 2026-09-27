//! Clock abstraction.
//!
//! Production code never calls the system clock directly. It takes a [`Clock`]
//! so tests can inject a deterministic implementation (specification §31.4,
//! `architecture.md` §9, TD-08). `orbisync-testkit` provides the fixed clock
//! used by unit tests.

use time::OffsetDateTime;

use crate::time::Timestamp;

/// Source of the current UTC time.
///
/// Implementations must be cheap to call and free of blocking I/O; the realtime
/// tick loop reads the clock on every iteration.
pub trait Clock: Send + Sync + 'static {
    /// Returns the current instant in UTC.
    fn now(&self) -> Timestamp;
}

/// [`Clock`] backed by the operating system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl SystemClock {
    /// Creates a system clock.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_offset_date_time(OffsetDateTime::now_utc())
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, SystemClock};

    #[test]
    fn test_system_clock_is_monotonic_across_two_reads() {
        let clock = SystemClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }

    #[test]
    fn test_clock_is_object_safe() {
        let clock: Box<dyn Clock> = Box::new(SystemClock::new());
        assert!(clock.now().to_unix_millis().expect("fits in i64") > 0);
    }
}
