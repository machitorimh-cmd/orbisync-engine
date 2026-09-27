//! Single-use realtime connection ticket secret value.

use core::fmt;

use orbisync_application::SecretString;

/// Opaque realtime ticket that cannot reveal its value through formatting.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionTicket(SecretString);

impl ConnectionTicket {
    /// Wraps an issued opaque ticket.
    #[must_use]
    pub fn new(value: SecretString) -> Self {
        Self(value)
    }

    /// Exposes the ticket only at the ClientHello boundary.
    #[must_use]
    pub const fn secret(&self) -> &SecretString {
        &self.0
    }
}

impl fmt::Debug for ConnectionTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionTicket([REDACTED])")
    }
}

impl fmt::Display for ConnectionTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectionTicket;
    use orbisync_application::SecretString;

    #[test]
    fn ticket_formatting_is_redacted() {
        let ticket = ConnectionTicket::new(SecretString::new("ticket-secret"));
        assert_eq!(ticket.to_string(), "[REDACTED]");
        assert!(!format!("{ticket:?}").contains("ticket-secret"));
    }
}
