//! Audit adapter.
//!
//! Milestone 1 persists audit records in PostgreSQL. Until then the adapter
//! writes them to the audit target of the structured log, which already keeps
//! real identifiers and rejects secrets (`observability-and-config.md` §5).

use orbisync_application::{AuditEvent, AuditSink};

/// [`AuditSink`] that emits one structured log record per audit event.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingAuditSink;

impl TracingAuditSink {
    /// Creates the adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl AuditSink for TracingAuditSink {
    async fn record(&self, event: AuditEvent) -> Result<(), String> {
        let occurred_at = event
            .occurred_at
            .to_rfc3339()
            .map_err(|error| error.to_string())?;
        let actor = event
            .actor_id
            .map_or_else(|| String::from("anonymous"), |actor| actor.to_string());
        tracing::info!(
            target: "orbisync::audit",
            event = event.action,
            occurred_at = occurred_at,
            actor_id = actor,
            succeeded = event.succeeded,
            "audit record"
        );
        Ok(())
    }
}
