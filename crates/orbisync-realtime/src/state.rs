//! Realtime connection state machine.
//!
//! `contracts/realtime-connection-state-machine.json` is the source of truth
//! (`realtime-protocol-and-connection.md`). The table below mirrors it, and
//! `tests/state_machine_contract.rs` compares both directions so a change to
//! either side fails CI.

use core::fmt;

/// State of a realtime connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// The WebSocket upgrade is in progress.
    Connecting,
    /// The upgrade succeeded and the server waits for `ClientHello`.
    AwaitingHello,
    /// The handshake completed; the connection has joined no instance.
    Ready,
    /// A `JoinInstance` request is being processed.
    Joining,
    /// The connection is a member of an instance and receives state.
    Active,
    /// A `ResumeSession` request is being processed.
    Resuming,
    /// A graceful close is draining.
    Closing,
    /// A fatal condition is being reported before closing.
    Failing,
    /// The connection is closed normally. Terminal.
    Closed,
    /// The connection is closed after a fatal condition. Terminal.
    Failed,
}

impl ConnectionState {
    /// State every connection starts in.
    pub const INITIAL: Self = Self::Connecting;

    /// Returns the contract spelling of the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::AwaitingHello => "awaiting_hello",
            Self::Ready => "ready",
            Self::Joining => "joining",
            Self::Active => "active",
            Self::Resuming => "resuming",
            Self::Closing => "closing",
            Self::Failing => "failing",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }

    /// Returns `true` when no further transition is possible.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Event that can drive a connection state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionEvent {
    /// Contract event `upgrade_succeeded`.
    UpgradeSucceeded,
    /// Contract event `upgrade_failed`.
    UpgradeFailed,
    /// Contract event `hello_accepted`.
    HelloAccepted,
    /// Contract event `version_negotiation_failed`.
    VersionNegotiationFailed,
    /// Contract event `invalid_token`.
    InvalidToken,
    /// Contract event `oversized_message`.
    OversizedMessage,
    /// Contract event `persistent_rate_limit`.
    PersistentRateLimit,
    /// Contract event `protocol_abuse`.
    ProtocolAbuse,
    /// Contract event `join_requested`.
    JoinRequested,
    /// Contract event `join_accepted`.
    JoinAccepted,
    /// Contract event `join_rejected`.
    JoinRejected,
    /// Contract event `resume_requested`.
    ResumeRequested,
    /// Contract event `resume_accepted`.
    ResumeAccepted,
    /// Contract event `resync_required`.
    ResyncRequired,
    /// Contract event `heartbeat_timeout`.
    HeartbeatTimeout,
    /// Contract event `transport_lost`.
    TransportLost,
    /// Contract event `close_requested`.
    CloseRequested,
    /// Contract event `graceful_close_completed`.
    GracefulCloseCompleted,
    /// Contract event `fatal_close_completed`.
    FatalCloseCompleted,
}

impl ConnectionEvent {
    /// Returns the contract spelling of the event.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpgradeSucceeded => "upgrade_succeeded",
            Self::UpgradeFailed => "upgrade_failed",
            Self::HelloAccepted => "hello_accepted",
            Self::VersionNegotiationFailed => "version_negotiation_failed",
            Self::InvalidToken => "invalid_token",
            Self::OversizedMessage => "oversized_message",
            Self::PersistentRateLimit => "persistent_rate_limit",
            Self::ProtocolAbuse => "protocol_abuse",
            Self::JoinRequested => "join_requested",
            Self::JoinAccepted => "join_accepted",
            Self::JoinRejected => "join_rejected",
            Self::ResumeRequested => "resume_requested",
            Self::ResumeAccepted => "resume_accepted",
            Self::ResyncRequired => "resync_required",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::TransportLost => "transport_lost",
            Self::CloseRequested => "close_requested",
            Self::GracefulCloseCompleted => "graceful_close_completed",
            Self::FatalCloseCompleted => "fatal_close_completed",
        }
    }
}

impl fmt::Display for ConnectionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every transition defined by the contract, as `(from, event, to)`.
pub const TRANSITIONS: &[(ConnectionState, ConnectionEvent, ConnectionState)] = &[
    (
        ConnectionState::Connecting,
        ConnectionEvent::UpgradeSucceeded,
        ConnectionState::AwaitingHello,
    ),
    (
        ConnectionState::Connecting,
        ConnectionEvent::UpgradeFailed,
        ConnectionState::Failed,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::HelloAccepted,
        ConnectionState::Ready,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::VersionNegotiationFailed,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::InvalidToken,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::OversizedMessage,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::PersistentRateLimit,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::ProtocolAbuse,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::AwaitingHello,
        ConnectionEvent::TransportLost,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::JoinRequested,
        ConnectionState::Joining,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::ResumeRequested,
        ConnectionState::Resuming,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::OversizedMessage,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::PersistentRateLimit,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::ProtocolAbuse,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::HeartbeatTimeout,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::TransportLost,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Ready,
        ConnectionEvent::CloseRequested,
        ConnectionState::Closing,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::JoinAccepted,
        ConnectionState::Active,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::JoinRejected,
        ConnectionState::Ready,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::OversizedMessage,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::PersistentRateLimit,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::ProtocolAbuse,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::HeartbeatTimeout,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::TransportLost,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Joining,
        ConnectionEvent::CloseRequested,
        ConnectionState::Closing,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::ResyncRequired,
        ConnectionState::Active,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::OversizedMessage,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::PersistentRateLimit,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::ProtocolAbuse,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::HeartbeatTimeout,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::TransportLost,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Active,
        ConnectionEvent::CloseRequested,
        ConnectionState::Closing,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::ResumeAccepted,
        ConnectionState::Active,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::ResyncRequired,
        ConnectionState::Ready,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::OversizedMessage,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::PersistentRateLimit,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::ProtocolAbuse,
        ConnectionState::Failing,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::HeartbeatTimeout,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::TransportLost,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Resuming,
        ConnectionEvent::CloseRequested,
        ConnectionState::Closing,
    ),
    (
        ConnectionState::Closing,
        ConnectionEvent::GracefulCloseCompleted,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Closing,
        ConnectionEvent::TransportLost,
        ConnectionState::Closed,
    ),
    (
        ConnectionState::Failing,
        ConnectionEvent::FatalCloseCompleted,
        ConnectionState::Failed,
    ),
    (
        ConnectionState::Failing,
        ConnectionEvent::TransportLost,
        ConnectionState::Failed,
    ),
];

/// Applies `event` to `from`.
///
/// Returns `None` when the contract defines no transition, which the gateway
/// treats as a protocol error rather than an implicit state change.
#[must_use]
pub fn transition(from: ConnectionState, event: ConnectionEvent) -> Option<ConnectionState> {
    TRANSITIONS
        .iter()
        .find(|(state, candidate, _)| *state == from && *candidate == event)
        .map(|(_, _, next)| *next)
}

#[cfg(test)]
mod tests {
    use super::{ConnectionEvent, ConnectionState, TRANSITIONS, transition};

    #[test]
    fn test_initial_state_is_connecting() {
        assert_eq!(ConnectionState::INITIAL, ConnectionState::Connecting);
        assert!(!ConnectionState::INITIAL.is_terminal());
    }

    #[test]
    fn test_terminal_states_have_no_outgoing_transition() {
        for (from, _, _) in TRANSITIONS {
            assert!(
                !from.is_terminal(),
                "terminal state {from} must not have an outgoing transition"
            );
        }
    }

    #[test]
    fn test_handshake_path_reaches_active() {
        let mut state = ConnectionState::INITIAL;
        for event in [
            ConnectionEvent::UpgradeSucceeded,
            ConnectionEvent::HelloAccepted,
            ConnectionEvent::JoinRequested,
            ConnectionEvent::JoinAccepted,
        ] {
            state = transition(state, event).expect("contract defines this transition");
        }
        assert_eq!(state, ConnectionState::Active);
    }

    #[test]
    fn test_undefined_transition_returns_none() {
        assert_eq!(
            transition(ConnectionState::Connecting, ConnectionEvent::JoinRequested),
            None
        );
    }
}
