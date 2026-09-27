//! Protocol version negotiation.
//!
//! The major version is part of the WebSocket subprotocol and never negotiated;
//! a mismatch closes the connection. The minor version is negotiated inside the
//! handshake: the server picks the highest minor supported by both sides
//! (ADR-004, `realtime-protocol-and-connection.md`).

/// Lowest minor version this server accepts.
pub const SUPPORTED_MINOR_MIN: u32 = 0;

/// Highest minor version this server implements.
pub const SUPPORTED_MINOR_MAX: u32 = 0;

/// Reason a handshake could not agree on a protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NegotiationError {
    /// The client advertised a range whose bounds are inverted.
    #[error("client advertised an inverted minor version range")]
    InvertedRange,
    /// The client and server ranges do not overlap.
    #[error("no common protocol minor version")]
    NoCommonMinor,
}

/// Selects the highest minor version supported by both peers.
///
/// # Errors
///
/// Returns [`NegotiationError::InvertedRange`] when `client_min` is greater
/// than `client_max`, and [`NegotiationError::NoCommonMinor`] when the ranges
/// do not overlap.
pub fn negotiate_minor(client_min: u32, client_max: u32) -> Result<u32, NegotiationError> {
    if client_min > client_max {
        return Err(NegotiationError::InvertedRange);
    }
    let supported = SUPPORTED_MINOR_MIN..=SUPPORTED_MINOR_MAX;
    let advertised = client_min..=client_max;
    if advertised.contains(&SUPPORTED_MINOR_MAX) {
        Ok(SUPPORTED_MINOR_MAX)
    } else if supported.contains(&client_max) {
        Ok(client_max)
    } else {
        Err(NegotiationError::NoCommonMinor)
    }
}

#[cfg(test)]
mod tests {
    use super::{NegotiationError, SUPPORTED_MINOR_MAX, negotiate_minor};

    #[test]
    fn test_negotiate_minor_picks_highest_common_version() {
        assert_eq!(negotiate_minor(0, 5), Ok(SUPPORTED_MINOR_MAX));
        assert_eq!(negotiate_minor(0, 0), Ok(0));
    }

    #[test]
    fn test_negotiate_minor_rejects_inverted_range() {
        assert_eq!(negotiate_minor(3, 1), Err(NegotiationError::InvertedRange));
    }

    #[test]
    fn test_negotiate_minor_rejects_disjoint_range() {
        assert_eq!(
            negotiate_minor(SUPPORTED_MINOR_MAX + 1, SUPPORTED_MINOR_MAX + 2),
            Err(NegotiationError::NoCommonMinor)
        );
    }
}
