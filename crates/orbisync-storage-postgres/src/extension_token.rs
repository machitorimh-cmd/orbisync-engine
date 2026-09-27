//! Digest-only service tokens. Rotation and authorization use current manifests.
use orbisync_application::extension_command::{ExtensionGrant, ExtensionTokenStore};
use orbisync_application::{ApplicationError, ApplicationErrorKind};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::BTreeSet;
use time::OffsetDateTime;
use uuid::Uuid;

/// PostgreSQL extension credential adapter.
#[derive(Clone)]
pub struct PgExtensionTokenStore {
    pool: PgPool,
}
impl PgExtensionTokenStore {
    /// Uses the caller's bounded database pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}
fn unavailable(_: sqlx::Error) -> ApplicationError {
    ApplicationError::port_failure("extension credential store unavailable")
}
fn strings(value: Value) -> Result<BTreeSet<String>, ApplicationError> {
    serde_json::from_value(value)
        .map_err(|_| ApplicationError::port_failure("invalid extension grants"))
}

#[async_trait::async_trait]
impl ExtensionTokenStore for PgExtensionTokenStore {
    async fn replace(
        &self,
        extension_id: Uuid,
        digest: &[u8],
        scopes: &BTreeSet<String>,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row: Option<(String, Value, Value)> = sqlx::query_as("SELECT status, capabilities, token_scopes FROM extension_registrations WHERE extension_id=$1 FOR UPDATE")
            .bind(extension_id).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        let (status, capabilities, allowed) = row.ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::NotFound, "extension not registered")
        })?;
        let capabilities = strings(capabilities)?;
        let allowed = strings(allowed)?;
        if status != "active"
            || !scopes.is_subset(&allowed)
            || scopes
                .iter()
                .filter(|scope| scope.starts_with("commands:"))
                .any(|scope| !capabilities.contains(scope))
        {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "extension grants denied",
            ));
        }
        sqlx::query("INSERT INTO extension_service_tokens (extension_id,token_digest,scopes,issued_at,expires_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (extension_id) DO UPDATE SET token_digest=EXCLUDED.token_digest,scopes=EXCLUDED.scopes,issued_at=EXCLUDED.issued_at,expires_at=EXCLUDED.expires_at")
            .bind(extension_id).bind(digest).bind(serde_json::json!(scopes)).bind(now).bind(expires_at).execute(&mut *tx).await.map_err(unavailable)?;
        sqlx::query("INSERT INTO audit_events (id,occurred_at,action,target_type,target_id,result,metadata) VALUES ($1,CURRENT_TIMESTAMP,'extension.token_rotated','extension',$2,'success','{}'::jsonb)")
            .bind(Uuid::now_v7()).bind(extension_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)
    }
    async fn resolve(
        &self,
        digest: &[u8],
        now: OffsetDateTime,
    ) -> Result<Option<ExtensionGrant>, ApplicationError> {
        let row: Option<(Uuid, Value, Value, Value)> = sqlx::query_as("SELECT t.extension_id,r.capabilities,r.token_scopes,t.scopes FROM extension_service_tokens t JOIN extension_registrations r USING (extension_id) WHERE t.token_digest=$1 AND t.issued_at <= $2 AND t.expires_at > $2 AND r.status='active'")
            .bind(digest).bind(now).fetch_optional(&self.pool).await.map_err(unavailable)?;
        row.map(|(extension_id, capabilities, allowed, issued)| {
            let allowed = strings(allowed)?;
            let issued = strings(issued)?;
            Ok(ExtensionGrant {
                extension_id,
                capabilities: strings(capabilities)?,
                scopes: allowed.intersection(&issued).cloned().collect(),
            })
        })
        .transpose()
    }
    async fn revoke(&self, extension_id: Uuid) -> Result<(), ApplicationError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        // Same lock order as issuance prevents a rotate/revoke race from reusing a token.
        sqlx::query(
            "SELECT extension_id FROM extension_registrations WHERE extension_id=$1 FOR UPDATE",
        )
        .bind(extension_id)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        sqlx::query("DELETE FROM extension_service_tokens WHERE extension_id=$1")
            .bind(extension_id)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        sqlx::query("INSERT INTO audit_events (id,occurred_at,action,target_type,target_id,result,metadata) VALUES ($1,CURRENT_TIMESTAMP,'extension.token_revoked','extension',$2,'success','{}'::jsonb)")
            .bind(Uuid::now_v7()).bind(extension_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)
    }
}
