//! D-3 regression coverage for the declared audit retention policy.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use orbisync_config::Config;

#[test]
fn audit_retention_days_has_a_safe_default_and_is_overridable() {
    let mut config = Config::default();
    assert_eq!(config.observability.audit_retention_days, 365);

    config
        .apply("observability.audit_retention_days", "730")
        .expect("audit retention override is accepted");
    assert_eq!(config.observability.audit_retention_days, 730);
    config
        .validate()
        .expect("positive audit retention is valid");
}

#[test]
fn audit_retention_days_rejects_zero() {
    let mut config = Config::default();
    config
        .apply("observability.audit_retention_days", "0")
        .expect("zero parses before validation");
    assert!(config.validate().is_err());
}

#[test]
fn audit_retention_days_rejects_values_above_database_bound() {
    let mut config = Config::default();
    config
        .apply("observability.audit_retention_days", "3651")
        .expect("large value parses before validation");
    assert!(config.validate().is_err());
}
