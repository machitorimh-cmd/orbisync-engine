//! Test fixtures, fake adapters and the deterministic clock.
//!
//! Production crates depend on this crate through `dev-dependencies` only
//! (`repo-crate-conventions.md` §2.2), which
//! `scripts/check_architecture.py` verifies.
//!
//! The fakes let use cases be tested without I/O (`architecture.md` §9.5) and
//! the fixed clock makes time dependent behaviour deterministic
//! (specification §31.4).

// `testkit` is test-support code. Production crates depend on it through
// `dev-dependencies` only, which `scripts/check_architecture.py` verifies, so
// the production `panic!` ban does not apply to its failure paths.
#![allow(clippy::panic)]
#![allow(missing_docs)]

pub mod external_idp;
pub mod identity;
pub mod world;

use std::sync::Mutex;

use orbisync_application::{AuditEvent, AuditSink, HealthProbe};
use orbisync_domain::{Clock, Timestamp};

pub use identity::{
    FakeIdempotencyStore, FakeIdentityAdministrationStore, FakeIdentityRepository,
    FakeIdentityStore, FakeRealtimeTicketStore, insecure_test_codec,
};
pub use world::{
    AllowAllAuthorizer, AllowEntityOwnerAuthorizer, DenyAllAuthorizer, FakeInstanceMembershipStore,
    FakeWorldDirectoryStore,
};

/// [`Clock`] returning a value the test controls.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<Timestamp>,
}

impl FixedClock {
    /// Creates a clock pinned to `now`.
    #[must_use]
    pub fn new(now: Timestamp) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// Creates a clock pinned to a fixed, arbitrary instant in 2026.
    ///
    /// # Panics
    ///
    /// Panics if the compiled-in constant is not a valid timestamp, which
    /// cannot happen for the literal used here.
    #[must_use]
    pub fn fixed() -> Self {
        Self::new(
            Timestamp::from_unix_millis(1_785_481_200_000)
                .unwrap_or_else(|error| panic!("constant timestamp must be valid: {error}")),
        )
    }

    /// Moves the clock forward by `millis`.
    ///
    /// # Panics
    ///
    /// Panics if the resulting instant is not representable or if the internal
    /// lock is poisoned.
    pub fn advance_millis(&self, millis: i64) {
        let mut guard = self.now.lock().unwrap_or_else(|error| {
            panic!("fixed clock lock is poisoned: {error}");
        });
        let current = guard
            .to_unix_millis()
            .unwrap_or_else(|error| panic!("current instant must be representable: {error}"));
        *guard = Timestamp::from_unix_millis(current + millis)
            .unwrap_or_else(|error| panic!("advanced instant must be representable: {error}"));
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        *self
            .now
            .lock()
            .unwrap_or_else(|error| panic!("fixed clock lock is poisoned: {error}"))
    }
}

/// [`HealthProbe`] whose answer the test controls.
#[derive(Debug)]
pub struct FakeHealthProbe {
    name: &'static str,
    failure: Mutex<Option<String>>,
}

impl FakeHealthProbe {
    /// Creates a probe that reports healthy.
    #[must_use]
    pub const fn healthy(name: &'static str) -> Self {
        Self {
            name,
            failure: Mutex::new(None),
        }
    }

    /// Makes the probe report the given failure reason.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn fail_with(&self, reason: impl Into<String>) {
        let mut guard = self
            .failure
            .lock()
            .unwrap_or_else(|error| panic!("fake probe lock is poisoned: {error}"));
        *guard = Some(reason.into());
    }
}

#[async_trait::async_trait]
impl HealthProbe for FakeHealthProbe {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn check(&self) -> Result<(), String> {
        let guard = self
            .failure
            .lock()
            .unwrap_or_else(|error| panic!("fake probe lock is poisoned: {error}"));
        guard.clone().map_or(Ok(()), Err)
    }
}

/// [`AuditSink`] that records events in memory.
#[derive(Debug, Default)]
pub struct RecordingAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl RecordingAuditSink {
    /// Creates an empty sink.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Returns the recorded events.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(|error| panic!("audit sink lock is poisoned: {error}"))
            .clone()
    }
}

#[async_trait::async_trait]
impl AuditSink for RecordingAuditSink {
    async fn record(&self, event: AuditEvent) -> Result<(), String> {
        self.events
            .lock()
            .unwrap_or_else(|error| panic!("audit sink lock is poisoned: {error}"))
            .push(event);
        Ok(())
    }
}

/// Returns the default configuration, which every test may adjust.
#[must_use]
pub fn default_config() -> orbisync_config::Config {
    orbisync_config::Config::default()
}

#[cfg(test)]
mod tests {
    use super::{FakeHealthProbe, FixedClock, default_config};
    use orbisync_domain::Clock as _;

    #[test]
    fn test_fixed_clock_does_not_move_on_its_own() {
        let clock = FixedClock::fixed();
        assert_eq!(clock.now(), clock.now());
    }

    #[test]
    fn test_fixed_clock_advances_on_request() {
        let clock = FixedClock::fixed();
        let before = clock.now();
        clock.advance_millis(1_500);
        assert!(clock.now() > before);
    }

    #[test]
    fn test_fake_probe_reports_the_configured_failure() {
        let probe = FakeHealthProbe::healthy("database");
        probe.fail_with("injected");
        let failure = pollster::block_on(async {
            use orbisync_application::HealthProbe as _;
            probe.check().await
        });
        assert_eq!(failure, Err(String::from("injected")));
    }

    #[test]
    fn test_default_config_is_the_validated_default() {
        assert_eq!(default_config(), orbisync_config::Config::default());
    }
}
