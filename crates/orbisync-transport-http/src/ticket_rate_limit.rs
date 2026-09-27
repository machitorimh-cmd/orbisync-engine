//! Rate limiting for realtime ticket issuance (C5, SELF-A1).
//!
//! Bounds `POST /v1/realtime/tickets` per user and per session so an
//! authenticated caller cannot grow `realtime_tickets` without limit.
//! Also bounds memory: empty buckets are removed and the number of distinct
//! buckets is capped with fail-closed semantics.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// Sliding window limiter for `POST /v1/realtime/tickets`.
///
/// Enforces both per-user and per-session limits so that neither a single
/// session nor multiple sessions for the same user can exhaust storage.
/// The window is fixed (default 60s) and the max is fixed (default 10).
///
/// Memory bound (SELF-A1):
/// - empty buckets are removed via `retain` after pruning
/// - a periodic sweep removes buckets that were never touched again
/// - the number of distinct buckets is capped; when the cap is reached
///   new keys are denied (fail-closed, not fail-open)
#[derive(Debug)]
pub struct RealtimeTicketRateLimiter {
    max_per_window: u32,
    window: Duration,
    max_buckets: usize,
    sweep_interval: Duration,
    user_buckets: Mutex<HashMap<Uuid, VecDeque<OffsetDateTime>>>,
    session_buckets: Mutex<HashMap<Uuid, VecDeque<OffsetDateTime>>>,
    last_sweep: Mutex<OffsetDateTime>,
}

impl RealtimeTicketRateLimiter {
    /// Default cap for distinct buckets per map.
    const DEFAULT_MAX_BUCKETS: usize = 10_000;

    /// Creates a limiter with explicit limits.
    #[must_use]
    pub fn new(max_per_window: u32, window_secs: u64) -> Self {
        Self::new_with_max_buckets(max_per_window, window_secs, Self::DEFAULT_MAX_BUCKETS)
    }

    /// Creates a limiter with explicit limits and a custom bucket cap.
    ///
    /// Exposed for tests and for tuning. Production uses [`Self::new`].
    #[must_use]
    pub fn new_with_max_buckets(max_per_window: u32, window_secs: u64, max_buckets: usize) -> Self {
        let window = Duration::seconds(i64::try_from(window_secs).unwrap_or(60));
        Self {
            max_per_window,
            window,
            max_buckets,
            sweep_interval: window,
            user_buckets: Mutex::new(HashMap::new()),
            session_buckets: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(OffsetDateTime::UNIX_EPOCH),
        }
    }

    /// Creates a limiter with production defaults: 10 tickets per 60s.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(10, 60)
    }

    /// Override the bucket cap (builder style).
    #[must_use]
    pub fn with_max_buckets(self, max_buckets: usize) -> Self {
        Self {
            max_buckets,
            ..self
        }
    }

    /// Override the sweep interval (builder style).
    #[must_use]
    pub fn with_sweep_interval(self, sweep_secs: u64) -> Self {
        Self {
            sweep_interval: Duration::seconds(i64::try_from(sweep_secs).unwrap_or(60)),
            ..self
        }
    }

    /// Returns whether the request is allowed. If allowed, records the
    /// issuance timestamp in both user and session buckets.
    ///
    /// Prunes entries outside `window` before checking. Empty buckets are
    /// removed and a periodic sweep cleans buckets that are never touched again.
    /// When the number of distinct buckets reaches `max_buckets`, new keys are
    /// denied (fail-closed).
    pub fn check_and_record(&self, user_id: Uuid, session_id: Uuid, now: OffsetDateTime) -> bool {
        let mut user_guard = self.user_buckets.lock().unwrap_or_else(|e| e.into_inner());
        let mut sess_guard = self
            .session_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let cutoff = now - self.window;

        // Periodic sweep for keys that are never touched again (SELF-A1 §2).
        // Uses `retain` to drop empty deques after pruning.
        self.maybe_sweep_locked(&mut user_guard, &mut sess_guard, now, cutoff);

        // Per-key pruning for the two keys in this request.
        // Remove empty buckets immediately to avoid leaking entries.
        let mut user_needs_insert = false;
        let mut sess_needs_insert = false;

        // User bucket: prune if exists, drop if empty after prune
        if let Some(entry) = user_guard.get_mut(&user_id) {
            while entry.front().is_some_and(|ts| *ts <= cutoff) {
                entry.pop_front();
            }
            if entry.is_empty() {
                // Remove empty bucket via HashMap::remove (retain is used in sweep;
                // per-key immediate removal avoids keeping empty VecDeque)
                // We cannot remove while holding &mut entry, so drop entry first.
            }
        }
        if let Some(entry) = user_guard.get(&user_id)
            && entry.is_empty()
        {
            user_guard.remove(&user_id);
        }
        // Session bucket: prune if exists, drop if empty after prune (must happen before any early return)
        if let Some(entry) = sess_guard.get_mut(&session_id) {
            while entry.front().is_some_and(|ts| *ts <= cutoff) {
                entry.pop_front();
            }
        }
        if let Some(entry) = sess_guard.get(&session_id)
            && entry.is_empty()
        {
            sess_guard.remove(&session_id);
        }
        // Check if user entry still exists
        let user_exists = user_guard.contains_key(&user_id);
        let user_len = user_guard.get(&user_id).map(VecDeque::len).unwrap_or(0);

        if user_exists {
            if user_len >= self.max_per_window as usize {
                return false;
            }
        } else {
            // New key: enforce cap fail-closed (SELF-A1 §3)
            if user_guard.len() >= self.max_buckets {
                return false;
            }
            user_needs_insert = true;
            // Rate check for new key is 0 < max, so allow (if session also allows)
        }
        let sess_exists = sess_guard.contains_key(&session_id);
        let sess_len = sess_guard.get(&session_id).map(VecDeque::len).unwrap_or(0);

        if sess_exists {
            if sess_len >= self.max_per_window as usize {
                // If we already decided user needs insert but session is rate-limited,
                // we have not yet inserted user – so just deny.
                return false;
            }
        } else {
            if sess_guard.len() >= self.max_buckets {
                return false;
            }
            sess_needs_insert = true;
        }

        // Both checks passed – record.
        // Need to handle both new and existing.
        if user_needs_insert {
            let mut dq = VecDeque::new();
            dq.push_back(now);
            user_guard.insert(user_id, dq);
        } else {
            // Safety: entry must exist (we checked) and we already pruned
            if let Some(entry) = user_guard.get_mut(&user_id) {
                entry.push_back(now);
            } else {
                // Race: was removed after empty check? Insert anew if cap allows.
                if user_guard.len() < self.max_buckets {
                    let mut dq = VecDeque::new();
                    dq.push_back(now);
                    user_guard.insert(user_id, dq);
                } else {
                    return false;
                }
            }
        }

        if sess_needs_insert {
            let mut dq = VecDeque::new();
            dq.push_back(now);
            sess_guard.insert(session_id, dq);
        } else if let Some(entry) = sess_guard.get_mut(&session_id) {
            entry.push_back(now);
        } else {
            if sess_guard.len() < self.max_buckets {
                let mut dq = VecDeque::new();
                dq.push_back(now);
                sess_guard.insert(session_id, dq);
            } else {
                // Roll back user insertion to keep atomicity? We already inserted user.
                // Remove the just-inserted user timestamp to avoid partial state.
                // But user rate limit already accounted; rolling back is cleaner.
                if let Some(entry) = user_guard.get_mut(&user_id) {
                    entry.pop_back();
                    if entry.is_empty() {
                        user_guard.remove(&user_id);
                    }
                }
                return false;
            }
        }

        true
    }

    /// Removes expired entries from all buckets and drops empty buckets.
    ///
    /// Uses `retain` as required by SELF-A1 §1.
    pub fn sweep(&self, now: OffsetDateTime) {
        let mut user_guard = self.user_buckets.lock().unwrap_or_else(|e| e.into_inner());
        let mut sess_guard = self
            .session_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cutoff = now - self.window;
        user_guard.retain(|_, deque| {
            while deque.front().is_some_and(|ts| *ts <= cutoff) {
                deque.pop_front();
            }
            !deque.is_empty()
        });
        sess_guard.retain(|_, deque| {
            while deque.front().is_some_and(|ts| *ts <= cutoff) {
                deque.pop_front();
            }
            !deque.is_empty()
        });
        let mut last = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
        *last = now;
    }

    fn maybe_sweep_locked(
        &self,
        user_guard: &mut HashMap<Uuid, VecDeque<OffsetDateTime>>,
        sess_guard: &mut HashMap<Uuid, VecDeque<OffsetDateTime>>,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) {
        let mut last = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
        // Sweep if interval elapsed
        if now - *last >= self.sweep_interval {
            user_guard.retain(|_, deque| {
                while deque.front().is_some_and(|ts| *ts <= cutoff) {
                    deque.pop_front();
                }
                !deque.is_empty()
            });
            sess_guard.retain(|_, deque| {
                while deque.front().is_some_and(|ts| *ts <= cutoff) {
                    deque.pop_front();
                }
                !deque.is_empty()
            });
            *last = now;
        }
    }

    /// Returns current count for a user (prunes expired entries).
    #[cfg(test)]
    pub fn user_count(&self, user_id: Uuid, now: OffsetDateTime) -> usize {
        let mut guard = self.user_buckets.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = now - self.window;
        if let Some(entry) = guard.get_mut(&user_id) {
            while entry.front().is_some_and(|ts| *ts <= cutoff) {
                entry.pop_front();
            }
            if entry.is_empty() {
                guard.remove(&user_id);
                0
            } else {
                entry.len()
            }
        } else {
            0
        }
    }

    /// Returns current count for a session (prunes expired entries).
    #[cfg(test)]
    pub fn session_count(&self, session_id: Uuid, now: OffsetDateTime) -> usize {
        let mut guard = self
            .session_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cutoff = now - self.window;
        if let Some(entry) = guard.get_mut(&session_id) {
            while entry.front().is_some_and(|ts| *ts <= cutoff) {
                entry.pop_front();
            }
            if entry.is_empty() {
                guard.remove(&session_id);
                0
            } else {
                entry.len()
            }
        } else {
            0
        }
    }

    /// Returns number of distinct user buckets (for testing).
    #[must_use]
    pub fn user_bucket_count(&self) -> usize {
        self.user_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Returns number of distinct session buckets (for testing).
    #[must_use]
    pub fn session_bucket_count(&self) -> usize {
        self.session_buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Returns total number of buckets.
    #[must_use]
    pub fn total_bucket_count(&self) -> usize {
        self.user_bucket_count() + self.session_bucket_count()
    }

    /// Returns true if bucket cap has been reached.
    #[must_use]
    pub fn is_at_capacity(&self) -> bool {
        self.user_bucket_count() >= self.max_buckets
            || self.session_bucket_count() >= self.max_buckets
    }
}

#[cfg(test)]
mod tests {
    use super::RealtimeTicketRateLimiter;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid")
    }

    #[test]
    fn allows_up_to_limit() {
        let limiter = RealtimeTicketRateLimiter::new(3, 60);
        let user = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let t = now();
        assert!(limiter.check_and_record(user, sess, t));
        assert!(limiter.check_and_record(user, sess, t));
        assert!(limiter.check_and_record(user, sess, t));
        assert!(!limiter.check_and_record(user, sess, t), "4th exceeds 3");
    }

    #[test]
    fn window_slides() {
        let limiter = RealtimeTicketRateLimiter::new(2, 60);
        let user = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let t0 = now();
        assert!(limiter.check_and_record(user, sess, t0));
        assert!(limiter.check_and_record(user, sess, t0));
        assert!(!limiter.check_and_record(user, sess, t0));
        let t1 = t0 + time::Duration::seconds(61);
        assert!(
            limiter.check_and_record(user, sess, t1),
            "window slid, should allow again"
        );
    }

    #[test]
    fn per_user_and_per_session_independent() {
        // Per-user limit hits even with different sessions
        let limiter = RealtimeTicketRateLimiter::new(2, 60);
        let user = Uuid::now_v7();
        let s1 = Uuid::now_v7();
        let s2 = Uuid::now_v7();
        let t = now();
        assert!(limiter.check_and_record(user, s1, t));
        assert!(limiter.check_and_record(user, s1, t));
        // Same user, different session – still limited by user bucket
        assert!(!limiter.check_and_record(user, s2, t));
        // Different user, same pattern should allow
        let other = Uuid::now_v7();
        assert!(limiter.check_and_record(other, s2, t));
    }

    // SELF-A1 regression: window expiry removes empty bucket
    #[test]
    fn empty_bucket_removed_after_window() {
        let limiter = RealtimeTicketRateLimiter::new(5, 60);
        let user = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let t0 = now();
        assert!(limiter.check_and_record(user, sess, t0));
        assert_eq!(limiter.user_bucket_count(), 1);
        assert_eq!(limiter.session_bucket_count(), 1);
        // Advance beyond window and sweep - bucket should disappear via retain
        let t1 = t0 + time::Duration::seconds(61);
        limiter.sweep(t1);
        assert_eq!(
            limiter.user_bucket_count(),
            0,
            "empty user bucket must be removed after sweep (retain)"
        );
        assert_eq!(
            limiter.session_bucket_count(),
            0,
            "empty session bucket must be removed after sweep"
        );
        // Also via check_and_record pruning path: new call should have cleaned
        let limiter2 = RealtimeTicketRateLimiter::new(5, 60);
        let u2 = Uuid::now_v7();
        let s2 = Uuid::now_v7();
        assert!(limiter2.check_and_record(u2, s2, t0));
        // Second key touches limiter after window, sweeps, should also clean first key via maybe_sweep
        let other_user = Uuid::now_v7();
        let other_sess = Uuid::now_v7();
        // Need to ensure sweep interval elapsed: default 60s, t1 is 61s later so maybe_sweep triggers
        assert!(limiter2.check_and_record(other_user, other_sess, t1));
        // The original bucket's timestamps are expired but its key would have been swept when
        // the second key triggered maybe_sweep. If sweep removed it, count should be 1 (only other)
        // If per-key retain alone, the stale key would still remain. We assert it was swept.
        // Note: limiter2 had 1 bucket for u2/s2, plus 1 for other => after sweep, u2/s2 expired => removed, so 1 each
        assert_eq!(limiter2.user_bucket_count(), 1);
        assert_eq!(limiter2.session_bucket_count(), 1);
    }

    // SELF-A1: never-again key removed by sweep over time
    #[test]
    fn stale_keys_removed_by_sweep_without_reaccess() {
        let limiter = RealtimeTicketRateLimiter::new(5, 60);
        let t0 = now();
        // Insert many distinct keys that will never be touched again
        for _ in 0..5 {
            let u = Uuid::now_v7();
            let s = Uuid::now_v7();
            assert!(limiter.check_and_record(u, s, t0));
        }
        assert_eq!(limiter.user_bucket_count(), 5);
        let t1 = t0 + time::Duration::seconds(61);
        limiter.sweep(t1);
        assert_eq!(
            limiter.user_bucket_count(),
            0,
            "stale user buckets must be swept even though keys never reaccessed"
        );
        assert_eq!(limiter.session_bucket_count(), 0);
    }

    // SELF-A1: large distinct ids do not grow unbounded due to cap
    #[test]
    fn bucket_count_bounded_by_max() {
        let max_buckets = 10;
        let limiter = RealtimeTicketRateLimiter::new_with_max_buckets(100, 60, max_buckets);
        let t = now();
        // Try to insert 30 distinct user/session pairs
        for _ in 0..30 {
            let u = Uuid::now_v7();
            let s = Uuid::now_v7();
            // Some will be denied due to cap (fail-closed) – that's expected
            let _ = limiter.check_and_record(u, s, t);
        }
        assert!(
            limiter.user_bucket_count() <= max_buckets,
            "user buckets {} must not exceed cap {}",
            limiter.user_bucket_count(),
            max_buckets
        );
        assert!(
            limiter.session_bucket_count() <= max_buckets,
            "session buckets {} must not exceed cap {}",
            limiter.session_bucket_count(),
            max_buckets
        );
    }

    // SELF-A1: user cap independently enforced (isolate user side)
    #[test]
    fn user_cap_enforced_independently() {
        let max_buckets = 5;
        let limiter = RealtimeTicketRateLimiter::new_with_max_buckets(100, 60, max_buckets)
            .with_sweep_interval(3600);
        let t = now();
        let shared_sess = Uuid::now_v7();
        // Fill user buckets with distinct users, same session (session bucket stays 1)
        for _ in 0..max_buckets {
            let u = Uuid::now_v7();
            assert!(
                limiter.check_and_record(u, shared_sess, t),
                "should allow until user cap"
            );
        }
        assert_eq!(limiter.user_bucket_count(), max_buckets);
        assert_eq!(limiter.session_bucket_count(), 1);
        // Next distinct user must be denied even though session cap is far from full
        let new_u = Uuid::now_v7();
        assert!(
            !limiter.check_and_record(new_u, shared_sess, t),
            "user cap must be enforced independently of session cap"
        );
        assert_eq!(limiter.user_bucket_count(), max_buckets);
        assert_eq!(limiter.session_bucket_count(), 1);
    }

    // SELF-A1: session cap independently enforced (isolate session side)
    #[test]
    fn session_cap_enforced_independently() {
        let max_buckets = 5;
        let limiter = RealtimeTicketRateLimiter::new_with_max_buckets(100, 60, max_buckets)
            .with_sweep_interval(3600);
        let t = now();
        let shared_user = Uuid::now_v7();
        for _ in 0..max_buckets {
            let s = Uuid::now_v7();
            assert!(
                limiter.check_and_record(shared_user, s, t),
                "should allow until session cap"
            );
        }
        assert_eq!(limiter.session_bucket_count(), max_buckets);
        assert_eq!(limiter.user_bucket_count(), 1);
        let new_s = Uuid::now_v7();
        assert!(
            !limiter.check_and_record(shared_user, new_s, t),
            "session cap must be enforced independently of user cap"
        );
        assert_eq!(limiter.session_bucket_count(), max_buckets);
    }

    // SELF-A1: cap does NOT become fail-open (rate limit still enforced)
    #[test]
    fn cap_does_not_fail_open() {
        let max_buckets = 3;
        // Use small per-window limit to easily test rate limiting
        let limiter = RealtimeTicketRateLimiter::new_with_max_buckets(2, 60, max_buckets);
        let t = now();
        // Fill to cap with distinct keys
        let mut users = Vec::new();
        let mut sessions = Vec::new();
        for _ in 0..max_buckets {
            let u = Uuid::now_v7();
            let s = Uuid::now_v7();
            assert!(limiter.check_and_record(u, s, t));
            users.push(u);
            sessions.push(s);
        }
        assert_eq!(limiter.user_bucket_count(), max_buckets);
        // New distinct key should be denied (fail-closed), not allowed unlimited
        let new_user = Uuid::now_v7();
        let new_sess = Uuid::now_v7();
        assert!(
            !limiter.check_and_record(new_user, new_sess, t),
            "new key at capacity must be denied (fail-closed)"
        );
        // Existing key should still be rate-limited (second call within window: 1st was 1, 2nd allowed, 3rd denied)
        let existing_user = users[0];
        let existing_sess = sessions[0];
        // This is 2nd request for existing - should still be allowed (limit 2)
        assert!(limiter.check_and_record(existing_user, existing_sess, t));
        // 3rd should be rate limited
        assert!(
            !limiter.check_and_record(existing_user, existing_sess, t),
            "existing key must still enforce per-window limit at capacity"
        );
        // Ensure cap still not exceeded
        assert!(limiter.user_bucket_count() <= max_buckets);
    }

    // SELF-A1: per-key empty bucket removal for user side (without sweep)
    #[test]
    fn user_empty_bucket_per_key_removed() {
        // Large sweep interval so maybe_sweep does not hide per-key bug
        let limiter =
            RealtimeTicketRateLimiter::new_with_max_buckets(5, 60, 100).with_sweep_interval(3600);
        let user = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let t0 = now();
        assert!(limiter.check_and_record(user, sess, t0));
        assert_eq!(limiter.user_bucket_count(), 1);
        assert_eq!(limiter.session_bucket_count(), 1);
        let t1 = t0 + time::Duration::seconds(61);
        // user_count prunes and must remove empty bucket
        assert_eq!(limiter.user_count(user, t1), 0);
        assert_eq!(
            limiter.user_bucket_count(),
            0,
            "user empty bucket must be removed per-key (not just via sweep retain)"
        );
        // session side should still be present until separately pruned (we only called user_count)
        // Now prune session side as well
        assert_eq!(limiter.session_count(sess, t1), 0);
        assert_eq!(limiter.session_bucket_count(), 0);
    }

    // SELF-A1: per-key empty bucket removal for session side (without sweep)
    #[test]
    fn session_empty_bucket_per_key_removed() {
        let limiter =
            RealtimeTicketRateLimiter::new_with_max_buckets(5, 60, 100).with_sweep_interval(3600);
        let user = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let t0 = now();
        assert!(limiter.check_and_record(user, sess, t0));
        assert_eq!(limiter.session_bucket_count(), 1);
        let t1 = t0 + time::Duration::seconds(61);
        assert_eq!(limiter.session_count(sess, t1), 0);
        assert_eq!(
            limiter.session_bucket_count(),
            0,
            "session empty bucket must be removed per-key (not just via sweep retain)"
        );
        // also verify user side cleaned via session_count path? user side still needs explicit check
        // but we test user side separately, so also ensure user side not leaked via session path
        // user bucket should still be 1 until user_count is called, but we can check it is still 1
        // Actually after t1, user bucket is expired but not yet pruned via session_count.
        // To avoid hiding bug, we explicitly check user bucket count remains 1 until user_count cleans it.
        assert_eq!(
            limiter.user_bucket_count(),
            1,
            "user bucket still present until user pruned"
        );
        assert_eq!(limiter.user_count(user, t1), 0);
        assert_eq!(limiter.user_bucket_count(), 0);
    }

    // SELF-A1: check_and_record per-key user empty removal on denied (rate-limited) path
    #[test]
    fn user_check_and_record_cleans_empty_on_denied() {
        // Use separate session for U1's initial entry so S1 can be rate-limited independently
        let limiter =
            RealtimeTicketRateLimiter::new_with_max_buckets(2, 60, 10).with_sweep_interval(3600);
        let t0 = now();
        let u1 = Uuid::now_v7();
        let s0 = Uuid::now_v7();
        let s1 = Uuid::now_v7();
        let u2 = Uuid::now_v7();
        let u3 = Uuid::now_v7();
        // U1 with S0 at t0 (user U1 bucket)
        assert!(limiter.check_and_record(u1, s0, t0));
        // Prepare session S1 to be rate-limited at t1: 2 entries for S1 from different users
        assert!(limiter.check_and_record(u2, s1, t0 + time::Duration::seconds(50)));
        assert!(limiter.check_and_record(u3, s1, t0 + time::Duration::seconds(55)));
        let t1 = t0 + time::Duration::seconds(61);
        // At t1, U1's entry at t0 is expired (empty), S1 has 2 entries (50,55) -> rate limited
        assert!(
            !limiter.check_and_record(u1, s1, t1),
            "session limit should deny"
        );
        // After denied, empty user bucket for U1 must have been removed (per-key), so count should be 2 (u2,u3) not 3
        assert_eq!(
            limiter.user_bucket_count(),
            2,
            "user empty bucket must be removed even on denied check_and_record"
        );
        assert_eq!(limiter.session_bucket_count(), 2);
    }

    // SELF-A1: check_and_record per-key session empty removal on denied path
    #[test]
    fn session_check_and_record_cleans_empty_on_denied() {
        let limiter =
            RealtimeTicketRateLimiter::new_with_max_buckets(2, 60, 10).with_sweep_interval(3600);
        let t0 = now();
        let u0 = Uuid::now_v7();
        let s1 = Uuid::now_v7();
        let u1 = Uuid::now_v7();
        let s2 = Uuid::now_v7();
        let s3 = Uuid::now_v7();
        // S1 with U0 at t0 (session S1 bucket to become empty)
        assert!(limiter.check_and_record(u0, s1, t0));
        // Prepare user U1 to be rate-limited: 2 entries for U1
        assert!(limiter.check_and_record(u1, s2, t0 + time::Duration::seconds(50)));
        assert!(limiter.check_and_record(u1, s3, t0 + time::Duration::seconds(55)));
        let t1 = t0 + time::Duration::seconds(61);
        // S1's only entry at t0 will be pruned to empty at t1, U1 has 2 entries (50,55) -> rate limited
        assert!(
            !limiter.check_and_record(u1, s1, t1),
            "user limit should deny"
        );
        assert_eq!(
            limiter.session_bucket_count(),
            2,
            "session empty bucket must be removed even on denied check_and_record"
        );
        assert_eq!(limiter.user_bucket_count(), 2);
    }
}
