//! Out-of-process extension gateway and eventing.
//!
//! Owns authentication towards external services, delivery, timeouts, retries,
//! and the internal distribution of generic events (`architecture.md` §3,
//! `extension-mechanism.md`). Extensions are always out-of-process
//! (specification §23).
//!
//! # Dependency rule
//!
//! `extensions` depends on `domain`, `application` and `config`
//! (`repo-crate-conventions.md` §3.2).
//!
//! WH1 defines event mapping, registration metadata and durable outbox input.
//! HTTP delivery, retries, backoff, circuit breaking and DLQ handling are
//! isolated here from domain state transitions.

pub mod command;
mod delivery;
mod precommit;

pub use delivery::{
    DeliveryEngine, DeliveryError, DeliveryOutcome, DeliveryWorker, DnsResolver, EnvSecretProvider,
    HttpClient, ReqwestHttpClient, SecretProvider, SystemDnsResolver, WebhookRequest,
    WebhookResponse, build_signed_webhook, full_jitter_delay,
};
pub use orbisync_application::ExtensionRegistrationStore;
pub use precommit::{
    PreCommitBodyClient, PreCommitDecision, PreCommitGate, PreCommitHttpResponse,
    PreCommitOperation, PreCommitValidationGate, PreCommitValidationPolicy,
    PreCommitValidationRequest, ReqwestPreCommitClient,
};

use orbisync_domain::{DomainError, DomainErrorKind};

pub use orbisync_application::PendingExtensionDelivery;
pub use orbisync_application::metrics::{ExtensionDeliveryResult, MetricsRecorder};
pub use orbisync_application::{
    ExtensionEvent, ExtensionRegistration, ExtensionStatus, PUBLIC_EVENT_KINDS,
};
pub use orbisync_config::ExtensionsConfig;

/// Selects only active registrations that subscribed to the event kind.
///
/// This is the delivery-target boundary; it does not perform HTTP I/O.
#[must_use]
pub fn select_subscribers<'a>(
    registrations: impl IntoIterator<Item = &'a ExtensionRegistration>,
    event: &ExtensionEvent,
) -> Vec<&'a ExtensionRegistration> {
    registrations
        .into_iter()
        .filter(|registration| registration.subscribes_to(event))
        .collect()
}

/// Bounded delivery policy for one extension endpoint.
///
/// Every bound is finite: an unbounded retry loop would turn a failing
/// extension into an unbounded resource consumer (`architecture.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryPolicy {
    max_concurrency: u32,
    timeout_ms: u32,
    max_attempts: u32,
    backoff_min_ms: u64,
    backoff_max_ms: u64,
    circuit_failure_threshold: u32,
    circuit_open_ms: u64,
    dlq_retention_days: u32,
    lease_ms: u64,
}

impl DeliveryPolicy {
    /// Validates and stores a delivery policy.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidValue`] when a bound is zero or
    /// cannot be represented by the persistence schema.
    pub fn new(timeout_ms: u32, max_attempts: u32) -> Result<Self, DomainError> {
        Self::new_with_concurrency(timeout_ms, max_attempts, 16)
    }

    /// Validates and stores a policy with an explicit process-wide concurrency
    /// limit. The upper bound prevents an accidental configuration value from
    /// creating an unreasonable number of in-flight network operations.
    pub fn new_with_concurrency(
        timeout_ms: u32,
        max_attempts: u32,
        max_concurrency: u32,
    ) -> Result<Self, DomainError> {
        if timeout_ms == 0 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension delivery timeout must be greater than 0",
            ));
        }
        if max_attempts == 0 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension delivery must allow at least one attempt",
            ));
        }
        if max_concurrency == 0 || max_concurrency > 1024 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension delivery concurrency must be between 1 and 1024",
            ));
        }
        if max_attempts > i32::MAX as u32 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension delivery attempts must fit the persistence schema",
            ));
        }
        Ok(Self {
            max_concurrency,
            timeout_ms,
            max_attempts,
            backoff_min_ms: 1_000,
            backoff_max_ms: 30_000,
            circuit_failure_threshold: 5,
            circuit_open_ms: 30_000,
            dlq_retention_days: 7,
            lease_ms: 60_000,
        })
    }

    /// Builds a policy from the validated application configuration.
    pub fn from_config(config: ExtensionsConfig) -> Result<Self, DomainError> {
        if config.max_attempts == 0
            || config.backoff_min_seconds == 0
            || config.backoff_max_seconds < config.backoff_min_seconds
            || config.circuit_failure_threshold == 0
            || config.circuit_open_seconds == 0
            || config.dlq_retention_days == 0
        {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension delivery configuration contains a zero or inconsistent bound",
            ));
        }
        let timeout_ms = config
            .delivery_timeout_seconds
            .checked_mul(1_000)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "extension delivery timeout does not fit milliseconds",
                )
            })?;
        let backoff_min_ms = config
            .backoff_min_seconds
            .checked_mul(1_000)
            .ok_or_else(|| {
                DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "extension backoff is too large",
                )
            })?;
        let backoff_max_ms = config
            .backoff_max_seconds
            .checked_mul(1_000)
            .ok_or_else(|| {
                DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "extension backoff is too large",
                )
            })?;
        let circuit_open_ms = config
            .circuit_open_seconds
            .checked_mul(1_000)
            .ok_or_else(|| {
                DomainError::new(
                    DomainErrorKind::InvalidValue,
                    "extension circuit duration is too large",
                )
            })?;
        if backoff_max_ms > i64::MAX as u64 || circuit_open_ms > i64::MAX as u64 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "extension duration does not fit the persistence time type",
            ));
        }
        let policy =
            Self::new_with_concurrency(timeout_ms, config.max_attempts, config.max_concurrency)?;
        Ok(Self {
            backoff_min_ms,
            backoff_max_ms,
            circuit_open_ms,
            lease_ms: timeout_ms as u64 + 30_000,
            ..policy
        })
    }

    /// Returns the process-wide maximum number of concurrent deliveries.
    #[must_use]
    pub const fn max_concurrency(self) -> u32 {
        self.max_concurrency
    }

    /// Overrides backoff values, primarily for deterministic tests.
    #[must_use]
    pub const fn with_backoff(self, min_ms: u64, max_ms: u64) -> Self {
        let upper = if max_ms < min_ms { min_ms } else { max_ms };
        Self {
            backoff_min_ms: min_ms,
            backoff_max_ms: upper,
            ..self
        }
    }

    /// Returns the full-jitter lower bound in milliseconds.
    #[must_use]
    pub const fn backoff_min_ms(self) -> u64 {
        self.backoff_min_ms
    }

    /// Returns the full-jitter upper bound in milliseconds.
    #[must_use]
    pub const fn backoff_max_ms(self) -> u64 {
        self.backoff_max_ms
    }

    /// Returns the consecutive-failure circuit threshold.
    #[must_use]
    pub const fn circuit_failure_threshold(self) -> u32 {
        self.circuit_failure_threshold
    }

    /// Returns circuit-open duration in milliseconds.
    #[must_use]
    pub const fn circuit_open_ms(self) -> u64 {
        self.circuit_open_ms
    }

    /// Returns the claim lease duration in milliseconds.
    #[must_use]
    pub const fn lease_ms(self) -> u64 {
        self.lease_ms
    }

    /// Returns dead-letter retention in days.
    #[must_use]
    pub const fn dlq_retention_days(self) -> u32 {
        self.dlq_retention_days
    }

    /// Returns the per attempt timeout in milliseconds.
    #[must_use]
    pub const fn timeout_ms(self) -> u32 {
        self.timeout_ms
    }

    /// Returns the maximum number of attempts, including the first one.
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        DeliveryPolicy, ExtensionEvent, ExtensionRegistration, ExtensionStatus, select_subscribers,
    };
    use orbisync_domain::DomainErrorKind;
    use orbisync_domain::UserId;

    #[test]
    fn test_valid_policy_is_accepted() {
        let policy = DeliveryPolicy::new(5_000, 3).expect("valid policy");
        assert_eq!(policy.timeout_ms(), 5_000);
        assert_eq!(policy.max_attempts(), 3);
    }

    #[test]
    fn test_zero_timeout_is_rejected() {
        assert_eq!(
            DeliveryPolicy::new(0, 3)
                .expect_err("timeout must be positive")
                .kind(),
            DomainErrorKind::InvalidValue
        );
    }

    #[test]
    fn test_zero_attempts_is_rejected() {
        assert_eq!(
            DeliveryPolicy::new(1_000, 0)
                .expect_err("at least one attempt is required")
                .kind(),
            DomainErrorKind::InvalidValue
        );
    }

    #[test]
    fn delivery_concurrency_is_bounded_at_construction() {
        assert_eq!(
            DeliveryPolicy::new_with_concurrency(1_000, 3, 0)
                .expect_err("zero concurrency must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
        assert_eq!(
            DeliveryPolicy::new_with_concurrency(1_000, 3, 1_025)
                .expect_err("excessive concurrency must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
    }

    #[test]
    fn subscription_filter_excludes_unsubscribed_and_suspended_extensions() {
        let event = ExtensionEvent::UserCreated {
            user_id: UserId::generate(),
        };
        let subscribed = ExtensionRegistration {
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("subscribed"),
            description: None,
            endpoint: String::from("https://example.invalid/webhook"),
            subscribed_events: BTreeSet::from([String::from("user.created")]),
            capabilities: BTreeSet::new(),
            token_scopes: BTreeSet::new(),
            status: ExtensionStatus::Active,
            signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_SUBSCRIBED"),
        };
        let other = ExtensionRegistration {
            subscribed_events: BTreeSet::from([String::from("user.disabled")]),
            status: ExtensionStatus::Active,
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("other"),
            description: None,
            endpoint: String::from("https://example.invalid/other"),
            capabilities: BTreeSet::new(),
            token_scopes: BTreeSet::new(),
            signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_OTHER"),
        };
        let suspended = ExtensionRegistration {
            status: ExtensionStatus::Suspended,
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("suspended"),
            description: None,
            endpoint: String::from("https://example.invalid/suspended"),
            subscribed_events: BTreeSet::from([String::from("user.created")]),
            capabilities: BTreeSet::new(),
            token_scopes: BTreeSet::new(),
            signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_SUSPENDED"),
        };
        let registrations = vec![subscribed.clone(), other, suspended];
        let targets = select_subscribers(&registrations, &event);
        assert_eq!(targets, vec![&subscribed]);
    }

    #[test]
    fn registration_rejects_raw_secret_values() {
        let registration = ExtensionRegistration {
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("invalid"),
            description: None,
            endpoint: String::from("https://example.invalid/webhook"),
            subscribed_events: BTreeSet::from([String::from("user.created")]),
            capabilities: BTreeSet::new(),
            token_scopes: BTreeSet::new(),
            status: ExtensionStatus::Active,
            signing_secret_ref: String::from("raw-secret-value"),
        };
        assert!(registration.validate().is_err());
    }
}
