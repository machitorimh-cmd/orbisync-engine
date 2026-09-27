//! Conversions between wire strings and domain identifiers.
//!
//! Protocol and domain types are converted explicitly (specification §7.3); the
//! conversion is the detection point for compatibility changes
//! (`architecture.md` §3.2). Optional identifier fields are empty strings on
//! the wire because proto3 scalars have no presence.

use orbisync_domain::{DomainError, InstanceId};

/// Parses the optional `Envelope.instance_id` field.
///
/// An empty string means "no instance" and yields `Ok(None)`.
///
/// # Errors
///
/// Returns a [`DomainError`] when the value is neither empty nor a canonical
/// UUIDv7 string.
pub fn parse_optional_instance_id(value: &str) -> Result<Option<InstanceId>, DomainError> {
    if value.is_empty() {
        return Ok(None);
    }
    InstanceId::parse(value).map(Some)
}

/// Renders an optional instance identifier for the wire.
#[must_use]
pub fn render_optional_instance_id(value: Option<InstanceId>) -> String {
    value.map(|id| id.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{parse_optional_instance_id, render_optional_instance_id};
    use orbisync_domain::{DomainErrorKind, InstanceId};

    #[test]
    fn test_empty_instance_id_is_absent() {
        assert_eq!(
            parse_optional_instance_id("").expect("empty is valid"),
            None
        );
        assert_eq!(render_optional_instance_id(None), "");
    }

    #[test]
    fn test_instance_id_round_trips() {
        let id = InstanceId::generate();
        let rendered = render_optional_instance_id(Some(id));
        assert_eq!(
            parse_optional_instance_id(&rendered).expect("canonical string"),
            Some(id)
        );
    }

    #[test]
    fn test_malformed_instance_id_is_rejected() {
        let error = parse_optional_instance_id("nope").expect_err("must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidIdentifier);
    }
}
