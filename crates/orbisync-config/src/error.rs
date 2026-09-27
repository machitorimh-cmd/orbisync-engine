//! Configuration error taxonomy.

use core::fmt;

/// Machine readable classification of a [`ConfigError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConfigErrorKind {
    /// The configuration file could not be read.
    FileUnreadable,
    /// The configuration file is not valid TOML.
    Syntax,
    /// A key is not part of the configuration schema.
    UnknownKey,
    /// A value has the wrong type or is outside the accepted range.
    InvalidValue,
    /// Two settings contradict each other.
    Inconsistent,
    /// A required secret is not present in the environment.
    MissingSecret,
}

impl ConfigErrorKind {
    /// Returns the stable snake_case code used in logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileUnreadable => "file_unreadable",
            Self::Syntax => "syntax",
            Self::UnknownKey => "unknown_key",
            Self::InvalidValue => "invalid_value",
            Self::Inconsistent => "inconsistent",
            Self::MissingSecret => "missing_secret",
        }
    }
}

impl fmt::Display for ConfigErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A configuration problem detected during startup validation.
///
/// The message states which key failed which check and why
/// (`observability-and-config.md` §6.2). It never contains a secret value: only
/// the name of the environment variable that should carry it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} at `{key}`: {detail}")]
pub struct ConfigError {
    kind: ConfigErrorKind,
    key: String,
    detail: String,
}

impl ConfigError {
    /// Creates a configuration error.
    #[must_use]
    pub fn new(kind: ConfigErrorKind, key: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
            detail: detail.into(),
        }
    }

    /// Returns the machine readable classification.
    #[must_use]
    pub const fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// Returns the configuration key or environment variable that failed.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the human oriented explanation.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}
