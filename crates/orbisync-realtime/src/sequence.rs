//! Per-connection inbound sequence validation.
//!
//! The wire contract starts each direction at one.  An exact sequence is
//! consumed, an older sequence is a harmless replay, and a future sequence
//! means that a reliable frame was lost or delivered out of order.  The
//! tracker deliberately has no connection or socket dependencies so every
//! transport can apply the same state machine before rate limiting or actor
//! admission.

/// Result of validating one inbound sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundSequence {
    /// The frame has the next expected sequence and was consumed.
    Exact,
    /// The frame is older than the next expected sequence and must be dropped.
    Duplicate,
    /// The frame is ahead of the next expected sequence.
    Gap {
        /// Sequence that the receiver expected.
        expected: u64,
        /// Sequence received from the peer.
        received: u64,
    },
    /// The exact frame would advance the counter past `u64::MAX`.
    Overflow,
}

/// Tracks the next inbound sequence for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundSequenceTracker {
    expected: u64,
}

impl Default for InboundSequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl InboundSequenceTracker {
    /// The first sequence required by a new connection.
    pub const INITIAL: u64 = 1;

    /// Creates a tracker for a newly established connection.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            expected: Self::INITIAL,
        }
    }

    /// Creates a tracker at an explicit expected sequence (for boundary tests).
    #[must_use]
    pub const fn with_expected(expected: u64) -> Self {
        Self { expected }
    }

    /// Returns the next sequence required from the peer.
    #[must_use]
    pub const fn expected(&self) -> u64 {
        self.expected
    }

    /// Validates and, for an exact frame, consumes `received`.
    ///
    /// A sequence is never allowed to wrap.  Once `u64::MAX` is consumed the
    /// connection must be replaced; sequence continuity is connection-local,
    /// so reconnect starts a fresh tracker at one.
    pub fn accept(&mut self, received: u64) -> InboundSequence {
        match received.cmp(&self.expected) {
            core::cmp::Ordering::Less => InboundSequence::Duplicate,
            core::cmp::Ordering::Greater => InboundSequence::Gap {
                expected: self.expected,
                received,
            },
            core::cmp::Ordering::Equal => match self.expected.checked_add(1) {
                Some(next) => {
                    self.expected = next;
                    InboundSequence::Exact
                }
                None => InboundSequence::Overflow,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InboundSequence, InboundSequenceTracker};

    #[test]
    fn exact_sequences_advance_from_one() {
        let mut tracker = InboundSequenceTracker::new();
        assert_eq!(tracker.accept(1), InboundSequence::Exact);
        assert_eq!(tracker.expected(), 2);
        assert_eq!(tracker.accept(2), InboundSequence::Exact);
    }

    #[test]
    fn duplicates_do_not_advance() {
        let mut tracker = InboundSequenceTracker::with_expected(4);
        assert_eq!(tracker.accept(3), InboundSequence::Duplicate);
        assert_eq!(tracker.expected(), 4);
    }

    #[test]
    fn out_of_order_frames_are_protocol_gaps_and_do_not_advance() {
        let mut tracker = InboundSequenceTracker::new();
        assert_eq!(
            tracker.accept(3),
            InboundSequence::Gap {
                expected: 1,
                received: 3
            }
        );
        assert_eq!(tracker.expected(), 1);
    }

    #[test]
    fn reconnect_starts_a_fresh_connection_sequence() {
        let mut first = InboundSequenceTracker::new();
        assert_eq!(first.accept(1), InboundSequence::Exact);
        assert_eq!(first.accept(2), InboundSequence::Exact);

        let second = InboundSequenceTracker::new();
        assert_eq!(second.expected(), InboundSequenceTracker::INITIAL);
    }

    #[test]
    fn max_sequence_requires_reconnect() {
        let mut tracker = InboundSequenceTracker::with_expected(u64::MAX);
        assert_eq!(tracker.accept(u64::MAX), InboundSequence::Overflow);
        assert_eq!(tracker.expected(), u64::MAX);
    }
}
