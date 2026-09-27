//! Time values.
//!
//! Specification §31.4 fixes the representations: UTC for persistence, RFC 3339
//! for the REST API, and explicit Unix milliseconds for the realtime protocol.
//! [`Timestamp`] is the single domain type behind all three; adapters convert
//! at the boundary.

use core::fmt;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{DomainError, DomainErrorKind};

/// An instant in UTC.
///
/// Construction never reads the system clock; obtain the current time from a
/// [`Clock`](crate::clock::Clock) so tests stay deterministic (specification
/// §31.4, TD-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// Wraps an [`OffsetDateTime`], normalising it to UTC.
    #[must_use]
    pub fn from_offset_date_time(value: OffsetDateTime) -> Self {
        Self(value.to_offset(time::UtcOffset::UTC))
    }

    /// Builds a timestamp from Unix milliseconds, the realtime protocol form.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidTimestamp`] when the value is outside
    /// the representable range.
    pub fn from_unix_millis(millis: i64) -> Result<Self, DomainError> {
        let nanos = i128::from(millis)
            .checked_mul(1_000_000)
            .ok_or_else(|| invalid_timestamp("unix milliseconds overflow"))?;
        OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .map(Self)
            .map_err(|_| invalid_timestamp("unix milliseconds out of range"))
    }

    /// Returns the instant as Unix milliseconds, truncating sub-millisecond
    /// precision towards negative infinity.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidTimestamp`] when the instant does not
    /// fit into `i64` milliseconds.
    pub fn to_unix_millis(self) -> Result<i64, DomainError> {
        let millis = self.0.unix_timestamp_nanos().div_euclid(1_000_000);
        i64::try_from(millis).map_err(|_| invalid_timestamp("instant exceeds i64 milliseconds"))
    }

    /// Renders the instant as an RFC 3339 string, the REST API form.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidTimestamp`] when the instant cannot be
    /// formatted.
    pub fn to_rfc3339(self) -> Result<String, DomainError> {
        self.0
            .format(&Rfc3339)
            .map_err(|_| invalid_timestamp("instant is not representable as RFC 3339"))
    }

    /// Parses an RFC 3339 string into a UTC instant.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidTimestamp`] when the input is not a
    /// valid RFC 3339 timestamp.
    pub fn parse_rfc3339(value: &str) -> Result<Self, DomainError> {
        OffsetDateTime::parse(value, &Rfc3339)
            .map(Self::from_offset_date_time)
            .map_err(|_| invalid_timestamp("value is not a valid RFC 3339 timestamp"))
    }

    /// Returns the underlying [`OffsetDateTime`] in UTC for adapter conversions.
    #[must_use]
    pub const fn as_offset_date_time(self) -> OffsetDateTime {
        self.0
    }

    /// Returns a timestamp advanced by the given milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidTimestamp`] on arithmetic overflow.
    pub fn checked_add_millis(self, millis: i64) -> Result<Self, DomainError> {
        self.0
            .checked_add(time::Duration::milliseconds(millis))
            .map(Self)
            .ok_or_else(|| invalid_timestamp("timestamp addition overflow"))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.format(&Rfc3339) {
            Ok(rendered) => f.write_str(&rendered),
            Err(_) => f.write_str("<unrepresentable timestamp>"),
        }
    }
}

fn invalid_timestamp(detail: &'static str) -> DomainError {
    DomainError::new(DomainErrorKind::InvalidTimestamp, detail)
}

#[cfg(test)]
mod tests {
    use super::Timestamp;
    use crate::error::DomainErrorKind;

    #[test]
    fn test_unix_millis_round_trip() {
        let millis = 1_767_225_600_123_i64;
        let timestamp = Timestamp::from_unix_millis(millis).expect("value is in range");
        assert_eq!(timestamp.to_unix_millis().expect("fits in i64"), millis);
    }

    #[test]
    fn test_rfc3339_round_trip_is_utc() {
        let parsed = Timestamp::parse_rfc3339("2026-08-01T09:30:00+09:00").expect("valid input");
        assert_eq!(
            parsed.to_rfc3339().expect("formattable"),
            "2026-08-01T00:30:00Z"
        );
    }

    #[test]
    fn test_parse_rfc3339_rejects_invalid_input() {
        let error = Timestamp::parse_rfc3339("2026-08-01 09:30:00").expect_err("must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidTimestamp);
    }

    #[test]
    fn test_from_unix_millis_rejects_out_of_range() {
        let error = Timestamp::from_unix_millis(i64::MAX).expect_err("must be rejected");
        assert_eq!(error.kind(), DomainErrorKind::InvalidTimestamp);
    }

    #[test]
    fn test_display_matches_rfc3339() {
        let timestamp = Timestamp::from_unix_millis(0).expect("epoch is valid");
        assert_eq!(timestamp.to_string(), "1970-01-01T00:00:00Z");
    }
}
