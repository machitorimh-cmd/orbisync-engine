//! Connection level policies derived from configuration.

use orbisync_config::RealtimeConfig;
use orbisync_domain::RealtimeConnectionId;

/// Heartbeat and liveness policy for a realtime connection (MRIB §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatPolicy {
    interval_seconds: u64,
    timeout_seconds: u64,
}

impl HeartbeatPolicy {
    /// Derives the policy from validated configuration.
    ///
    /// [`orbisync_config::Config::validate`] already guarantees that the
    /// timeout exceeds the interval, so this conversion cannot fail.
    #[must_use]
    pub const fn from_config(config: &RealtimeConfig) -> Self {
        Self {
            interval_seconds: config.heartbeat_interval_seconds,
            timeout_seconds: config.connection_timeout_seconds,
        }
    }

    /// Returns the interval between server heartbeats, in seconds.
    #[must_use]
    pub const fn interval_seconds(self) -> u64 {
        self.interval_seconds
    }

    /// Returns the idle timeout, in seconds.
    #[must_use]
    pub const fn timeout_seconds(self) -> u64 {
        self.timeout_seconds
    }

    /// Returns the heartbeat interval advertised in `ServerHello`, in
    /// milliseconds (`realtime-protocol-and-connection.md`).
    #[must_use]
    pub const fn interval_millis(self) -> u64 {
        self.interval_seconds * 1_000
    }
}

/// Identity of one accepted realtime connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionDescriptor {
    /// Identifier reported to the client in `ServerHello` and used as the
    /// `connection_id` log field.
    pub connection_id: RealtimeConnectionId,
    /// Negotiated protocol minor version.
    pub protocol_minor: u32,
}

/// Returns the WebSocket subprotocol the gateway advertises (ADR-001).
#[must_use]
pub const fn binary_subprotocol() -> &'static str {
    orbisync_protocol::WEBSOCKET_SUBPROTOCOL
}

#[cfg(test)]
mod tests {
    use super::{HeartbeatPolicy, binary_subprotocol};
    use orbisync_config::Config;

    #[test]
    fn test_policy_follows_configuration() {
        let config = Config::default();
        let policy = HeartbeatPolicy::from_config(&config.realtime);
        assert_eq!(policy.interval_seconds(), 20);
        assert_eq!(policy.timeout_seconds(), 60);
        assert_eq!(policy.interval_millis(), 20_000);
    }

    #[test]
    fn test_subprotocol_matches_the_protocol_crate() {
        assert_eq!(binary_subprotocol(), "orbisync.v1.protobuf");
    }
}
