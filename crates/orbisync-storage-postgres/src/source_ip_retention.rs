//! Retention operations for privacy-sensitive audit source IPs.

use sqlx::PgPool;
use time::OffsetDateTime;

/// PostgreSQL adapter for bounded source-IP retention cleanup.
#[derive(Debug, Clone)]
pub struct PgAuditSourceIpRetentionStore {
    pool: PgPool,
}

impl PgAuditSourceIpRetentionStore {
    /// Creates a source-IP retention adapter backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Deletes at most `limit` source-IP rows older than `before`.
    pub async fn delete_expired(
        &self,
        before: OffsetDateTime,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        // Runtime may delete expired source IPs but must never UPDATE them.
        // SELECT FOR UPDATE requires UPDATE privilege even on an empty table.
        // DELETE itself locks its candidates; bound contention between workers.
        sqlx::query("SET LOCAL lock_timeout = '1s'")
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query(
            "WITH expired AS (SELECT audit_event_id FROM audit_source_ips WHERE created_at < $1 ORDER BY created_at ASC, audit_event_id ASC LIMIT $2) DELETE FROM audit_source_ips AS source_ips USING expired WHERE source_ips.audit_event_id = expired.audit_event_id",
        )
        .bind(before)
        .bind(limit.max(1))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected())
    }
}
