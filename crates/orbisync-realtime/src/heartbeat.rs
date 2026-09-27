//! Heartbeat liveness for realtime connections.
//!
//! Implements `mobile-resume-interest-backpressure.md` §6. The manager tracks
//! the last observed activity and the last heartbeat that was sent, and decides
//! when the connection should emit a `Heartbeat`, reply with a `HeartbeatAck`,
//! or be considered timed out via `ConnectionEvent::HeartbeatTimeout`.
//!
//! The manager is deterministic: time is supplied as [`Timestamp`] so tests can
//! inject a fixed clock and protocol encoding stays in the gateway/heartbeat
//! helpers without hidden system calls.

use orbisync_domain::Timestamp;
use orbisync_protocol::PROTOCOL_MAJOR;
use orbisync_protocol::v1::{Envelope, Heartbeat, HeartbeatAck, envelope};

use crate::connection::HeartbeatPolicy;
use crate::state::{ConnectionEvent, ConnectionState, transition};

/// Tracks heartbeat interval and idle timeout for a single connection.
///
/// `last_activity` is updated whenever a valid frame (including `Heartbeat`)
/// is received. Timeout is measured against this field; sending a heartbeat
/// does not extend the deadline.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatManager {
    policy: HeartbeatPolicy,
    last_activity: Timestamp,
    last_heartbeat_sent: Option<Timestamp>,
}

impl HeartbeatManager {
    /// Creates a manager with the given policy whose last activity is `now`.
    #[must_use]
    pub const fn new(policy: HeartbeatPolicy, now: Timestamp) -> Self {
        Self {
            policy,
            last_activity: now,
            last_heartbeat_sent: None,
        }
    }

    /// Returns the policy that drives this manager.
    #[must_use]
    pub const fn policy(self) -> HeartbeatPolicy {
        self.policy
    }

    /// Returns the last activity timestamp.
    #[must_use]
    pub const fn last_activity(self) -> Timestamp {
        self.last_activity
    }

    /// Returns the timestamp of the last heartbeat that was sent, if any.
    #[must_use]
    pub const fn last_heartbeat_sent(self) -> Option<Timestamp> {
        self.last_heartbeat_sent
    }

    /// Records that a valid frame was received at `now`.
    pub fn record_activity(&mut self, now: Timestamp) {
        self.last_activity = now;
    }

    /// Records that a `Heartbeat` was received at `now`.
    ///
    /// Currently identical to [`Self::record_activity`] but kept separate so
    /// call sites can express intent and metrics can distinguish the two.
    pub fn record_heartbeat(&mut self, now: Timestamp) {
        self.record_activity(now);
    }

    /// Returns `true` when the server should send a `Heartbeat` at `now`.
    ///
    /// A heartbeat is due when at least `interval` has elapsed since the last
    /// heartbeat that was sent. If no heartbeat has been sent yet, the interval
    /// is measured from `last_activity` (typically the handshake completion).
    #[must_use]
    pub fn should_send_heartbeat(&self, now: Timestamp) -> bool {
        let interval_millis = self.policy.interval_millis() as i64;
        let Some(last_sent) = self.last_heartbeat_sent else {
            // No heartbeat sent yet: measure from last_activity
            let Some(expiry) = self.last_activity.checked_add_millis(interval_millis).ok() else {
                return false;
            };
            return now >= expiry;
        };
        let Some(expiry) = last_sent.checked_add_millis(interval_millis).ok() else {
            return false;
        };
        now >= expiry
    }

    /// Marks that a `Heartbeat` was sent at `now`.
    pub fn mark_heartbeat_sent(&mut self, now: Timestamp) {
        self.last_heartbeat_sent = Some(now);
    }

    /// Returns `true` when `now` is at or past the idle timeout.
    #[must_use]
    pub fn is_timed_out(&self, now: Timestamp) -> bool {
        let timeout_millis = self.policy.timeout_seconds() as i64 * 1000;
        let Ok(expiry) = self.last_activity.checked_add_millis(timeout_millis) else {
            return false;
        };
        now >= expiry
    }

    /// Returns `Some(HeartbeatTimeout)` when the connection has timed out.
    #[must_use]
    pub fn timeout_event(&self, now: Timestamp) -> Option<ConnectionEvent> {
        if self.is_timed_out(now) {
            Some(ConnectionEvent::HeartbeatTimeout)
        } else {
            None
        }
    }

    /// Returns `Some(Closed)` when the timeout transition is defined for `from`.
    ///
    /// The contract defines `heartbeat_timeout -> closed` for
    /// `ready`, `joining`, `active`, and `resuming`. This helper composes the
    /// timeout check with [`transition`].
    #[must_use]
    pub fn next_state_on_timeout(
        &self,
        from: ConnectionState,
        now: Timestamp,
    ) -> Option<ConnectionState> {
        let event = self.timeout_event(now)?;
        transition(from, event)
    }

    /// Returns the absolute time at which the connection will time out if no
    /// further activity is observed.
    ///
    /// Returns `None` only when the addition overflows the representable
    /// timestamp range, which is not expected for normal timeout values.
    #[must_use]
    pub fn expires_at(&self) -> Option<Timestamp> {
        let timeout_millis = self.policy.timeout_seconds() as i64 * 1000;
        self.last_activity.checked_add_millis(timeout_millis).ok()
    }
}

// ---------------------------------------------------------------------------
// Envelope helpers (Control class)
// ---------------------------------------------------------------------------

/// Builds a `Heartbeat` envelope.
///
/// `envelope` header fields follow `realtime-protocol-and-connection.md` §2.2
/// and are supplied by the caller so the manager stays independent of
/// connection identifiers.
#[must_use]
pub fn heartbeat_envelope(
    protocol_minor: u32,
    message_id: String,
    sequence: u64,
    sent_at_unix_ms: i64,
    instance_id: String,
    client_time_unix_ms: i64,
) -> Envelope {
    Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor,
        message_id,
        sequence,
        sent_at_unix_ms,
        instance_id,
        payload: Some(envelope::Payload::Heartbeat(Heartbeat {
            client_time_unix_ms,
        })),
    }
}

/// Builds a `HeartbeatAck` envelope that echoes `client_time_unix_ms` and
/// carries the server's current time.
///
/// The server time is supplied as a pre-encoded `server_time_unix_ms` so the
/// caller can propagate encoding errors without panicking.
#[must_use]
pub fn heartbeat_ack_envelope(
    protocol_minor: u32,
    message_id: String,
    sequence: u64,
    sent_at_unix_ms: i64,
    instance_id: String,
    client_time_unix_ms: i64,
    server_time_unix_ms: i64,
) -> Envelope {
    Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor,
        message_id,
        sequence,
        sent_at_unix_ms,
        instance_id,
        payload: Some(envelope::Payload::HeartbeatAck(HeartbeatAck {
            client_time_unix_ms,
            server_time_unix_ms,
        })),
    }
}

/// Builds a `HeartbeatAck` that echoes the client's timestamp and uses `now`
/// as the server time, returning the envelope or `None` when `now` cannot be
/// represented as `i64` milliseconds.
///
/// The `None` case is not expected for normal timestamps but the function
/// avoids `expect`/`unwrap` as required by the crate's lint rules.
pub fn heartbeat_ack_for_heartbeat(
    protocol_minor: u32,
    message_id: String,
    sequence: u64,
    instance_id: String,
    heartbeat: &Heartbeat,
    now: Timestamp,
) -> Option<Envelope> {
    let server_time_unix_ms = now.to_unix_millis().ok()?;
    let sent_at_unix_ms = server_time_unix_ms;
    Some(heartbeat_ack_envelope(
        protocol_minor,
        message_id,
        sequence,
        sent_at_unix_ms,
        instance_id,
        heartbeat.client_time_unix_ms,
        server_time_unix_ms,
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::{HeartbeatManager, heartbeat_ack_for_heartbeat, heartbeat_envelope};
    use crate::connection::HeartbeatPolicy;
    use crate::state::{ConnectionEvent, ConnectionState, transition};
    use orbisync_config::Config;
    use orbisync_domain::Timestamp;
    use orbisync_protocol::v1::{Heartbeat, envelope};

    fn policy() -> HeartbeatPolicy {
        let config = Config::default();
        HeartbeatPolicy::from_config(&config.realtime)
    }

    fn custom_policy(interval_s: u64, timeout_s: u64) -> HeartbeatPolicy {
        // Build via config mutation to reuse the validated policy constructor.
        let mut config = Config::default();
        config.realtime.heartbeat_interval_seconds = interval_s;
        config.realtime.connection_timeout_seconds = timeout_s;
        HeartbeatPolicy::from_config(&config.realtime)
    }

    fn ts(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis).expect("test timestamp is in range")
    }

    #[test]
    fn test_new_tracks_last_activity() {
        let now = ts(1_000_000);
        let manager = HeartbeatManager::new(policy(), now);
        assert_eq!(manager.last_activity(), now);
        assert_eq!(manager.last_heartbeat_sent(), None);
        assert_eq!(manager.policy().interval_seconds(), 20);
        assert_eq!(manager.policy().timeout_seconds(), 60);
    }

    #[test]
    fn test_should_send_heartbeat_after_interval_when_never_sent() {
        let start = ts(0);
        let manager = HeartbeatManager::new(custom_policy(20, 60), start);
        // 19s -> not yet
        assert!(!manager.should_send_heartbeat(ts(19_000)));
        // 20s -> due
        assert!(manager.should_send_heartbeat(ts(20_000)));
        // 21s -> also due (overdue)
        assert!(manager.should_send_heartbeat(ts(21_000)));
    }

    #[test]
    fn test_should_send_heartbeat_respects_last_sent() {
        let start = ts(0);
        let mut manager = HeartbeatManager::new(custom_policy(15, 45), start);
        // First heartbeat due at 15s
        assert!(manager.should_send_heartbeat(ts(15_000)));
        manager.mark_heartbeat_sent(ts(15_000));
        // Next due 30s
        assert!(!manager.should_send_heartbeat(ts(29_999)));
        assert!(manager.should_send_heartbeat(ts(30_000)));
        manager.mark_heartbeat_sent(ts(30_000));
        assert!(!manager.should_send_heartbeat(ts(44_999)));
        assert!(manager.should_send_heartbeat(ts(45_000)));
    }

    #[test]
    fn test_is_timed_out_after_timeout() {
        let start = ts(0);
        let manager = HeartbeatManager::new(custom_policy(20, 60), start);
        assert!(!manager.is_timed_out(ts(59_999)));
        assert!(manager.is_timed_out(ts(60_000)));
        assert!(manager.is_timed_out(ts(61_000)));
    }

    #[test]
    fn test_timeout_event_returns_heartbeat_timeout() {
        let start = ts(1_000);
        let manager = HeartbeatManager::new(custom_policy(10, 30), start);
        assert_eq!(manager.timeout_event(ts(30_000)), None);
        // start 1_000 + 30_000 = 31_000
        assert_eq!(
            manager.timeout_event(ts(31_000)),
            Some(ConnectionEvent::HeartbeatTimeout)
        );
    }

    #[test]
    fn test_next_state_on_timeout_maps_to_closed() {
        let start = ts(0);
        let manager = HeartbeatManager::new(custom_policy(10, 30), start);
        let timeout_at = ts(30_000);
        for from in [
            ConnectionState::Ready,
            ConnectionState::Joining,
            ConnectionState::Active,
            ConnectionState::Resuming,
        ] {
            let next = manager.next_state_on_timeout(from, timeout_at);
            assert_eq!(next, Some(ConnectionState::Closed));
            // Also verify contract transition directly
            assert_eq!(
                transition(from, ConnectionEvent::HeartbeatTimeout),
                Some(ConnectionState::Closed)
            );
        }
    }

    #[test]
    fn test_next_state_on_timeout_returns_none_when_not_timed_out() {
        let start = ts(0);
        let manager = HeartbeatManager::new(custom_policy(10, 30), start);
        assert_eq!(
            manager.next_state_on_timeout(ConnectionState::Active, ts(10_000)),
            None
        );
    }

    #[test]
    fn test_next_state_on_timeout_returns_none_for_state_without_transition() {
        let start = ts(0);
        let manager = HeartbeatManager::new(custom_policy(10, 30), start);
        // Connecting has no heartbeat_timeout transition
        let timeout_at = ts(30_000);
        assert_eq!(
            manager.next_state_on_timeout(ConnectionState::Connecting, timeout_at),
            None
        );
        assert_eq!(
            manager.next_state_on_timeout(ConnectionState::Closed, timeout_at),
            None
        );
    }

    #[test]
    fn test_record_activity_resets_timeout() {
        let start = ts(0);
        let mut manager = HeartbeatManager::new(custom_policy(20, 60), start);
        // Would time out at 60s
        assert!(manager.is_timed_out(ts(60_000)));
        // Receive activity at 50s pushes timeout to 110s
        manager.record_activity(ts(50_000));
        assert!(!manager.is_timed_out(ts(60_000)));
        assert!(!manager.is_timed_out(ts(109_999)));
        assert!(manager.is_timed_out(ts(110_000)));
    }

    #[test]
    fn test_record_heartbeat_also_resets_timeout() {
        let start = ts(0);
        let mut manager = HeartbeatManager::new(custom_policy(20, 60), start);
        manager.record_heartbeat(ts(50_000));
        assert!(!manager.is_timed_out(ts(60_000)));
        assert!(manager.is_timed_out(ts(110_000)));
    }

    #[test]
    fn test_expires_at_is_last_activity_plus_timeout() {
        let start = ts(5_000);
        let manager = HeartbeatManager::new(custom_policy(20, 60), start);
        let expires = manager.expires_at().expect("must be representable");
        assert_eq!(expires, ts(65_000));
    }

    #[test]
    fn test_heartbeat_envelope_has_control_payload() {
        let env = heartbeat_envelope(0, "msg-1".to_owned(), 1, 1_000, String::new(), 999);
        match env.payload.expect("payload") {
            envelope::Payload::Heartbeat(hb) => assert_eq!(hb.client_time_unix_ms, 999),
            other => panic!("expected heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn test_heartbeat_ack_for_heartbeat_echoes_client_time() {
        let hb = Heartbeat {
            client_time_unix_ms: 12345,
        };
        let now = ts(1_700_000_000_000);
        let env = heartbeat_ack_for_heartbeat(0, "msg-2".to_owned(), 2, String::new(), &hb, now)
            .expect("must build ack");
        match env.payload.expect("payload") {
            envelope::Payload::HeartbeatAck(ack) => {
                assert_eq!(ack.client_time_unix_ms, 12345);
                assert_eq!(ack.server_time_unix_ms, 1_700_000_000_000);
            }
            other => panic!("expected heartbeat_ack, got {other:?}"),
        }
    }

    #[test]
    fn test_timeout_does_not_fire_prematurely_with_drift() {
        // Regression: ensure that a manager started at non-zero time does not
        // treat 0 as last_activity.
        let start = ts(1_000_000);
        let manager = HeartbeatManager::new(custom_policy(15, 45), start);
        // 44s after start -> not yet
        assert!(!manager.is_timed_out(ts(1_000_000 + 44_000)));
        assert!(!manager.is_timed_out(ts(1_000_000 + 44_999)));
        assert!(manager.is_timed_out(ts(1_000_000 + 45_000)));
    }
}
