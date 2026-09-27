//! Application error taxonomy.
//!
//! Use cases return typed errors (specification §31.1). Adapter failures reach
//! the application as [`ApplicationErrorKind::PortFailure`] with a redacted
//! message: the concrete driver error never crosses the port boundary, so
//! internal database or library details cannot leak into a public response
//! (specification §21.8).

use core::fmt;

use orbisync_domain::DomainError;

/// Machine readable classification of an [`ApplicationError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApplicationErrorKind {
    /// Authentication failed without revealing whether the account exists.
    Unauthenticated,
    /// A bounded security or request rate limit rejected the operation.
    RateLimited,
    /// A domain invariant was violated.
    DomainRule,
    /// The caller is not permitted to perform the operation.
    NotAuthorized,
    /// The requested resource does not exist.
    NotFound,
    /// The operation conflicts with the current state.
    Conflict,
    /// An outbound port (database, extension, telemetry) failed.
    PortFailure,
    /// Authoritative state is temporarily fenced or its owner is unavailable.
    Unavailable,
    /// A durable payload exceeded its configured size limit and was refused.
    CheckpointTooLarge,
    /// Generation admission has no state or outcome capacity; no mutation ran.
    CheckpointCapacity,
    /// Exact generation committed, but its original replay window elapsed.
    CommittedButExpired,
}

impl ApplicationErrorKind {
    /// Returns the stable snake_case code used in logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::RateLimited => "rate_limited",
            Self::DomainRule => "domain_rule",
            Self::NotAuthorized => "not_authorized",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PortFailure => "port_failure",
            Self::Unavailable => "unavailable",
            Self::CheckpointTooLarge => "checkpoint_too_large",
            Self::CheckpointCapacity => "checkpoint_capacity",
            Self::CommittedButExpired => "committed_but_expired",
        }
    }
}

impl fmt::Display for ApplicationErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failed use case invocation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {detail}")]
pub struct ApplicationError {
    kind: ApplicationErrorKind,
    detail: String,
}

impl ApplicationError {
    /// Creates an application error of the given kind.
    #[must_use]
    pub fn new(kind: ApplicationErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// Creates a [`ApplicationErrorKind::PortFailure`].
    ///
    /// Adapters pass a short, secret-free summary; the underlying driver error
    /// is logged by the adapter and is not carried further
    /// (`observability-and-config.md` §2.3).
    #[must_use]
    pub fn port_failure(detail: impl Into<String>) -> Self {
        Self::new(ApplicationErrorKind::PortFailure, detail)
    }

    /// Returns the machine readable classification.
    #[must_use]
    pub const fn kind(&self) -> ApplicationErrorKind {
        self.kind
    }

    /// Returns the human oriented detail message.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl From<DomainError> for ApplicationError {
    fn from(error: DomainError) -> Self {
        Self::new(ApplicationErrorKind::DomainRule, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplicationError, ApplicationErrorKind};
    use orbisync_domain::{DomainError, DomainErrorKind};

    #[test]
    fn test_domain_error_maps_to_domain_rule() {
        let domain = DomainError::new(DomainErrorKind::InvalidValue, "bad value");
        let application = ApplicationError::from(domain);
        assert_eq!(application.kind(), ApplicationErrorKind::DomainRule);
        assert!(application.detail().contains("bad value"));
    }

    #[test]
    fn test_port_failure_keeps_kind() {
        let error = ApplicationError::port_failure("database unavailable");
        assert_eq!(error.kind(), ApplicationErrorKind::PortFailure);
        assert_eq!(error.to_string(), "port_failure: database unavailable");
    }
}
