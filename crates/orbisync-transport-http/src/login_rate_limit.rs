//! Per-source-IP rate limiting for `POST /v1/auth/login`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use orbisync_application::metrics::RateLimitScope;
use orbisync_domain::AuthMethod;
use time::{Duration, OffsetDateTime};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Initial authentication admission policy, shared by all four methods.
/// Counters and concurrency are process-local; deployment-wide budgets must
/// divide the desired total across replicas or be enforced at the ingress.
#[derive(Debug, Clone, Copy)]
pub struct AuthIssuanceLimits {
    /// Attempts per source IP across all initial authentication methods.
    pub per_ip_per_minute: u32,
    /// Attempts per IP for local, guest, name-only and external, in that order.
    pub per_method_per_minute: [u32; 4],
    /// Maximum simultaneous initial authentication operations.
    pub max_concurrent: u32,
    /// Total guest/name-only attempts per minute, including failed attempts.
    pub temporary_subjects_per_minute: u32,
}

impl Default for AuthIssuanceLimits {
    fn default() -> Self {
        Self {
            per_ip_per_minute: 60,
            per_method_per_minute: [10, 30, 30, 30],
            max_concurrent: 32,
            temporary_subjects_per_minute: 1_000,
        }
    }
}

#[derive(Debug)]
struct IssuanceBucket {
    started: OffsetDateTime,
    total: u32,
    methods: [u32; 4],
}

#[derive(Debug, Default)]
struct IssuanceCounters {
    ips: HashMap<Option<IpAddr>, IssuanceBucket>,
    temporary: Option<(OffsetDateTime, u32)>,
}

/// Rejected admission, with bounded metrics scope and a positive retry delay.
#[derive(Debug, Clone, Copy)]
pub struct AuthIssuanceRejection {
    /// Rejection category; never contains user, IP or token labels.
    pub scope: RateLimitScope,
    /// Seconds before retrying may succeed.
    pub retry_after_seconds: u64,
}

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
    issuance_limits: AuthIssuanceLimits,
    issuance_slots: Arc<Semaphore>,
    issuance_counters: Mutex<IssuanceCounters>,
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
            issuance_limits: AuthIssuanceLimits::default(),
            issuance_slots: Arc::new(Semaphore::new(
                AuthIssuanceLimits::default().max_concurrent as usize,
            )),
            issuance_counters: Mutex::new(IssuanceCounters::default()),
        }
    }

    /// Configures shared initial-authentication admission before serving requests.
    #[must_use]
    pub fn with_issuance_limits(mut self, limits: AuthIssuanceLimits) -> Self {
        assert!(
            limits.per_ip_per_minute > 0 && limits.per_method_per_minute.iter().all(|v| *v > 0)
        );
        assert!(
            (1..=65_536).contains(&limits.max_concurrent)
                && limits.temporary_subjects_per_minute > 0
        );
        self.issuance_slots = Arc::new(Semaphore::new(limits.max_concurrent as usize));
        self.issuance_limits = limits;
        self
    }

    /// Admits one complete initial-authentication operation. Hold the returned
    /// permit until all cryptographic and database work has finished. Unknown
    /// peers share one bucket. A full IP table rejects new sources instead of
    /// evicting live counters (which would let source rotation bypass limits).
    pub fn acquire_issuance(
        &self,
        method: AuthMethod,
        source_ip: Option<IpAddr>,
        now: OffsetDateTime,
    ) -> Result<OwnedSemaphorePermit, AuthIssuanceRejection> {
        let permit = Arc::clone(&self.issuance_slots)
            .try_acquire_owned()
            .map_err(|_| AuthIssuanceRejection {
                scope: RateLimitScope::AuthConcurrency,
                retry_after_seconds: 1,
            })?;
        let mut counters = self
            .issuance_counters
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let window = Duration::minutes(1);
        counters
            .ips
            .retain(|_, bucket| now - bucket.started < window);
        let index = match method {
            AuthMethod::Local => 0,
            AuthMethod::Guest => 1,
            AuthMethod::NameOnly => 2,
            AuthMethod::External => 3,
        };
        if !counters.ips.contains_key(&source_ip) && counters.ips.len() >= self.max_buckets {
            let retry = counters
                .ips
                .values()
                .map(|b| retry_after_seconds(now, b.started + window))
                .min()
                .unwrap_or(1)
                .max(1);
            return Err(AuthIssuanceRejection {
                scope: RateLimitScope::Ip,
                retry_after_seconds: retry,
            });
        }
        if let Some(bucket) = counters.ips.get(&source_ip) {
            let retry = retry_after_seconds(now, bucket.started + window).max(1);
            if bucket.total >= self.issuance_limits.per_ip_per_minute {
                return Err(AuthIssuanceRejection {
                    scope: RateLimitScope::Ip,
                    retry_after_seconds: retry,
                });
            }
            if bucket.methods[index] >= self.issuance_limits.per_method_per_minute[index] {
                return Err(AuthIssuanceRejection {
                    scope: RateLimitScope::AuthMethod,
                    retry_after_seconds: retry,
                });
            }
        }
        if matches!(method, AuthMethod::Guest | AuthMethod::NameOnly) {
            let (started, count) = counters.temporary.get_or_insert((now, 0));
            if now - *started >= window {
                *started = now;
                *count = 0;
            }
            if *count >= self.issuance_limits.temporary_subjects_per_minute {
                return Err(AuthIssuanceRejection {
                    scope: RateLimitScope::TemporarySubjectBudget,
                    retry_after_seconds: retry_after_seconds(now, *started + window).max(1),
                });
            }
            *count += 1;
        }
        let bucket = counters.ips.entry(source_ip).or_insert(IssuanceBucket {
            started: now,
            total: 0,
            methods: [0; 4],
        });
        bucket.total += 1;
        bucket.methods[index] += 1;
        Ok(permit)
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

    fn issuance_limits() -> super::AuthIssuanceLimits {
        super::AuthIssuanceLimits {
            per_ip_per_minute: 3,
            per_method_per_minute: [2; 4],
            max_concurrent: 2,
            temporary_subjects_per_minute: 3,
        }
    }

    #[test]
    fn issuance_method_ip_and_global_temporary_limits_are_independent() {
        use orbisync_application::metrics::RateLimitScope;
        use orbisync_domain::AuthMethod::{External, Guest, Local, NameOnly};
        let limiter = LoginRateLimiter::new(100, 300, 10).with_issuance_limits(issuance_limits());
        let t = now();
        drop(limiter.acquire_issuance(Guest, ip(1), t).unwrap());
        drop(limiter.acquire_issuance(Guest, ip(1), t).unwrap());
        let rejected = limiter.acquire_issuance(Guest, ip(1), t).unwrap_err();
        assert_eq!(rejected.scope, RateLimitScope::AuthMethod);
        assert_eq!(rejected.retry_after_seconds, 60);
        drop(limiter.acquire_issuance(External, ip(1), t).unwrap());
        assert_eq!(
            limiter.acquire_issuance(Local, ip(1), t).unwrap_err().scope,
            RateLimitScope::Ip
        );
        drop(limiter.acquire_issuance(NameOnly, ip(2), t).unwrap());
        assert_eq!(
            limiter.acquire_issuance(Guest, ip(3), t).unwrap_err().scope,
            RateLimitScope::TemporarySubjectBudget
        );
        // A guest flood exhausting the temporary budget does not stop permanent authentication.
        drop(limiter.acquire_issuance(Local, ip(3), t).unwrap());
        drop(
            limiter
                .acquire_issuance(Guest, ip(1), t + time::Duration::minutes(1))
                .unwrap(),
        );
    }

    #[test]
    fn issuance_permits_bound_all_methods_and_are_released_on_cancellation() {
        use orbisync_domain::AuthMethod::{External, Guest, Local};
        let limiter = LoginRateLimiter::new(100, 300, 10).with_issuance_limits(issuance_limits());
        let a = limiter.acquire_issuance(Guest, ip(1), now()).unwrap();
        let b = limiter.acquire_issuance(External, ip(2), now()).unwrap();
        assert_eq!(
            limiter
                .acquire_issuance(Local, ip(3), now())
                .unwrap_err()
                .scope,
            orbisync_application::metrics::RateLimitScope::AuthConcurrency
        );
        drop(a);
        drop(limiter.acquire_issuance(Local, ip(3), now()).unwrap());
        drop(b);
    }

    #[test]
    fn issuance_bucket_bound_never_evicts_live_limits_or_allows_source_rotation() {
        use orbisync_domain::AuthMethod::Guest;
        let limiter = LoginRateLimiter::new(100, 300, 2).with_issuance_limits(issuance_limits());
        drop(limiter.acquire_issuance(Guest, ip(1), now()).unwrap());
        drop(limiter.acquire_issuance(Guest, ip(2), now()).unwrap());
        assert!(limiter.acquire_issuance(Guest, ip(3), now()).is_err());
        drop(limiter.acquire_issuance(Guest, ip(1), now()).unwrap());
        assert!(limiter.acquire_issuance(Guest, ip(1), now()).is_err());
        drop(
            limiter
                .acquire_issuance(Guest, ip(3), now() + time::Duration::minutes(1))
                .unwrap(),
        );
    }
}
