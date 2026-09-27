//! PostgreSQL-backed REST idempotency records with lease and owner token (P2-C2).

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use orbisync_application::{
    EncryptedResponse, IdempotencyClaim as PortClaim, IdempotencyClaimCommand,
    IdempotencyCompletion as PortCompletion, IdempotencyStore as IdempotencyStorePort,
    IdentityPortError,
};
use orbisync_domain::Timestamp;

use crate::{IdempotencyRecordRow, StorageError};

/// Result of claiming an idempotency key.
#[derive(Debug, Clone)]
pub enum IdempotencyClaim {
    /// This request owns a newly inserted in-progress record.
    Acquired {
        /// Owner token.
        owner: Uuid,
        /// Lease expiry.
        lease_until: OffsetDateTime,
    },
    /// The same request already owns or completed this key.
    Existing(IdempotencyRecordRow),
    /// The key exists for a different operation, actor, or request hash.
    Reused,
    /// Same request is in progress with remaining lease.
    InProgress {
        /// Seconds until lease expiry.
        retry_after_secs: u64,
    },
}

/// Encrypted response data used to complete an idempotency record.
#[derive(Debug, Clone)]
pub struct IdempotencyCompletion {
    /// HTTP status code.
    pub status_code: i16,
    /// Response media type.
    pub response_content_type: String,
    /// Application-level envelope-encrypted response body.
    pub encrypted_response_body: Vec<u8>,
}

/// PostgreSQL idempotency repository with fixed 24-hour retention and short lease.
#[derive(Debug, Clone)]
pub struct IdempotencyStore {
    pool: PgPool,
}

impl IdempotencyStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Lease duration for in-progress records (P2-C2).
    const LEASE_SECS: u64 = 30;

    /// Claims a key or returns the request already associated with it.
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when the transaction fails.
    pub async fn claim(
        &self,
        key: Uuid,
        actor_user_id: Option<Uuid>,
        operation: &str,
        request_hash: &[u8],
        now: OffsetDateTime,
    ) -> Result<IdempotencyClaim, StorageError> {
        let owner = Uuid::now_v7();
        let lease_until = now + Duration::seconds(i64::try_from(Self::LEASE_SECS).unwrap_or(30));
        let expires_at = now + Duration::hours(24);
        let mut tx = self.pool.begin().await.map_err(StorageError::Database)?;
        // Cleanup expired by expires_at
        sqlx::query("DELETE FROM idempotency_records WHERE key = $1 AND expires_at <= $2")
            .bind(key)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::Database)?;
        let inserted = sqlx::query(
            "INSERT INTO idempotency_records (key, actor_user_id, operation, request_hash, state, created_at, expires_at, owner_token, lease_until) VALUES ($1, $2, $3, $4, 'in_progress', $5, $6, $7, $8) ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(actor_user_id)
        .bind(operation)
        .bind(request_hash)
        .bind(now)
        .bind(expires_at)
        .bind(owner)
        .bind(lease_until)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        if inserted.rows_affected() == 1 {
            tx.commit().await.map_err(StorageError::Database)?;
            return Ok(IdempotencyClaim::Acquired { owner, lease_until });
        }
        // Conflict – fetch existing row
        let record: IdempotencyRecordRow = sqlx::query_as(
            "SELECT key, actor_user_id, operation, request_hash, state, status_code, response_content_type, response_body, created_at, expires_at, owner_token, lease_until FROM idempotency_records WHERE key = $1 FOR UPDATE",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        let matches = record.actor_user_id == actor_user_id
            && record.operation == operation
            && record.request_hash == request_hash;
        // Completed case
        if record.state == "completed" {
            tx.commit().await.map_err(StorageError::Database)?;
            return Ok(if matches {
                IdempotencyClaim::Existing(record)
            } else {
                IdempotencyClaim::Reused
            });
        }
        // In-progress case
        if record.state == "in_progress" {
            let lease_expired = record.lease_until.is_some_and(|v| v <= now);
            if lease_expired {
                if matches {
                    // Steal expired lease: update to new owner/lease
                    let new_owner = Uuid::now_v7();
                    let new_lease =
                        now + Duration::seconds(i64::try_from(Self::LEASE_SECS).unwrap_or(30));
                    let new_expires = now + Duration::hours(24);
                    sqlx::query(
                        "UPDATE idempotency_records SET actor_user_id = $2, operation = $3, request_hash = $4, owner_token = $5, lease_until = $6, created_at = $7, expires_at = $8 WHERE key = $1",
                    )
                    .bind(key)
                    .bind(actor_user_id)
                    .bind(operation)
                    .bind(request_hash)
                    .bind(new_owner)
                    .bind(new_lease)
                    .bind(now)
                    .bind(new_expires)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    tx.commit().await.map_err(StorageError::Database)?;
                    return Ok(IdempotencyClaim::Acquired {
                        owner: new_owner,
                        lease_until: new_lease,
                    });
                }
                tx.commit().await.map_err(StorageError::Database)?;
                return Ok(IdempotencyClaim::Reused);
            }
            // Lease still valid
            if matches {
                let retry_after = record
                    .lease_until
                    .map(|v| {
                        let diff = v - now;
                        let secs = diff.whole_seconds();
                        if secs < 0 {
                            0
                        } else {
                            u64::try_from(secs).unwrap_or(Self::LEASE_SECS)
                        }
                    })
                    .unwrap_or(Self::LEASE_SECS);
                let retry_after = if retry_after == 0 { 1 } else { retry_after };
                tx.commit().await.map_err(StorageError::Database)?;
                return Ok(IdempotencyClaim::InProgress {
                    retry_after_secs: retry_after,
                });
            }
            tx.commit().await.map_err(StorageError::Database)?;
            return Ok(IdempotencyClaim::Reused);
        }
        tx.commit().await.map_err(StorageError::Database)?;
        Ok(IdempotencyClaim::Reused)
    }

    /// Stores the encrypted response for a previously claimed key (owner-checked).
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when the update fails.
    pub async fn complete(
        &self,
        key: Uuid,
        owner: Uuid,
        completion: &IdempotencyCompletion,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            "UPDATE idempotency_records SET state = 'completed', status_code = $2, response_content_type = $3, response_body = $4 WHERE key = $1 AND owner_token = $5 AND state = 'in_progress'",
        )
        .bind(key)
        .bind(completion.status_code)
        .bind(&completion.response_content_type)
        .bind(&completion.encrypted_response_body)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::Database)?;
        Ok(result.rows_affected() == 1)
    }

    /// Abandons an owned in-progress record (owner-checked, P2-C2).
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when the delete fails.
    pub async fn abandon(&self, key: Uuid, owner: Uuid) -> Result<bool, StorageError> {
        let result = sqlx::query(
            "DELETE FROM idempotency_records WHERE key = $1 AND owner_token = $2 AND state = 'in_progress'",
        )
        .bind(key)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::Database)?;
        Ok(result.rows_affected() == 1)
    }

    /// Deletes expired records in a bounded batch and returns the count.
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when cleanup fails.
    pub async fn delete_expired(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<u64, StorageError> {
        let result = sqlx::query("DELETE FROM idempotency_records WHERE key IN (SELECT key FROM idempotency_records WHERE expires_at <= $1 ORDER BY expires_at LIMIT $2 FOR UPDATE SKIP LOCKED)")
            .bind(now)
            .bind(limit)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Database)?;
        Ok(result.rows_affected())
    }

    /// Returns the count of expired idempotency records.
    pub async fn count_expired(&self, now: OffsetDateTime) -> Result<i64, StorageError> {
        let cnt: i64 =
            sqlx::query_scalar("SELECT count(*) FROM idempotency_records WHERE expires_at <= $1")
                .bind(now)
                .fetch_one(&self.pool)
                .await
                .map_err(StorageError::Database)?;
        Ok(cnt)
    }

    /// Returns the age in seconds of the oldest expired idempotency record.
    pub async fn oldest_expired_age_secs(
        &self,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, StorageError> {
        let oldest: Option<OffsetDateTime> = sqlx::query_scalar(
            "SELECT min(expires_at) FROM idempotency_records WHERE expires_at <= $1",
        )
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::Database)?;
        Ok(oldest.map(|t| (now - t).whole_seconds()))
    }

    /// Drains expired idempotency records in multiple batches with budget and yield.
    pub async fn drain_expired(
        &self,
        now: OffsetDateTime,
        batch: i64,
        max_batches: usize,
    ) -> Result<u64, StorageError> {
        let start = std::time::Instant::now();
        let budget = std::time::Duration::from_secs(20);
        let mut total = 0;
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
impl IdempotencyStorePort for IdempotencyStore {
    async fn claim(
        &self,
        command: IdempotencyClaimCommand,
    ) -> Result<PortClaim, IdentityPortError> {
        let key = Uuid::parse_str(&command.key).map_err(|_| IdentityPortError::InvalidRequest)?;
        let existing = self
            .claim(
                key,
                command.actor_user_id.map(|value| value.as_uuid()),
                &command.operation,
                &command.request_hash,
                command.now.as_offset_date_time(),
            )
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        match existing {
            IdempotencyClaim::Acquired { owner, lease_until } => {
                let ts = Timestamp::from_unix_millis(
                    lease_until.unix_timestamp() * 1000 + i64::from(lease_until.millisecond()),
                )
                .map_err(|_| IdentityPortError::DataCorruption)?;
                Ok(PortClaim::Acquired {
                    owner: owner.to_string(),
                    lease_until: ts,
                })
            }
            IdempotencyClaim::InProgress { retry_after_secs } => {
                Ok(PortClaim::InProgress { retry_after_secs })
            }
            IdempotencyClaim::Reused => Ok(PortClaim::Reused),
            IdempotencyClaim::Existing(record) => {
                let status_code = record
                    .status_code
                    .and_then(|value| u16::try_from(value).ok())
                    .ok_or(IdentityPortError::DataCorruption)?;
                let response_content_type = record
                    .response_content_type
                    .ok_or(IdentityPortError::DataCorruption)?;
                let response = record
                    .response_body
                    .ok_or(IdentityPortError::DataCorruption)?;
                Ok(PortClaim::Completed(PortCompletion {
                    status_code,
                    response_content_type,
                    response: EncryptedResponse::new(response),
                }))
            }
        }
    }

    async fn complete(
        &self,
        key: String,
        owner: String,
        completion: PortCompletion,
    ) -> Result<(), IdentityPortError> {
        let key = Uuid::parse_str(&key).map_err(|_| IdentityPortError::InvalidRequest)?;
        let owner_uuid = Uuid::parse_str(&owner).map_err(|_| IdentityPortError::InvalidRequest)?;
        let status_code =
            i16::try_from(completion.status_code).map_err(|_| IdentityPortError::DataCorruption)?;
        let completed = self
            .complete(
                key,
                owner_uuid,
                &IdempotencyCompletion {
                    status_code,
                    response_content_type: completion.response_content_type,
                    encrypted_response_body: completion.response.into_bytes(),
                },
            )
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        if completed {
            Ok(())
        } else {
            Err(IdentityPortError::Unavailable)
        }
    }

    async fn abandon(&self, key: String, owner: String) -> Result<(), IdentityPortError> {
        let key = Uuid::parse_str(&key).map_err(|_| IdentityPortError::InvalidRequest)?;
        let owner_uuid = Uuid::parse_str(&owner).map_err(|_| IdentityPortError::InvalidRequest)?;
        let deleted = self
            .abandon(key, owner_uuid)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        if deleted {
            Ok(())
        } else {
            Err(IdentityPortError::Unavailable)
        }
    }
}
