//! Revision counter.
//!
//! `Revision` stays a `u64` rather than a UUID (ADR-001) and is persisted as
//! `BIGINT` (ADR-005). It is monotonic per aggregate and is used for optimistic
//! concurrency and for realtime delta ordering.

use core::fmt;

use crate::error::{DomainError, DomainErrorKind};

/// Monotonic revision of an aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Revision(u64);

impl Revision {
    /// Revision of a freshly created aggregate.
    pub const INITIAL: Self = Self(0);

    /// Wraps a raw counter value read from persistence or the wire.
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw counter value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns the next revision.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::RevisionOverflow`] when the counter reaches
    /// `u64::MAX`. Callers must not wrap around: a wrapped revision would make
    /// deltas apply out of order.
    pub fn next(self) -> Result<Self, DomainError> {
        self.0.checked_add(1).map(Self).ok_or_else(|| {
            DomainError::new(
                DomainErrorKind::RevisionOverflow,
                "revision counter reached u64::MAX",
            )
        })
    }

    /// Checks an optimistic concurrency precondition.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::RevisionMismatch`] when `expected` is not the
    /// current revision.
    pub fn ensure_matches(self, expected: Self) -> Result<(), DomainError> {
        if self == expected {
            Ok(())
        } else {
            Err(DomainError::new(
                DomainErrorKind::RevisionMismatch,
                format!("expected revision {expected}, found {self}"),
            ))
        }
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::Revision;
    use crate::error::DomainErrorKind;

    #[test]
    fn test_initial_revision_is_zero() {
        assert_eq!(Revision::INITIAL.as_u64(), 0);
        assert_eq!(Revision::default(), Revision::INITIAL);
    }

    #[test]
    fn test_next_increments_monotonically() {
        let first = Revision::INITIAL.next().expect("no overflow");
        let second = first.next().expect("no overflow");
        assert_eq!(first.as_u64(), 1);
        assert!(second > first);
    }

    #[test]
    fn test_next_rejects_overflow() {
        let error = Revision::from_u64(u64::MAX)
            .next()
            .expect_err("must not wrap");
        assert_eq!(error.kind(), DomainErrorKind::RevisionOverflow);
    }

    #[test]
    fn test_ensure_matches_detects_mismatch() {
        let current = Revision::from_u64(7);
        current
            .ensure_matches(Revision::from_u64(7))
            .expect("equal revisions match");
        let error = current
            .ensure_matches(Revision::from_u64(6))
            .expect_err("different revisions must not match");
        assert_eq!(error.kind(), DomainErrorKind::RevisionMismatch);
    }
}
