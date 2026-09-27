//! Port traits implemented by outbound adapters.
//!
//! Ports are declared here and implemented elsewhere, so use cases can be
//! tested against fakes (`architecture.md` §2.1, §9.5). Port signatures use
//! domain types and application types only.

use orbisync_domain::{Timestamp, UserId};

/// Readiness of a backing store.
///
/// `/health/ready` reports 503 while the probe fails, and the process stays
/// live (specification §26.1, `deployment-and-threat-model.md` §1.3).
#[async_trait::async_trait]
pub trait HealthProbe: Send + Sync + 'static {
    /// Name reported in the readiness payload, for example `database`.
    fn name(&self) -> &'static str;

    /// Returns `Ok(())` when the dependency can serve requests.
    ///
    /// # Errors
    ///
    /// Returns a short, secret-free reason when the dependency is unavailable.
    /// Connection strings and driver messages must not be included
    /// (`observability-and-config.md` §2.3).
    async fn check(&self) -> Result<(), String>;
}

/// An administrative action recorded for audit (specification §27.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// Time the action was performed.
    pub occurred_at: Timestamp,
    /// Stable action name, for example `user.disable`.
    pub action: &'static str,
    /// User that performed the action, when the actor is authenticated.
    pub actor_id: Option<UserId>,
    /// Outcome of the action.
    pub succeeded: bool,
}

/// Sink for audit records.
///
/// Audit records keep real identifiers (`observability-and-config.md` §5) and
/// never carry passwords, tokens or full payloads.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync + 'static {
    /// Records one audit event.
    ///
    /// # Errors
    ///
    /// Returns a short, secret-free reason when the record cannot be stored.
    async fn record(&self, event: AuditEvent) -> Result<(), String>;
}
