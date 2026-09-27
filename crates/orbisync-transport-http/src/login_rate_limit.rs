//! Per-source-IP rate limiting for `POST /v1/auth/login`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use time::{Duration, OffsetDateTime};

#[derive(Debug)]
struct Bucket {
    created_at: OffsetDateTime,
    attempts: u32,
    blocked_until: Option<OffsetDateTime>,
}

/// The result of a login rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginRateLimitDecision {
    /// Whether the login handler may continue.
    pub allowed: bool,
    /// Seconds until a blocked source may try again, when blocked.
    pub retry_after_seconds: Option<u64>,
}

/// Bounded per-source-IP login limiter.
///
/// Every request that reaches the login handler consumes one attempt, whether
/// authentication succeeds or fails. Once the per-minute limit is exceeded,
/// the source is blocked for the configured duration. When the bucket bound is
/// reached, the oldest bucket is evicted before a new source is inserted.
#[derive(Debug)]
pub struct LoginRateLimiter {
    max_attempts_per_window: u32,
    window: Duration,
    block_duration: Duration,
    max_buckets: usize,
    buckets: Mutex<HashMap<Option<IpAddr>, Bucket>>,
}

impl LoginRateLimiter {
    /// Creates a limiter with explicit limits.
    #[must_use]
    pub fn new(max_attempts_per_window: u32, block_seconds: u64, max_buckets: usize) -> Self {
        assert!(
            max_attempts_per_window > 0,
            "login rate limit must be positive"
        );
        assert!(
            block_seconds > 0,
            "login IP block duration must be positive"
        );
        assert!(max_buckets > 0, "login IP bucket cap must be positive");
        let block_seconds = i64::try_from(block_seconds).map_or(i64::MAX, |value| value);
        Self {
            max_attempts_per_window,
            window: Duration::minutes(1),
            block_duration: Duration::seconds(block_seconds),
            max_buckets,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Creates the production defaults from the A-4 specification.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(10, 300, 10_000)
    }

    /// Checks and records one login attempt for `source_ip`.
    pub fn check_and_record(
        &self,
        source_ip: Option<IpAddr>,
        now: OffsetDateTime,
    ) -> LoginRateLimitDecision {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buckets.retain(|_, bucket| {
            bucket.blocked_until.is_some_and(|until| until > now)
                || now - bucket.created_at < self.window
        });

        if let Some(bucket) = buckets.get_mut(&source_ip) {
            if let Some(blocked_until) = bucket.blocked_until {
                if blocked_until > now {
                    return LoginRateLimitDecision {
                        allowed: false,
                        retry_after_seconds: Some(retry_after_seconds(now, blocked_until)),
                    };
                }
                bucket.blocked_until = None;
                bucket.attempts = 0;
            }

            if now - bucket.created_at >= self.window {
                bucket.created_at = now;
                bucket.attempts = 0;
            }

            if bucket.attempts >= self.max_attempts_per_window {
                bucket.attempts = bucket.attempts.saturating_add(1);
                bucket.blocked_until = Some(now + self.block_duration);
                return LoginRateLimitDecision {
                    allowed: false,
                    retry_after_seconds: Some(self.block_duration.whole_seconds() as u64),
                };
            }

            bucket.attempts = bucket.attempts.saturating_add(1);
            return LoginRateLimitDecision {
                allowed: true,
                retry_after_seconds: None,
            };
        }

        if buckets.len() >= self.max_buckets {
            let oldest = buckets
                .iter()
                .min_by_key(|(_, bucket)| bucket.created_at)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                buckets.remove(&oldest);
            }
        }

        buckets.insert(
            source_ip,
            Bucket {
                created_at: now,
                attempts: 1,
                blocked_until: None,
            },
        );
        LoginRateLimitDecision {
            allowed: true,
            retry_after_seconds: None,
        }
    }

    /// Removes buckets whose one-minute window is no longer active.
    pub fn sweep(&self, now: OffsetDateTime) {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buckets.retain(|_, bucket| {
            bucket.blocked_until.is_some_and(|until| until > now)
                || now - bucket.created_at < self.window
        });
    }

    /// Returns the number of retained source buckets.
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    #[cfg(test)]
    fn contains_ip(&self, source_ip: Option<IpAddr>) -> bool {
        self.buckets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&source_ip)
    }
}

fn retry_after_seconds(now: OffsetDateTime, until: OffsetDateTime) -> u64 {
    let duration = until - now;
    let whole = duration.whole_seconds().max(0);
    let has_fraction = duration.whole_nanoseconds() % 1_000_000_000 != 0;
    u64::try_from(whole)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(has_fraction))
}

#[cfg(test)]
mod tests {
    use super::LoginRateLimiter;
    use std::net::{IpAddr, Ipv4Addr};
    use time::OffsetDateTime;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp")
    }

    fn ip(last: u8) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)))
    }

    #[test]
    fn blocks_the_attempt_after_the_configured_limit() {
        let limiter = LoginRateLimiter::new(2, 300, 10);
        let t = now();
        assert!(limiter.check_and_record(ip(1), t).allowed);
        assert!(limiter.check_and_record(ip(1), t).allowed);
        let blocked = limiter.check_and_record(ip(1), t);
        assert!(!blocked.allowed);
        assert_eq!(blocked.retry_after_seconds, Some(300));
    }

    #[test]
    fn blocked_source_recovers_after_block_and_window_expiry() {
        let limiter = LoginRateLimiter::new(1, 5, 10);
        let t = now();
        assert!(limiter.check_and_record(ip(1), t).allowed);
        assert!(!limiter.check_and_record(ip(1), t).allowed);
        assert!(
            limiter
                .check_and_record(ip(1), t + time::Duration::seconds(6))
                .allowed
        );
    }

    #[test]
    fn another_source_is_not_affected() {
        let limiter = LoginRateLimiter::new(1, 300, 10);
        let t = now();
        assert!(limiter.check_and_record(ip(1), t).allowed);
        assert!(!limiter.check_and_record(ip(1), t).allowed);
        assert!(limiter.check_and_record(ip(2), t).allowed);
    }

    #[test]
    fn expired_empty_bucket_is_removed() {
        let limiter = LoginRateLimiter::new(2, 300, 10);
        let t = now();
        assert!(limiter.check_and_record(ip(1), t).allowed);
        assert_eq!(limiter.bucket_count(), 1);
        limiter.sweep(t + time::Duration::seconds(61));
        assert_eq!(limiter.bucket_count(), 0);

        assert!(limiter.check_and_record(ip(1), t).allowed);
        assert!(
            limiter
                .check_and_record(ip(2), t + time::Duration::seconds(61))
                .allowed
        );
        assert_eq!(limiter.bucket_count(), 1);
    }

    #[test]
    fn bucket_cap_evicts_the_oldest_bucket() {
        let limiter = LoginRateLimiter::new(10, 300, 2);
        let t = now();
        assert!(limiter.check_and_record(ip(1), t).allowed);
        assert!(
            limiter
                .check_and_record(ip(2), t + time::Duration::seconds(1))
                .allowed
        );
        assert!(
            limiter
                .check_and_record(ip(1), t + time::Duration::seconds(2))
                .allowed
        );
        assert!(
            limiter
                .check_and_record(ip(3), t + time::Duration::seconds(3))
                .allowed
        );
        assert_eq!(limiter.bucket_count(), 2);
        assert!(!limiter.contains_ip(ip(1)));
        assert!(limiter.contains_ip(ip(2)));
        assert!(limiter.contains_ip(ip(3)));
    }

    #[test]
    fn missing_peer_uses_one_fail_closed_bucket() {
        let limiter = LoginRateLimiter::new(1, 300, 10);
        let t = now();
        assert!(limiter.check_and_record(None, t).allowed);
        assert!(!limiter.check_and_record(None, t).allowed);
    }
}
