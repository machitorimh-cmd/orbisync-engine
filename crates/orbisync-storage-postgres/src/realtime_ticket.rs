//! Realtime ticket persistence (C1).

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use orbisync_application::{
    CreateRealtimeTicketCommand, IdentityPortError, RealtimeTicketConsumption,
};
use orbisync_domain::{AuthSessionId, UserId};

/// Store for single-use realtime tickets (opaque random + HMAC digest).
#[derive(Debug, Clone)]
pub struct PgRealtimeTicketStore {
    pool: PgPool,
}

impl PgRealtimeTicketStore {
    /// Wraps `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts the digest.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` on DB failure.
    pub async fn create_ticket(
        &self,
        digest: &[u8],
        session_id: Uuid,
        user_id: Uuid,
        issued_at: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<(), crate::StorageError> {
        sqlx::query(
            "INSERT INTO realtime_tickets (token_digest, session_id, user_id, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(digest)
        .bind(session_id)
        .bind(user_id)
        .bind(issued_at)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(crate::StorageError::Database)?;
        Ok(())
    }

    /// Atomically consumes the ticket if the session is active and not expired.
    ///
    /// Single statement per C1 spec: DELETE USING auth_sessions + RETURNING.
    pub async fn consume_ticket(
        &self,
        digest: &[u8],
        now: OffsetDateTime,
    ) -> Result<Option<(Uuid, Uuid)>, crate::StorageError> {
        // DELETE ... USING ensures atomic check of session status and expiry.
        // No audit row is written here to avoid logging the digest; the raw
        // ticket is already redacted at the gateway layer.
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "DELETE FROM realtime_tickets USING auth_sessions s
             WHERE realtime_tickets.token_digest = $1
               AND realtime_tickets.session_id = s.id
               AND s.status = 'active'
               AND s.expires_at > $2
               AND realtime_tickets.expires_at > $2
             RETURNING realtime_tickets.user_id, realtime_tickets.session_id",
        )
        .bind(digest)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(crate::StorageError::Database)?;
        Ok(row)
    }

    /// Deletes expired and revoked-session tickets in a bounded batch.
    ///
    /// Uses `LIMIT ... FOR UPDATE SKIP LOCKED` to bound work per tick
    /// and to avoid contending with concurrent `consume_ticket` (C5).
    /// Deletes tickets where `expires_at <= now` or the owning session
    /// is not `active` (revoked/expired), covering the three retention
    /// cases: expired, revoked-session, and never-consumed expired rows.
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when cleanup fails.
    pub async fn delete_expired(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<u64, crate::StorageError> {
        // Bounded cleanup with SKIP LOCKED to avoid blocking consume.
        // The subquery selects the bounded set; the outer DELETE removes only that set.
        // Revoked-session tickets are included even if not yet expired to prevent
        // accumulation from session revocation without consumption.
        let result = sqlx::query(
            "DELETE FROM realtime_tickets WHERE token_digest IN ( \
                SELECT token_digest FROM realtime_tickets \
                WHERE expires_at <= $1 \
                   OR session_id IN (SELECT id FROM auth_sessions WHERE status != 'active') \
                ORDER BY expires_at LIMIT $2 FOR UPDATE SKIP LOCKED \
            )",
        )
        .bind(now)
        .bind(limit)
        .execute(&self.pool)
        .await
        .map_err(crate::StorageError::Database)?;
        Ok(result.rows_affected())
    }

    /// Returns the count of expired/revoked tickets that are eligible for cleanup.
    pub async fn count_expired(&self, now: OffsetDateTime) -> Result<i64, crate::StorageError> {
        let cnt: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM realtime_tickets WHERE expires_at <= $1 OR session_id IN (SELECT id FROM auth_sessions WHERE status != 'active')",
        )
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(crate::StorageError::Database)?;
        Ok(cnt)
    }

    /// Returns the age in seconds of the oldest expired ticket, if any.
    pub async fn oldest_expired_age_secs(
        &self,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, crate::StorageError> {
        let oldest: Option<OffsetDateTime> = sqlx::query_scalar(
            "SELECT min(expires_at) FROM realtime_tickets WHERE expires_at <= $1 OR session_id IN (SELECT id FROM auth_sessions WHERE status != 'active')",
        )
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(crate::StorageError::Database)?;
        Ok(oldest.map(|t| (now - t).whole_seconds()))
    }

    /// Drains expired tickets in multiple bounded batches until `deleted < batch` or limits hit.
    ///
    /// Bounded by `max_batches` and a 5s time budget per tick to avoid monopolising the DB.
    /// Yields between batches.
    pub async fn drain_expired(
        &self,
        now: OffsetDateTime,
        batch: i64,
        max_batches: usize,
    ) -> Result<u64, crate::StorageError> {
        let start = std::time::Instant::now();
        let budget = std::time::Duration::from_secs(20);
        let mut total: u64 = 0;
        for _ in 0..max_batches {
            if start.elapsed() >= budget {
                break;
            }
            let deleted = self.delete_expired(now, batch).await?;
            total += deleted;
            if deleted < u64::try_from(batch).unwrap_or(0) {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(total)
    }
}

#[async_trait::async_trait]
impl orbisync_application::RealtimeTicketStore for PgRealtimeTicketStore {
    async fn create(&self, command: CreateRealtimeTicketCommand) -> Result<(), IdentityPortError> {
        self.create_ticket(
            &command.token_digest,
            command.session_id.as_uuid(),
            command.user_id.as_uuid(),
            command.issued_at.as_offset_date_time(),
            command.expires_at.as_offset_date_time(),
        )
        .await
        .map_err(|_| IdentityPortError::Unavailable)
    }

    async fn consume(
        &self,
        token_digest: [u8; 32],
        now: orbisync_domain::Timestamp,
    ) -> Result<RealtimeTicketConsumption, IdentityPortError> {
        let row = self
            .consume_ticket(&token_digest, now.as_offset_date_time())
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        match row {
            Some((user_id, session_id)) => {
                let uid = UserId::new(user_id).map_err(|_| IdentityPortError::DataCorruption)?;
                let sid = AuthSessionId::new(session_id)
                    .map_err(|_| IdentityPortError::DataCorruption)?;
                Ok(RealtimeTicketConsumption::Consumed {
                    user_id: uid,
                    session_id: sid,
                })
            }
            None => Ok(RealtimeTicketConsumption::Rejected),
        }
    }
}
