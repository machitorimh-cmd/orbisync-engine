//! Per-connection rate limiting (`transport-boundaries.md` TB-06,
//! `mobile-resume-interest-backpressure.md` §8.4).
//!
//! A simple token-bucket limiter with separate buckets for normal traffic
//! (100 msg/s) and custom events (10 msg/s). The limiter is per-connection
//! and owns no I/O; the gateway calls it for each inbound frame and maps
//! a [`RateLimitError::PersistentRateLimit`] to
//! [`crate::state::ConnectionEvent::PersistentRateLimit`].

use orbisync_domain::Timestamp;

use crate::state::ConnectionEvent;

/// Category of an inbound message for rate-limit purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageCategory {
    /// Regular realtime traffic (transform, heartbeat, etc.).
    Normal,
    /// Custom/user-defined events, which are more expensive.
    Custom,
}

impl core::fmt::Display for MessageCategory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Normal => f.write_str("normal"),
            Self::Custom => f.write_str("custom"),
        }
    }
}

/// Error returned when a message exceeds the configured rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitError {
    /// A single message was rejected; caller should reply with an error and
    /// keep the connection alive.
    RateLimited {
        /// Category that was limited.
        category: MessageCategory,
    },
    /// Persistent abuse detected; caller should transition via
    /// `PersistentRateLimit -> Failing`.
    PersistentRateLimit {
        /// Category that triggered persistence.
        category: MessageCategory,
    },
}

impl core::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RateLimited { category } => write!(f, "rate limited ({category})"),
            Self::PersistentRateLimit { category } => {
                write!(f, "persistent rate limit ({category})")
            }
        }
    }
}

impl std::error::Error for RateLimitError {}

impl RateLimitError {
    /// Maps the error to the connection state machine event.
    ///
    /// Only the persistent variant drives `PersistentRateLimit -> failing`;
    /// the transient `RateLimited` is kept as an `ErrorMessage` without a
    /// state transition.
    #[must_use]
    pub const fn to_event(self) -> Option<ConnectionEvent> {
        match self {
            Self::PersistentRateLimit { .. } => Some(ConnectionEvent::PersistentRateLimit),
            Self::RateLimited { .. } => None,
        }
    }

    /// Returns `true` when the error is the persistent variant.
    #[must_use]
    pub const fn is_persistent(self) -> bool {
        matches!(self, Self::PersistentRateLimit { .. })
    }
}

/// Token bucket for a single category.
///
/// `capacity` is the burst size and `refill_per_second` is the long-term
/// rate. Tokens are refilled lazily on each `try_consume` call.
#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_second: f64,
    last_refill: Timestamp,
}

impl TokenBucket {
    /// Creates a bucket with the given capacity and refill rate.
    ///
    /// `now` is the initial `last_refill` timestamp; the bucket starts full.
    #[must_use]
    pub fn new(capacity: u32, refill_per_second: u32, now: Timestamp) -> Self {
        let cap = capacity as f64;
        Self {
            capacity: cap,
            tokens: cap,
            refill_per_second: refill_per_second as f64,
            last_refill: now,
        }
    }

    /// Creates a bucket with floating-point rates.
    #[must_use]
    pub fn new_with_rate(capacity: f64, refill_per_second: f64, now: Timestamp) -> Self {
        let cap = if capacity.is_finite() && capacity > 0.0 {
            capacity
        } else {
            0.0
        };
        let rate = if refill_per_second.is_finite() && refill_per_second > 0.0 {
            refill_per_second
        } else {
            0.0
        };
        Self {
            capacity: cap,
            tokens: cap,
            refill_per_second: rate,
            last_refill: now,
        }
    }

    /// Returns the current token count, refilling first at `now`.
    pub fn available(&mut self, now: Timestamp) -> f64 {
        self.refill(now);
        self.tokens
    }

    /// Tries to consume `amount` tokens at `now`.
    ///
    /// Returns `true` when the consumption succeeded.
    pub fn try_consume(&mut self, now: Timestamp, amount: f64) -> bool {
        self.refill(now);
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }

    /// Tries to consume a single token.
    pub fn try_consume_one(&mut self, now: Timestamp) -> bool {
        self.try_consume(now, 1.0)
    }

    fn refill(&mut self, now: Timestamp) {
        // Compute elapsed milliseconds using unix millis; this is deterministic
        // and avoids reading any hidden clock.
        let Ok(now_millis) = now.to_unix_millis() else {
            return;
        };
        let Ok(last_millis) = self.last_refill.to_unix_millis() else {
            return;
        };
        let elapsed_millis = now_millis.saturating_sub(last_millis);
        if elapsed_millis <= 0 {
            return;
        }
        let elapsed_seconds = elapsed_millis as f64 / 1000.0;
        let added = elapsed_seconds * self.refill_per_second;
        self.tokens = (self.tokens + added).min(self.capacity);
        self.last_refill = now;
    }
}

/// Per-connection rate limiter with separate buckets for normal and custom
/// traffic.
///
/// Limits follow `transport-boundaries.md` TB-06:
///
/// - Normal traffic: 100 messages/second (burst 100)
/// - Custom events: 10 messages/second (burst 10)
#[derive(Debug, Clone)]
pub struct RateLimiter {
    normal: TokenBucket,
    custom: TokenBucket,
    consecutive_denials: u32,
    persistent_threshold: u32,
}

impl RateLimiter {
    /// Normal category rate, messages per second.
    pub const DEFAULT_NORMAL_PER_SEC: u32 = 100;
    /// Custom category rate, messages per second.
    pub const DEFAULT_CUSTOM_PER_SEC: u32 = 10;
    /// How many consecutive denials constitute persistent abuse.
    pub const DEFAULT_PERSISTENT_THRESHOLD: u32 = 5;

    /// Creates a limiter with the default TB-06 rates at `now`.
    #[must_use]
    pub fn new(now: Timestamp) -> Self {
        Self::new_with_limits(
            Self::DEFAULT_NORMAL_PER_SEC,
            Self::DEFAULT_CUSTOM_PER_SEC,
            now,
        )
    }

    /// Creates a limiter with explicit per-second rates at `now`.
    #[must_use]
    pub fn new_with_limits(normal_per_sec: u32, custom_per_sec: u32, now: Timestamp) -> Self {
        Self {
            normal: TokenBucket::new(normal_per_sec, normal_per_sec, now),
            custom: TokenBucket::new(custom_per_sec, custom_per_sec, now),
            consecutive_denials: 0,
            persistent_threshold: Self::DEFAULT_PERSISTENT_THRESHOLD,
        }
    }

    /// Creates a limiter with explicit per-second rates and a custom
    /// persistence threshold.
    #[must_use]
    pub fn new_with_threshold(
        normal_per_sec: u32,
        custom_per_sec: u32,
        persistent_threshold: u32,
        now: Timestamp,
    ) -> Self {
        Self {
            normal: TokenBucket::new(normal_per_sec, normal_per_sec, now),
            custom: TokenBucket::new(custom_per_sec, custom_per_sec, now),
            consecutive_denials: 0,
            persistent_threshold,
        }
    }

    /// Checks whether a message of `category` is allowed at `now`.
    ///
    /// On success the token is consumed and the denial counter resets. On
    /// failure the denial counter grows and `PersistentRateLimit` is returned
    /// once the threshold is reached.
    pub fn check(
        &mut self,
        now: Timestamp,
        category: MessageCategory,
    ) -> Result<(), RateLimitError> {
        let allowed = match category {
            MessageCategory::Normal => self.normal.try_consume_one(now),
            MessageCategory::Custom => self.custom.try_consume_one(now),
        };
        if allowed {
            self.consecutive_denials = 0;
            Ok(())
        } else {
            self.consecutive_denials = self.consecutive_denials.saturating_add(1);
            if self.consecutive_denials >= self.persistent_threshold {
                Err(RateLimitError::PersistentRateLimit { category })
            } else {
                Err(RateLimitError::RateLimited { category })
            }
        }
    }

    /// Returns the number of consecutive denials so far.
    #[must_use]
    pub fn consecutive_denials(&self) -> u32 {
        self.consecutive_denials
    }

    /// Resets the denial counter without consuming a token.
    pub fn reset_denials(&mut self) {
        self.consecutive_denials = 0;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::{MessageCategory, RateLimitError, RateLimiter, TokenBucket};
    use crate::state::{ConnectionEvent, ConnectionState, transition};
    use orbisync_domain::Timestamp;

    fn ts(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis).expect("test timestamp is in range")
    }

    // -- TokenBucket ---------------------------------------------------------

    #[test]
    fn test_token_bucket_starts_full() {
        let mut bucket = TokenBucket::new(10, 10, ts(0));
        assert!((bucket.available(ts(0)) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_token_bucket_consumes_and_rejects_when_empty() {
        let mut bucket = TokenBucket::new(2, 100, ts(0));
        assert!(bucket.try_consume_one(ts(0)));
        assert!(bucket.try_consume_one(ts(0)));
        assert!(!bucket.try_consume_one(ts(0)));
    }

    #[test]
    fn test_token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(10, 10, ts(0));
        // Drain
        for _ in 0..10 {
            assert!(bucket.try_consume_one(ts(0)));
        }
        assert!(!bucket.try_consume_one(ts(0)));
        // 100 ms -> 1 token refilled
        assert!(bucket.try_consume_one(ts(100)));
        assert!(!bucket.try_consume_one(ts(100)));
        // Wait 1 s -> 10 tokens refilled (capped)
        assert!(bucket.try_consume_one(ts(1_100)));
        // Drain again 9 more should succeed at 1_100 (bucket had 9 left after one consume)
        for _ in 0..9 {
            assert!(bucket.try_consume_one(ts(1_100)));
        }
        assert!(!bucket.try_consume_one(ts(1_100)));
    }

    #[test]
    fn test_token_bucket_does_not_refill_for_backward_clock() {
        let mut bucket = TokenBucket::new(1, 100, ts(1000));
        assert!(bucket.try_consume_one(ts(1000)));
        assert!(!bucket.try_consume_one(ts(1000)));
        // Clock goes backward - no refill
        assert!(!bucket.try_consume_one(ts(500)));
    }

    #[test]
    fn test_token_bucket_capped_at_capacity() {
        let mut bucket = TokenBucket::new(5, 100, ts(0));
        assert!(bucket.try_consume_one(ts(0)));
        // Wait long enough to refill more than capacity
        assert!(bucket.try_consume_one(ts(10_000)));
        // Should have 5 capacity, we consumed 2, so 4 remain? Actually after 10s,
        // bucket refills to 5 then we consume 1 -> 4 left. Consume 4 -> empty.
        for _ in 0..4 {
            assert!(bucket.try_consume_one(ts(10_000)));
        }
        assert!(!bucket.try_consume_one(ts(10_000)));
    }

    // -- RateLimiter ---------------------------------------------------------

    #[test]
    fn test_rate_limiter_allows_burst_normal() {
        let mut limiter = RateLimiter::new(ts(0));
        for _ in 0..100 {
            assert_eq!(limiter.check(ts(0), MessageCategory::Normal), Ok(()));
        }
        // 101st at same instant should be limited
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Normal),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Normal
            })
        );
    }

    #[test]
    fn configured_normal_rate_changes_rejection_boundary() {
        let mut limiter = RateLimiter::new_with_limits(2, 10, ts(0));
        assert_eq!(limiter.check(ts(0), MessageCategory::Normal), Ok(()));
        assert_eq!(limiter.check(ts(0), MessageCategory::Normal), Ok(()));
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Normal),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Normal
            })
        );
    }

    #[test]
    fn test_rate_limiter_allows_burst_custom() {
        let mut limiter = RateLimiter::new(ts(0));
        for _ in 0..10 {
            assert_eq!(limiter.check(ts(0), MessageCategory::Custom), Ok(()));
        }
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Custom),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Custom
            })
        );
    }

    #[test]
    fn test_rate_limiter_refills_normal_after_one_second() {
        let mut limiter = RateLimiter::new(ts(0));
        for _ in 0..100 {
            limiter
                .check(ts(0), MessageCategory::Normal)
                .expect("burst should succeed");
        }
        // One denial
        assert!(limiter.check(ts(0), MessageCategory::Normal).is_err());
        // After 1 second, full refill
        assert_eq!(limiter.check(ts(1000), MessageCategory::Normal), Ok(()));
    }

    #[test]
    fn test_rate_limiter_custom_refill_is_slower() {
        let mut limiter = RateLimiter::new(ts(0));
        for _ in 0..10 {
            limiter
                .check(ts(0), MessageCategory::Custom)
                .expect("burst should succeed");
        }
        // At 500ms only ~5 tokens refilled
        let mut successes = 0;
        for _ in 0..10 {
            if limiter.check(ts(500), MessageCategory::Custom).is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 5);
    }

    #[test]
    fn test_rate_limiter_persistent_after_threshold() {
        let mut limiter = RateLimiter::new_with_threshold(1, 1, 3, ts(0));
        // Capacity 1, so first succeeds
        assert_eq!(limiter.check(ts(0), MessageCategory::Normal), Ok(()));
        // Next 3 should progress RateLimited -> RateLimited -> Persistent
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Normal),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Normal
            })
        );
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Normal),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Normal
            })
        );
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Normal),
            Err(RateLimitError::PersistentRateLimit {
                category: MessageCategory::Normal
            })
        );
        assert!(
            limiter
                .check(ts(0), MessageCategory::Normal)
                .unwrap_err()
                .is_persistent()
        );
    }

    #[test]
    fn test_rate_limiter_resets_denials_on_success() {
        let mut limiter = RateLimiter::new_with_threshold(1, 1, 2, ts(0));
        assert_eq!(limiter.check(ts(0), MessageCategory::Normal), Ok(()));
        assert_eq!(
            limiter.check(ts(0), MessageCategory::Normal),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Normal
            })
        );
        assert_eq!(limiter.consecutive_denials(), 1);
        // After 1s refill, success resets
        assert_eq!(limiter.check(ts(1000), MessageCategory::Normal), Ok(()));
        assert_eq!(limiter.consecutive_denials(), 0);
        // Again denial should be RateLimited, not yet persistent
        assert_eq!(
            limiter.check(ts(1000), MessageCategory::Normal),
            Err(RateLimitError::RateLimited {
                category: MessageCategory::Normal
            })
        );
    }

    #[test]
    fn test_rate_limiter_persistent_maps_to_connection_event() {
        let mut limiter = RateLimiter::new_with_threshold(1, 1, 1, ts(0));
        limiter
            .check(ts(0), MessageCategory::Normal)
            .expect("first ok");
        let err = limiter
            .check(ts(0), MessageCategory::Normal)
            .expect_err("should be limited");
        assert_eq!(err.to_event(), Some(ConnectionEvent::PersistentRateLimit));
        let event = err.to_event().expect("persistent has event");
        assert_eq!(
            transition(ConnectionState::Active, event),
            Some(ConnectionState::Failing)
        );
        // Transient does not map to event
        let mut limiter2 = RateLimiter::new_with_threshold(1, 1, 5, ts(0));
        limiter2
            .check(ts(0), MessageCategory::Normal)
            .expect("first ok");
        let transient = limiter2
            .check(ts(0), MessageCategory::Normal)
            .expect_err("transient");
        assert_eq!(transient.to_event(), None);
    }

    #[test]
    fn test_rate_limiter_custom_persistent_maps_to_failing() {
        let mut limiter = RateLimiter::new_with_threshold(1, 1, 1, ts(0));
        limiter
            .check(ts(0), MessageCategory::Custom)
            .expect("first custom ok");
        let err = limiter
            .check(ts(0), MessageCategory::Custom)
            .expect_err("second custom limited");
        assert_eq!(err.to_event(), Some(ConnectionEvent::PersistentRateLimit));
        assert_eq!(
            transition(ConnectionState::Ready, ConnectionEvent::PersistentRateLimit),
            Some(ConnectionState::Failing)
        );
    }

    #[test]
    fn test_rate_limiter_normal_and_custom_are_independent() {
        let mut limiter = RateLimiter::new(ts(0));
        for _ in 0..10 {
            limiter
                .check(ts(0), MessageCategory::Custom)
                .expect("custom burst");
        }
        // Custom exhausted, normal still full
        assert_eq!(limiter.check(ts(0), MessageCategory::Normal), Ok(()));
        // Drain normal
        for _ in 0..99 {
            limiter
                .check(ts(0), MessageCategory::Normal)
                .expect("normal remaining");
        }
        assert!(limiter.check(ts(0), MessageCategory::Normal).is_err());
    }

    #[test]
    fn test_rate_limiter_display() {
        let err = RateLimitError::PersistentRateLimit {
            category: MessageCategory::Custom,
        };
        assert!(err.to_string().contains("persistent"));
        let transient = RateLimitError::RateLimited {
            category: MessageCategory::Normal,
        };
        assert!(transient.to_string().contains("normal"));
    }
}
