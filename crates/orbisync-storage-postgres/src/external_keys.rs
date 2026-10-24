//! Audited local identity-provider key publication.
use orbisync_application::{ExternalKeyAuditStore, ExternalKeyReloadAudit, IdentityPortError};
use sqlx::PgPool;
/// Persists key reload evidence without key material or file paths.
#[derive(Debug, Clone)]
pub struct PgExternalKeyAuditStore {
    pool: PgPool,
}
impl PgExternalKeyAuditStore {
    /// Uses the deployment's existing audit table and permission boundary.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}
#[async_trait::async_trait]
impl ExternalKeyAuditStore for PgExternalKeyAuditStore {
    async fn record_reload(&self, event: ExternalKeyReloadAudit) -> Result<(), IdentityPortError> {
        sqlx::query("INSERT INTO audit_events (id,occurred_at,actor_user_id,action,target_type,target_id,request_id,result,metadata) VALUES ($1,$2,$3,'auth.external_keys.reload','identity_provider','external',$4,$5,$6)")
            .bind(uuid::Uuid::now_v7()).bind(event.occurred_at.as_offset_date_time())
            .bind(event.actor_id.as_uuid()).bind(event.request_id.as_str())
            .bind(if event.succeeded { "success" } else { "failure" })
            .bind(serde_json::json!({"generation":event.generation,"key_count":event.key_count}))
            .execute(&self.pool).await.map_err(|_| IdentityPortError::Unavailable)?;
        Ok(())
    }
}
