//! Server-side resume token store (W-18, D-23, D-24).
//!
//! Implements `mobile-resume-interest-backpressure.md` §3.2 / §3.4 with a
//! server-stored, opaque random token (MRIB-02, D-23). The store lives in
//! process memory only (D-24): tokens are short-lived (`resume_grace_seconds`
//! ≈ 60 s) and the instance actor's state is also ephemeral, so persisting
//! them would keep data for a destination that no longer exists after a
//! restart. Multi-node deployment will require revisiting this.
//!
//! Tokens are 32 cryptographically random bytes (256 bit) encoded as
//! URL-safe base64 without padding (≈43 chars, >128-bit entropy, §3.1
//! "推測困難"). Comparison uses [`crate::resume::constant_time_eq`] when both
//! values are present.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use orbisync_domain::{Clock, InstanceId, PresenceId, Revision, Timestamp, UserId};
use rand::Rng as _;

/// Binding kept server-side for a resume token (mobile-resume §3.2 table).
#[derive(Debug, Clone)]
pub struct ResumeBinding {
    /// Authenticated user.
    pub user_id: UserId,
    /// Instance to resume into.
    pub instance_id: InstanceId,
    /// Presence to rebind (same `PresenceId` on replay, §2.5).
    pub presence_id: PresenceId,
    /// Session epoch (bind generation, §3.3), incremented on rebind.
    pub epoch: u64,
    /// Admission revision: the minimum history cursor this presence may request.
    /// Token rotation preserves it so an interrupted replay remains recoverable.
    pub last_revision: Revision,
    /// Token expiry = issued_at + grace (§3.4). For a membership cleanup
    /// returned by the store, this is instead its disconnect-time grace deadline.
    pub expires_at: Timestamp,
    /// When the token was issued.
    pub issued_at: Timestamp,
    /// `true` once the connection that owned this presence has gone away.
    ///
    /// A binding is created while the client is still connected, because the
    /// token ships in `JoinAccepted`. Only a binding whose owner has actually
    /// left is a candidate for eviction; evicting a live member's binding would
    /// drop them from the instance.
    pub disconnected: bool,
}

/// In-process token → binding store (D-24).
///
/// Token comparison on the read path uses `constant_time_eq` rather than
/// `HashMap`'s hashed `==`.  The map is still keyed by the token string, but
/// lookup iterates and compares with `constant_time_eq` to avoid short-circuit
/// timing leaks on mismatch position (`resume.rs` §6.1).
pub struct ResumeSessionStore {
    clock: Arc<dyn Clock>,
    grace_seconds: u64,
    inner: Mutex<SessionEntries>,
}

#[derive(Default)]
struct SessionEntries {
    tokens: HashMap<String, ResumeBinding>,
    // Membership ownership outlives a consumed/expired token. For a
    // disconnected owner, expires_at is the independent membership grace
    // deadline; a token always keeps its original issuance-time TTL.
    presences: HashMap<PresenceId, ResumeBinding>,
    // Keep claims until the runtime acknowledges Leave. Failed or cancelled
    // submissions remain eligible for the next maintenance pass.
    retiring: HashSet<PresenceId>,
}

/// Count-only lifecycle view. Tokens and identifiers never leave this API.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PresenceOwnershipSnapshot {
    /// Connected owner generations, independent of token TTL.
    pub connected: u64,
    /// Disconnected owners inside the normal grace window.
    pub grace_waiting: u64,
    /// Expired/evicted owners awaiting acknowledged cleanup.
    pub reclaim_waiting: u64,
    /// Owned presences whose actor membership still exists.
    pub bound_memberships: u64,
    /// Owned presences with no actor membership.
    pub bindings_without_membership: u64,
}

impl core::fmt::Debug for ResumeSessionStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResumeSessionStore")
            .field("grace_seconds", &self.grace_seconds)
            .field("clock", &"dyn Clock")
            .field(
                "entry_count",
                &self.inner.lock().map(|g| g.tokens.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl ResumeSessionStore {
    /// Samples lifecycle ownership, then checks runtime membership outside the
    /// store lock. Concurrent Join/Resume/Leave can transiently skew the sample;
    /// consumers should alert only on sustained discrepancies.
    #[must_use]
    pub fn ownership_snapshot(
        &self,
        mut is_member: impl FnMut(InstanceId, PresenceId) -> bool,
    ) -> PresenceOwnershipSnapshot {
        let now = self.clock.now();
        let owners: Vec<_> = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .presences
                .values()
                .map(|binding| {
                    (
                        binding.instance_id,
                        binding.presence_id,
                        binding.disconnected,
                        guard.retiring.contains(&binding.presence_id)
                            || is_expired(binding.expires_at, now),
                    )
                })
                .collect()
        };
        let mut result = PresenceOwnershipSnapshot::default();
        for (instance, presence, disconnected, reclaim) in owners {
            if !disconnected {
                result.connected += 1;
            } else if reclaim {
                result.reclaim_waiting += 1;
            } else {
                result.grace_waiting += 1;
            }
            if is_member(instance, presence) {
                result.bound_memberships += 1;
            } else {
                result.bindings_without_membership += 1;
            }
        }
        result
    }
    /// Creates a store with `grace_seconds` TTL (typically
    /// `world.resume_grace_seconds`, 60 s).
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, grace_seconds: u64) -> Self {
        Self {
            clock,
            grace_seconds,
            inner: Mutex::new(SessionEntries::default()),
        }
    }

    /// Generates a new opaque token and stores `binding`.
    ///
    /// The token is 32 bytes from `OsRng` (CSPRNG) base64url-encoded without
    /// padding. Raw tokens and prefixes are never logged (mobile-resume §3.2).
    pub fn issue(
        &self,
        user_id: UserId,
        instance_id: InstanceId,
        presence_id: PresenceId,
        epoch: u64,
        last_revision: Revision,
    ) -> String {
        let token = generate_token();
        let now = self.clock.now();
        let expires_at = timestamp_add_seconds(now, self.grace_seconds);
        let binding = ResumeBinding {
            user_id,
            instance_id,
            presence_id,
            epoch,
            last_revision,
            expires_at,
            issued_at: now,
            disconnected: false,
        };
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.presences.insert(presence_id, binding.clone());
        guard.tokens.insert(token.clone(), binding);
        token
    }

    /// Atomically transfers the presence to its next connection generation.
    /// A presence already evicted/reaped or rebound cannot be resurrected by
    /// a token consumed before that lifecycle change.
    pub fn rebind(&self, previous: &ResumeBinding) -> Option<(String, u64)> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.retiring.contains(&previous.presence_id) {
            return None;
        }
        let owner = guard.presences.get(&previous.presence_id)?;
        if owner.epoch != previous.epoch
            || owner.user_id != previous.user_id
            || owner.instance_id != previous.instance_id
            || owner.disconnected && is_expired(owner.expires_at, self.clock.now())
        {
            return None;
        }
        let epoch = previous.epoch.checked_add(1)?;
        let now = self.clock.now();
        let mut binding = previous.clone();
        binding.epoch = epoch;
        binding.issued_at = now;
        binding.expires_at = timestamp_add_seconds(now, self.grace_seconds);
        binding.disconnected = false;
        let token = generate_token();
        guard
            .tokens
            .retain(|_, value| value.presence_id != previous.presence_id);
        guard
            .presences
            .insert(previous.presence_id, binding.clone());
        guard.tokens.insert(token.clone(), binding);
        Some((token, epoch))
    }

    /// Whether this connection still owns the presence. Token TTL is not
    /// consulted: a long-lived connection remains the owner until disconnect.
    #[must_use]
    pub fn is_current(&self, presence: PresenceId, epoch: u64) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .presences
            .get(&presence)
            .is_some_and(|binding| binding.epoch == epoch && !binding.disconnected)
    }

    /// Inserts a binding under a specific token (test helper).
    #[cfg(test)]
    pub fn insert_with_token(&self, token: String, binding: ResumeBinding) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.presences.insert(binding.presence_id, binding.clone());
        guard.tokens.insert(token, binding);
    }

    /// Number of stored tokens (including expired, until cleaned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.tokens.len()).unwrap_or(0)
    }

    /// Returns `true` when no token is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Claims the longest-held binding for `instance` for membership cleanup.
    /// The caller acknowledges successful Leave with [`Self::complete_cleanup`].
    ///
    /// A grace-held presence occupies capacity while its owner is gone. Without
    /// a way to reclaim it, joining and disconnecting in a loop would keep an
    /// instance full for a whole grace window each time, which is a cheap
    /// denial of service. `mobile-resume-interest-backpressure.md` line 71
    /// settles the conflict: resume is an optimisation, and losing the resume
    /// material must degrade to a full resync rather than break the connection.
    /// So a live member that wants in outranks a departed one that might come
    /// back, and the evicted client simply receives `ResyncRequired`.
    ///
    /// The earliest disconnect grace deadline goes first.
    pub fn evict_oldest_for_instance(&self, instance: InstanceId) -> Option<ResumeBinding> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let presence = guard
            .presences
            .iter()
            .filter(|(presence, b)| {
                b.instance_id == instance && b.disconnected && !guard.retiring.contains(*presence)
            })
            .min_by_key(|(_, b)| b.expires_at.to_unix_millis().unwrap_or(i64::MAX))
            .map(|(presence, _)| *presence)?;
        guard
            .tokens
            .retain(|_, binding| binding.presence_id != presence);
        guard.retiring.insert(presence);
        guard.presences.get(&presence).cloned()
    }

    /// Drops cleanup ownership only after Leave was acknowledged (or the
    /// runtime no longer contains the membership). Stale acknowledgements may
    /// never remove a different connection generation.
    pub fn complete_cleanup(&self, presence: PresenceId, epoch: u64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .presences
            .get(&presence)
            .is_some_and(|binding| binding.epoch == epoch && binding.disconnected)
            && guard.retiring.contains(&presence)
        {
            guard.presences.remove(&presence);
            guard.retiring.remove(&presence);
            guard
                .tokens
                .retain(|_, binding| binding.presence_id != presence);
        }
    }

    /// Ends only the calling connection's ownership. Cleanup from a superseded
    /// socket must not disconnect a newer incarnation of the same presence.
    pub fn mark_disconnected_epoch(&self, presence: PresenceId, epoch: u64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.disconnect_inner(&mut guard, presence, epoch);
    }

    fn disconnect_inner(&self, guard: &mut SessionEntries, presence: PresenceId, epoch: u64) {
        let Some(binding) = guard.presences.get_mut(&presence) else {
            return;
        };
        if binding.epoch != epoch || binding.disconnected {
            return;
        }
        binding.disconnected = true;
        binding.expires_at = timestamp_add_seconds(self.clock.now(), self.grace_seconds);
        for token_binding in guard.tokens.values_mut() {
            if token_binding.presence_id == presence && token_binding.epoch == epoch {
                token_binding.disconnected = true;
            }
        }
    }

    /// Consumes `token` if it exists, has not expired, and matches via
    /// `constant_time_eq` (single-use, §3.4).  Expired entries are removed and
    /// return `None`.
    ///
    /// The lookup iterates over keys and uses `constant_time_eq` so the timing
    /// does not leak the position of a mismatch.  Length alone is not hidden
    /// (tokens are fixed-length, so length is not a secret).
    pub fn consume(&self, token: &str) -> Option<ResumeBinding> {
        self.consume_inner(token, None)
    }

    /// Consumes a token only for its authenticated owner. A rejected user must
    /// not invalidate another user's session by attempting to resume it.
    pub fn consume_for_user(&self, token: &str, user: UserId) -> Option<ResumeBinding> {
        self.consume_inner(token, Some(user))
    }

    fn consume_inner(&self, token: &str, user: Option<UserId>) -> Option<ResumeBinding> {
        if token.is_empty() {
            return None;
        }
        let now = self.clock.now();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Find the key with constant-time equality.
        let matched_key = {
            let mut found: Option<String> = None;
            for k in guard.tokens.keys() {
                if crate::resume::constant_time_eq(k, token) {
                    found = Some(k.clone());
                    break;
                }
            }
            found
        };
        let key = matched_key?;
        if user.is_some_and(|user| {
            guard
                .tokens
                .get(&key)
                .is_none_or(|binding| binding.user_id != user)
        }) {
            return None;
        }
        // Remove single-use regardless of expiry outcome.
        let binding = guard.tokens.remove(&key)?;
        if is_expired(binding.expires_at, now)
            || guard
                .presences
                .get(&binding.presence_id)
                .is_none_or(|owner| owner.epoch != binding.epoch)
        {
            None
        } else {
            Some(binding)
        }
    }

    /// Peeks without consuming (used for gap tests that need to keep the token).
    #[cfg(test)]
    pub fn peek_constant_time(&self, token: &str) -> Option<ResumeBinding> {
        if token.is_empty() {
            return None;
        }
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in guard.tokens.iter() {
            if crate::resume::constant_time_eq(k, token) {
                let now = self.clock.now();
                if is_expired(v.expires_at, now) {
                    return None;
                }
                return Some(v.clone());
            }
        }
        None
    }

    /// Expires tokens independently and claims memberships whose grace elapsed,
    /// including unacknowledged earlier claims. Active ownership survives token
    /// expiry; cleanup ownership survives failed or cancelled Leave attempts.
    pub fn prune_expired(&self) -> Vec<ResumeBinding> {
        let now = self.clock.now();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .tokens
            .retain(|_, binding| !is_expired(binding.expires_at, now));
        let expired: Vec<_> = guard
            .presences
            .iter()
            .filter(|(presence, binding)| {
                binding.disconnected
                    && (guard.retiring.contains(*presence) || is_expired(binding.expires_at, now))
            })
            .map(|(_, binding)| binding.clone())
            .collect();
        for binding in &expired {
            guard.retiring.insert(binding.presence_id);
        }
        expired
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn timestamp_add_seconds(ts: Timestamp, secs: u64) -> Timestamp {
    let millis = ts.to_unix_millis().unwrap_or(0);
    let added = millis.saturating_add(secs.saturating_mul(1000) as i64);
    Timestamp::from_unix_millis(added).unwrap_or(ts)
}

fn is_expired(expires_at: Timestamp, now: Timestamp) -> bool {
    match (expires_at.to_unix_millis(), now.to_unix_millis()) {
        (Ok(exp), Ok(cur)) => cur > exp,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ResumeSessionStore, is_expired, timestamp_add_seconds};
    use orbisync_domain::{Clock, InstanceId, PresenceId, Revision, Timestamp, UserId};
    use std::sync::Arc;

    struct FixedClock {
        inner: std::sync::Mutex<Timestamp>,
    }
    impl FixedClock {
        fn new(ts: Timestamp) -> Self {
            Self {
                inner: std::sync::Mutex::new(ts),
            }
        }
        fn advance_millis(&self, d: u64) {
            let mut g = self.inner.lock().expect("lock");
            let cur = g.to_unix_millis().expect("ts");
            *g = Timestamp::from_unix_millis(cur + d as i64).expect("future");
        }
    }
    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            *self.inner.lock().expect("lock")
        }
    }

    fn ts(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis).expect("valid")
    }

    #[test]
    fn diagnostics_distinguish_connected_grace_reclaim_and_missing_memberships() {
        let clock = Arc::new(FixedClock::new(ts(1_000)));
        let store = ResumeSessionStore::new(clock.clone(), 60);
        let presence = PresenceId::generate();
        let instance = InstanceId::generate();
        store.issue(UserId::generate(), instance, presence, 0, Revision::INITIAL);
        clock.advance_millis(61_000);
        store.prune_expired();
        let connected = store.ownership_snapshot(|_, _| true);
        assert_eq!(
            (
                connected.connected,
                connected.grace_waiting,
                connected.reclaim_waiting
            ),
            (1, 0, 0)
        );
        store.mark_disconnected_epoch(presence, 0);
        let grace = store.ownership_snapshot(|_, _| true);
        assert_eq!(
            (grace.connected, grace.grace_waiting, grace.reclaim_waiting),
            (0, 1, 0)
        );
        assert_eq!(grace.bound_memberships, 1);
        store.evict_oldest_for_instance(instance);
        let reclaim = store.ownership_snapshot(|_, _| false);
        assert_eq!(
            (reclaim.reclaim_waiting, reclaim.bindings_without_membership),
            (1, 1)
        );
        store.complete_cleanup(presence, 0);
        assert_eq!(
            store.ownership_snapshot(|_, _| false),
            super::PresenceOwnershipSnapshot::default()
        );
    }

    #[test]
    fn issue_and_consume_roundtrip() {
        let clock = Arc::new(FixedClock::new(ts(1_700_000_000_000)));
        let store = ResumeSessionStore::new(clock.clone() as Arc<dyn Clock>, 60);
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            PresenceId::generate(),
            0,
            Revision::from_u64(42),
        );
        assert!(!token.is_empty());
        assert!(store.len() == 1);
        let binding = store.consume(&token).expect("must consume");
        assert_eq!(binding.last_revision, Revision::from_u64(42));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn single_use_second_consume_fails() {
        let clock = Arc::new(FixedClock::new(ts(1_700_000_000_000)));
        let store = ResumeSessionStore::new(Arc::clone(&clock) as Arc<dyn Clock>, 60);
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            PresenceId::generate(),
            0,
            Revision::from_u64(10),
        );
        assert!(store.consume(&token).is_some());
        assert!(
            store.consume(&token).is_none(),
            "single-use: second consume must fail"
        );
    }

    #[test]
    fn expired_token_is_rejected() {
        let clock = Arc::new(FixedClock::new(ts(1_700_000_000_000)));
        let store = ResumeSessionStore::new(clock.clone() as Arc<dyn Clock>, 60);
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            PresenceId::generate(),
            0,
            Revision::from_u64(5),
        );
        clock.advance_millis(61_000);
        assert!(
            store.consume(&token).is_none(),
            "expired token must be rejected"
        );
    }

    #[test]
    fn disconnected_binding_is_retained_during_grace_then_pruned() {
        let clock = Arc::new(FixedClock::new(ts(1_700_000_000_000)));
        let store = ResumeSessionStore::new(Arc::clone(&clock) as Arc<dyn Clock>, 60);
        let presence = PresenceId::generate();
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            presence,
            0,
            Revision::from_u64(1),
        );
        store.mark_disconnected_epoch(presence, 0);
        clock.advance_millis(60_000);
        assert!(
            store.prune_expired().is_empty(),
            "grace boundary is retained"
        );
        assert_eq!(store.len(), 1);
        clock.advance_millis(1);
        let expired = store.prune_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(store.len(), 0);
        assert_eq!(expired[0].presence_id, presence);
        assert!(store.consume(&token).is_none());
    }

    #[test]
    fn constant_time_lookup_rejects_mismatch() {
        let clock = Arc::new(FixedClock::new(ts(1_700_000_000_000)));
        let store = ResumeSessionStore::new(Arc::clone(&clock) as Arc<dyn Clock>, 60);
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            PresenceId::generate(),
            0,
            Revision::from_u64(7),
        );
        let mut bad = token.clone();
        // flip last char
        let last = bad.pop().expect("pop");
        bad.push(if last == 'A' { 'B' } else { 'A' });
        assert!(store.consume(&bad).is_none());
        // original still consumable
        assert!(store.consume(&token).is_some());
    }

    #[test]
    fn active_token_expiry_preserves_disconnect_cleanup_without_extending_token_ttl() {
        let clock = Arc::new(FixedClock::new(ts(1_000)));
        let store = ResumeSessionStore::new(clock.clone(), 60);
        let presence = PresenceId::generate();
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            presence,
            0,
            Revision::INITIAL,
        );
        clock.advance_millis(61_000);
        assert!(
            store.prune_expired().is_empty(),
            "active memberships must not be reaped"
        );
        assert!(
            store.consume(&token).is_none(),
            "token expiry remains issuance-time plus TTL"
        );
        assert!(store.is_current(presence, 0));
        store.mark_disconnected_epoch(presence, 0);
        clock.advance_millis(60_000);
        assert!(
            store.prune_expired().is_empty(),
            "membership grace starts at disconnect"
        );
        clock.advance_millis(1);
        let expired = store.prune_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].presence_id, presence);
        assert!(!store.is_current(presence, 0));
    }

    #[test]
    fn consumed_token_does_not_lose_membership_cleanup_when_resume_fails() {
        let clock = Arc::new(FixedClock::new(ts(1_000)));
        let store = ResumeSessionStore::new(clock.clone(), 60);
        let presence = PresenceId::generate();
        let token = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            presence,
            0,
            Revision::INITIAL,
        );
        store.mark_disconnected_epoch(presence, 0);
        assert!(store.consume(&token).is_some());
        // A revision gap now rejects resume, so no new token is issued.
        clock.advance_millis(60_001);
        assert_eq!(store.prune_expired()[0].presence_id, presence);
    }

    #[test]
    fn old_epoch_disconnect_cannot_evict_or_expire_rebound_live_presence() {
        let clock = Arc::new(FixedClock::new(ts(1_000)));
        let store = ResumeSessionStore::new(clock.clone(), 60);
        let user = UserId::generate();
        let instance = InstanceId::generate();
        let presence = PresenceId::generate();
        let token = store.issue(user, instance, presence, 0, Revision::INITIAL);
        let previous = store.consume_for_user(&token, user).unwrap();
        let (new_token, epoch) = store.rebind(&previous).unwrap();
        assert_eq!(epoch, 1);
        assert!(
            store.rebind(&previous).is_none(),
            "stale rebind cannot claim the presence again"
        );
        assert!(!store.is_current(presence, 0));
        store.mark_disconnected_epoch(presence, 0);
        assert!(store.is_current(presence, 1));
        assert!(store.evict_oldest_for_instance(instance).is_none());
        clock.advance_millis(60_001);
        assert!(
            store.prune_expired().is_empty(),
            "old cleanup must not reap a live owner"
        );
        assert!(
            store.consume(&new_token).is_none(),
            "new token keeps its TTL"
        );
        store.mark_disconnected_epoch(presence, 1);
        assert_eq!(store.evict_oldest_for_instance(instance).unwrap().epoch, 1);
        assert!(
            store.rebind(&previous).is_none(),
            "evicted presence cannot be resurrected"
        );
    }

    #[test]
    fn cleanup_claim_survives_until_matching_leave_acknowledgement() {
        let clock = Arc::new(FixedClock::new(ts(1_000)));
        let store = ResumeSessionStore::new(clock, 60);
        let user = UserId::generate();
        let instance = InstanceId::generate();
        let presence = PresenceId::generate();
        let token = store.issue(user, instance, presence, 0, Revision::INITIAL);
        store.mark_disconnected_epoch(presence, 0);
        let binding = store.evict_oldest_for_instance(instance).unwrap();
        assert!(
            store.consume(&token).is_none(),
            "a cleanup claim invalidates its token immediately"
        );
        assert!(
            store.rebind(&binding).is_none(),
            "a consumed token cannot resurrect a retiring membership"
        );
        assert_eq!(
            store.prune_expired().len(),
            1,
            "retry eviction before its original grace deadline"
        );
        assert_eq!(
            store.prune_expired().len(),
            1,
            "an unacknowledged attempt must stay recoverable"
        );
        store.complete_cleanup(presence, 1);
        assert_eq!(
            store.prune_expired().len(),
            1,
            "a stale acknowledgement cannot remove another epoch"
        );
        store.complete_cleanup(presence, 0);
        assert!(store.prune_expired().is_empty());
    }

    #[test]
    fn token_has_high_entropy_length() {
        let clock = Arc::new(FixedClock::new(ts(0)));
        let store = ResumeSessionStore::new(Arc::clone(&clock) as Arc<dyn Clock>, 60);
        let t1 = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            PresenceId::generate(),
            0,
            Revision::from_u64(1),
        );
        let t2 = store.issue(
            UserId::generate(),
            InstanceId::generate(),
            PresenceId::generate(),
            0,
            Revision::from_u64(1),
        );
        assert_ne!(t1, t2);
        // 32 bytes -> 43 chars url-safe no pad
        assert_eq!(t1.len(), 43);
    }

    #[test]
    fn is_expired_helper() {
        assert!(!is_expired(ts(2000), ts(1000)));
        assert!(is_expired(ts(1000), ts(2000)));
        assert!(!is_expired(ts(1000), ts(1000)));
    }

    #[test]
    fn timestamp_add_seconds_saturates() {
        let base = ts(1_000);
        let added = timestamp_add_seconds(base, 60);
        assert_eq!(added.to_unix_millis().expect("ok"), 61_000);
    }
}
