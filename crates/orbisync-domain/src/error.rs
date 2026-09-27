//! Domain error taxonomy.
//!
//! Domain errors are part of the stable contract (specification §26.5). They
//! are typed (§31.1), carry a machine readable kind, and never embed transport
//! concerns. Adapters map [`DomainErrorKind`] onto the public REST error codes
//! in `openapi/errors.yaml` and onto realtime `ErrorMessage` codes; the domain
//! never names an HTTP status.

use core::fmt;

/// Machine readable classification of a [`DomainError`].
///
/// The variants are stable: renaming one is a breaking change to the public
/// error contract. Log records use [`DomainErrorKind::as_str`] as the
/// `error_code` field (`observability-and-config.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DomainErrorKind {
    /// A value did not satisfy a domain invariant.
    InvalidValue,
    /// An identifier was not a canonical lowercase hyphenated UUID.
    InvalidIdentifier,
    /// A timestamp was outside the representable or accepted range.
    InvalidTimestamp,
    /// A revision counter reached its upper bound.
    RevisionOverflow,
    /// An optimistic concurrency check failed.
    RevisionMismatch,
}

impl DomainErrorKind {
    /// Returns the stable snake_case code used in logs and adapter mappings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidValue => "invalid_value",
            Self::InvalidIdentifier => "invalid_identifier",
            Self::InvalidTimestamp => "invalid_timestamp",
            Self::RevisionOverflow => "revision_overflow",
            Self::RevisionMismatch => "revision_mismatch",
        }
    }
}

impl fmt::Display for DomainErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A violated domain invariant.
///
/// The message is human oriented and carries no compatibility guarantee; the
/// [`DomainError::kind`] is the machine readable part (specification §21.8).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {detail}")]
pub struct DomainError {
    kind: DomainErrorKind,
    detail: String,
}

impl DomainError {
    /// Creates a domain error of the given kind.
    #[must_use]
    pub fn new(kind: DomainErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// Returns the machine readable classification.
    #[must_use]
    pub const fn kind(&self) -> DomainErrorKind {
        self.kind
    }

    /// Returns the human oriented detail message.
    ///
    /// The detail never contains secrets, tokens or full payloads
    /// (`observability-and-config.md` §2.3).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[cfg(test)]
mod tests {
    use super::{DomainError, DomainErrorKind};

    #[test]
    fn test_domain_error_kind_codes_are_stable() {
        assert_eq!(DomainErrorKind::InvalidValue.as_str(), "invalid_value");
        assert_eq!(
            DomainErrorKind::InvalidIdentifier.as_str(),
            "invalid_identifier"
        );
        assert_eq!(
            DomainErrorKind::InvalidTimestamp.as_str(),
            "invalid_timestamp"
        );
        assert_eq!(
            DomainErrorKind::RevisionOverflow.as_str(),
            "revision_overflow"
        );
        assert_eq!(
            DomainErrorKind::RevisionMismatch.as_str(),
            "revision_mismatch"
        );
    }

    #[test]
    fn test_domain_error_display_includes_kind_and_detail() {
        let error = DomainError::new(DomainErrorKind::InvalidValue, "capacity must be positive");
        assert_eq!(
            error.to_string(),
            "invalid_value: capacity must be positive"
        );
        assert_eq!(error.kind(), DomainErrorKind::InvalidValue);
        assert_eq!(error.detail(), "capacity must be positive");
    }
}
