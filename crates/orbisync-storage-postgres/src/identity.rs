//! Identity persistence operations with narrow, module-local transactions.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use orbisync_application::{
    ExtensionEvent, IdentityAdministrationStore as IdentityAdministrationStorePort,
    IdentityAuditEvent, IdentityMutation, IdentityPortError, RefreshRotationResult,
    RefreshTokenRotationStore, RotateRefreshTokenCommand,
};
use orbisync_domain::{UserId, UserStatus};

use crate::StorageError;

type RefreshTokenRow = (
    Uuid,
    Uuid,
    Uuid,
    Option<OffsetDateTime>,
    OffsetDateTime,
    String,
    Uuid,
    // ADR-026: the temporary subject deadline, when the session belongs to one.
    Option<OffsetDateTime>,
);

/// Data required to create a user and credential row.
#[derive(Debug, Clone)]
pub struct NewUser {
    /// User identifier.
    pub id: Uuid,
    /// Unique login identifier.
    pub login_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Argon2id encoded password hash, never a plaintext password.
    pub password_hash: String,
    /// Creation and password-change timestamp.
    pub occurred_at: OffsetDateTime,
}

/// An append-only audit event written with an identity mutation.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Audit identifier.
    pub id: Uuid,
    /// Event timestamp.
    pub occurred_at: OffsetDateTime,
    /// Acting user, when the action has a user actor.
    pub actor_user_id: Option<Uuid>,
    /// Stable machine-readable action.
    pub action: String,
    /// Target resource kind.
    pub target_type: Option<String>,
    /// Target resource identifier.
    pub target_id: Option<String>,
    /// Server-generated request identifier.
    pub request_id: Option<String>,
    /// Source IP textual representation.
    pub source_ip: Option<String>,
    /// Stable result value.
    pub result: String,
    /// Non-secret structured metadata.
    pub metadata: Value,
}

/// A replacement refresh token row supplied after cryptographic token creation.
#[derive(Debug, Clone)]
pub struct NewRefreshToken {
    /// Token row identifier.
    pub id: Uuid,
    /// Keyed token digest; raw tokens must never be passed to this adapter.
    pub token_digest: Vec<u8>,
    /// Issue timestamp.
    pub issued_at: OffsetDateTime,
    /// Expiry timestamp.
    pub expires_at: OffsetDateTime,
}

/// Result of atomically consuming a refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRotation {
    /// The token was consumed and replaced.
    Rotated {
        /// Session whose token was rotated.
        session_id: Uuid,
        /// User owning the session.
        user_id: Uuid,
        /// Absolute temporary-subject deadline read during rotation.
        absolute_deadline: Option<OffsetDateTime>,
    },
    /// A consumed token was reused and its session was revoked.
    ReuseDetected,
    /// The digest was unknown, expired, or belonged to a revoked session.
    Rejected,
}

/// PostgreSQL implementation of identity administration persistence.
#[derive(Debug, Clone)]
pub struct IdentityAdministrationStore {
    pool: PgPool,
}

impl IdentityAdministrationStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates a user and credential and appends its audit event atomically.
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when any statement or commit fails.
    pub async fn create_user_with_audit(
        &self,
        user: &NewUser,
        audit: &AuditEvent,
    ) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::Database)?;
        sqlx::query(
            "INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, 'active', true, 1, $4, $4)",
        )
        .bind(user.id)
        .bind(&user.login_id)
        .bind(&user.display_name)
        .bind(user.occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        sqlx::query(
            "INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, $2, $3)",
        )
        .bind(user.id)
        .bind(&user.password_hash)
        .bind(user.occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        append_audit(&mut tx, audit).await?;
        append_extension_outbox(
            &mut tx,
            &ExtensionEvent::UserCreated {
                user_id: UserId::new(user.id)
                    .map_err(|_| StorageError::InvalidExtensionRegistration)?,
            },
        )
        .await?;
        tx.commit().await.map_err(StorageError::Database)
    }

    /// Disables a user, revokes all active sessions, and audits the mutation.
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error and rolls back all changes on failure.
    pub async fn disable_user_with_audit(
        &self,
        user_id: Uuid,
        expected_revision: i64,
        occurred_at: OffsetDateTime,
        audit: &AuditEvent,
    ) -> Result<bool, StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::Database)?;
        let updated = sqlx::query(
            "UPDATE users SET status = 'disabled', revision = revision + 1, updated_at = $3 WHERE id = $1 AND revision = $2",
        )
        .bind(user_id)
        .bind(expected_revision)
        .bind(occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        if updated.rows_affected() == 0 {
            tx.rollback().await.map_err(StorageError::Database)?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_disabled', revision = revision + 1 WHERE user_id = $1 AND status = 'active'",
        )
        .bind(user_id)
        .bind(occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        append_audit(&mut tx, audit).await?;
        append_extension_outbox(
            &mut tx,
            &ExtensionEvent::UserDisabled {
                user_id: UserId::new(user_id)
                    .map_err(|_| StorageError::InvalidExtensionRegistration)?,
            },
        )
        .await?;
        tx.commit().await.map_err(StorageError::Database)?;
        Ok(true)
    }

    /// Atomically increments the login failure count and conditionally sets
    /// `locked_until` when the DM-05 threshold is reached.
    ///
    /// Unified with `PgIdentityRepository::record_login_failure` so the two
    /// adapters do not duplicate divergent logic (RV-B deduplication).
    /// The threshold / duration are the domain constants `5 / 15 min`
    /// (`domain-model.md` DM-05); hard-coded consistently rather than via
    /// config (no `auth.login_*` key exists).
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error when the update fails.
    pub async fn record_login_failure(
        &self,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, StorageError> {
        let lock_deadline = now + Duration::minutes(15);
        sqlx::query_scalar(
            "UPDATE user_credentials SET failed_login_count = failed_login_count + 1, locked_until = CASE WHEN failed_login_count + 1 >= 5 THEN $2 ELSE locked_until END WHERE user_id = $1 RETURNING failed_login_count",
        )
        .bind(user_id)
        .bind(lock_deadline)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::Database)
    }

    /// Atomically clears failure state when the row still matches the
    /// expected values observed at `find_login` time (optimistic predicate).
    /// Returns `true` when the row was updated, `false` on concurrent
    /// modification. Unified with `PgIdentityRepository::reset_login_success`
    /// to avoid duplication.
    pub async fn reset_login_success(
        &self,
        user_id: Uuid,
        expected_failed_count: i32,
        expected_locked_until: Option<OffsetDateTime>,
    ) -> Result<bool, StorageError> {
        let rows = sqlx::query(
            "UPDATE user_credentials SET failed_login_count = 0, locked_until = NULL WHERE user_id = $1 AND failed_login_count = $2 AND ((locked_until = $3) OR (locked_until IS NULL AND $3 IS NULL))",
        )
        .bind(user_id)
        .bind(expected_failed_count)
        .bind(expected_locked_until)
        .execute(&self.pool)
        .await
        .map_err(StorageError::Database)?;
        Ok(rows.rows_affected() == 1)
    }

    /// Consumes and rotates a refresh token under row locks, detecting reuse.
    ///
    /// The caller passes only a keyed digest. Cryptographic comparison happens
    /// before this boundary; PostgreSQL uses the digest solely as an indexed key.
    /// Audit events are typed per outcome: `token.refreshed` for a normal rotation,
    /// `token.reuse_detected` for a consumed-token replay, and `token.rejected`
    /// for unknown/expired/inactive tokens (CR-20 / ADR-008). No raw token material
    /// or digest is ever written to the audit row (W-27 §0.1).
    /// Rejected audits are persisted via a separate transaction so that the
    /// rollback of the main row-lock transaction does not erase the audit.
    ///
    /// # Errors
    ///
    /// Returns a generic persistence error and rolls back on statement failure.
    pub async fn rotate_refresh_token(
        &self,
        token_digest: &[u8],
        replacement: &NewRefreshToken,
        now: OffsetDateTime,
        request_id: &str,
    ) -> Result<RefreshRotation, StorageError> {
        self.rotate_refresh_token_with_source_ip(token_digest, replacement, now, request_id, None)
            .await
    }

    /// Rotates a refresh token and carries its trusted-proxy-filtered source IP
    /// into the audit event.
    pub async fn rotate_refresh_token_with_source_ip(
        &self,
        token_digest: &[u8],
        replacement: &NewRefreshToken,
        now: OffsetDateTime,
        request_id: &str,
        source_ip: Option<String>,
    ) -> Result<RefreshRotation, StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::Database)?;
        // (c) fix: fetch user_id from auth_sessions so audit can record the correct actor.
        // Previously actor_user_id was None for both token events, causing actor_type to be
        // derived as "system" even though the operation is on a user's session (security-relevant).
        let row: Option<RefreshTokenRow> = sqlx::query_as(
            "SELECT r.id, r.session_id, r.family_id, r.consumed_at, r.expires_at, s.status, s.user_id, e.expires_at \n             FROM refresh_tokens r \n             JOIN auth_sessions s ON s.id = r.session_id \n             LEFT JOIN ephemeral_subjects e ON e.user_id = s.user_id \n             WHERE r.token_digest = $1 FOR UPDATE OF r, s",
        )
        .bind(token_digest)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        let Some((
            id,
            session_id,
            family_id,
            consumed_at,
            expires_at,
            session_status,
            user_id,
            ephemeral_expires_at,
        )) = row
        else {
            // CR-20 / ADR-008: unknown digest must be audited as token.rejected/failure.
            // The main transaction is rolled back (its row locks are discarded), but the
            // audit must survive via a separate transaction so rollback does not erase it.
            // Raw token/digest must not be written to audit (W-27 §0.1).
            tx.rollback().await.map_err(StorageError::Database)?;
            let mut audit_tx = self.pool.begin().await.map_err(StorageError::Database)?;
            let audit = AuditEvent {
                id: Uuid::now_v7(),
                occurred_at: now,
                actor_user_id: None,
                action: "token.rejected".to_owned(),
                target_type: Some("refresh_token".to_owned()),
                target_id: None,
                request_id: Some(request_id.to_owned()),
                source_ip: source_ip.clone(),
                result: "failure".to_owned(),
                metadata: serde_json::json!({
                    "reason": "unknown_token"
                }),
            };
            append_audit(&mut audit_tx, &audit).await?;
            audit_tx.commit().await.map_err(StorageError::Database)?;
            return Ok(RefreshRotation::Rejected);
        };
        if consumed_at.is_some() {
            sqlx::query("UPDATE refresh_tokens SET reuse_detected_at = COALESCE(reuse_detected_at, $2) WHERE id = $1")
                .bind(id)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::Database)?;
            sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = COALESCE(revoked_at, $2), revocation_reason = 'refresh_token_reuse', revision = revision + 1 WHERE id = $1 AND status = 'active'")
                .bind(session_id)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::Database)?;
            let audit = AuditEvent {
                id: Uuid::now_v7(),
                occurred_at: now,
                actor_user_id: Some(user_id),
                action: "token.reuse_detected".to_owned(),
                target_type: Some("auth_session".to_owned()),
                target_id: Some(session_id.to_string()),
                request_id: Some(request_id.to_owned()),
                source_ip: source_ip.clone(),
                result: "failure".to_owned(),
                metadata: serde_json::json!({
                    "session_id": session_id.to_string(),
                    "family_id": family_id.to_string(),
                    "reason": "refresh_token_reuse"
                }),
            };
            append_audit(&mut tx, &audit).await?;
            tx.commit().await.map_err(StorageError::Database)?;
            return Ok(RefreshRotation::ReuseDetected);
        }
        // ADR-026: a temporary subject past its deadline cannot refresh, whatever
        // the session row says. Rotation advances `auth_sessions.expires_at` to
        // each new token's expiry, so without this the subject would renew
        // itself indefinitely; the session expiry alone cannot bound it, and
        // this query is the only place that reads the deadline on this path.
        // The check compares the clock against the stored deadline, so it holds
        // whether or not the revocation job has run.
        let past_subject_deadline = ephemeral_expires_at.is_some_and(|deadline| deadline <= now);
        if expires_at <= now || session_status != "active" || past_subject_deadline {
            // CR-20 / ADR-008: expired / inactive session must be audited as token.rejected/failure.
            // Persist audit via separate transaction after rolling back the main tx.
            // Raw token/digest must not be written to audit (W-27 §0.1).
            tx.rollback().await.map_err(StorageError::Database)?;
            let mut audit_tx = self.pool.begin().await.map_err(StorageError::Database)?;
            let reason = if past_subject_deadline {
                "ephemeral_subject_expired"
            } else if expires_at <= now {
                "expired"
            } else {
                "inactive_session"
            };
            let audit = AuditEvent {
                id: Uuid::now_v7(),
                occurred_at: now,
                actor_user_id: Some(user_id),
                action: "token.rejected".to_owned(),
                target_type: Some("auth_session".to_owned()),
                target_id: Some(session_id.to_string()),
                request_id: Some(request_id.to_owned()),
                source_ip: source_ip.clone(),
                result: "failure".to_owned(),
                metadata: serde_json::json!({
                    "session_id": session_id.to_string(),
                    "family_id": family_id.to_string(),
                    "reason": reason
                }),
            };
            append_audit(&mut audit_tx, &audit).await?;
            audit_tx.commit().await.map_err(StorageError::Database)?;
            return Ok(RefreshRotation::Rejected);
        }
        // ADR-026: clamp the replacement to the subject's deadline. Without this
        // the new token would carry the global refresh lifetime and outlive the
        // subject it belongs to.
        let replacement_expires_at = match ephemeral_expires_at {
            Some(deadline) if deadline < replacement.expires_at => deadline,
            _ => replacement.expires_at,
        };
        sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(replacement.id)
            .bind(session_id)
            .bind(family_id)
            .bind(&replacement.token_digest)
            .bind(replacement.issued_at)
            .bind(replacement_expires_at)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::Database)?;
        sqlx::query("UPDATE refresh_tokens SET consumed_at = $2, replaced_by = $3 WHERE id = $1")
            .bind(id)
            .bind(now)
            .bind(replacement.id)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::Database)?;
        // Extend session expiry to the new refresh token's expiry so the
        // session lifetime follows the configured refresh TTL (CR-H).
        //
        // ADR-026: for a temporary subject the extension is capped by the
        // subject's own deadline, computed above. LEAST is applied in SQL as
        // well so that a concurrent rotation cannot push the session past the
        // deadline between this read and this write. `ephemeral_subjects` is
        // never written here: the deadline is fixed when the subject is
        // issued, and rotation may only read it.
        sqlx::query(
            "UPDATE auth_sessions SET expires_at = \
             CASE WHEN $3::timestamptz IS NULL THEN $2 ELSE LEAST($2, $3) END \
             WHERE id = $1",
        )
        .bind(session_id)
        .bind(replacement_expires_at)
        .bind(ephemeral_expires_at)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        let audit = AuditEvent {
            id: Uuid::now_v7(),
            occurred_at: now,
            actor_user_id: Some(user_id),
            action: "token.refreshed".to_owned(),
            target_type: Some("auth_session".to_owned()),
            target_id: Some(session_id.to_string()),
            request_id: Some(request_id.to_owned()),
            source_ip,
            result: "success".to_owned(),
            metadata: serde_json::json!({
                "session_id": session_id.to_string(),
                "family_id": family_id.to_string()
            }),
        };
        append_audit(&mut tx, &audit).await?;
        tx.commit().await.map_err(StorageError::Database)?;
        Ok(RefreshRotation::Rotated {
            session_id,
            user_id,
            absolute_deadline: ephemeral_expires_at,
        })
    }
}

#[async_trait::async_trait]
impl RefreshTokenRotationStore for IdentityAdministrationStore {
    async fn rotate(
        &self,
        command: RotateRefreshTokenCommand,
    ) -> Result<RefreshRotationResult, IdentityPortError> {
        self.rotate_with_source_ip(command, None).await
    }

    async fn rotate_with_source_ip(
        &self,
        command: RotateRefreshTokenCommand,
        source_ip: Option<String>,
    ) -> Result<RefreshRotationResult, IdentityPortError> {
        let token_id = Uuid::parse_str(&command.replacement.token_id)
            .map_err(|_| IdentityPortError::Unavailable)?;
        let replacement = NewRefreshToken {
            id: token_id,
            token_digest: command.replacement.digest.to_vec(),
            issued_at: command.replacement.issued_at.as_offset_date_time(),
            expires_at: command.replacement.expires_at.as_offset_date_time(),
        };
        let request_id = command.request_id.to_string();
        match self
            .rotate_refresh_token_with_source_ip(
                &command.presented_digest,
                &replacement,
                command.now.as_offset_date_time(),
                &request_id,
                source_ip,
            )
            .await
            .map_err(|_| IdentityPortError::Unavailable)?
        {
            RefreshRotation::Rotated {
                session_id,
                user_id,
                absolute_deadline,
            } => {
                let sid = orbisync_domain::AuthSessionId::new(session_id)
                    .map_err(|_| IdentityPortError::DataCorruption)?;
                let uid = UserId::new(user_id).map_err(|_| IdentityPortError::DataCorruption)?;
                Ok(RefreshRotationResult::Rotated {
                    user_id: uid,
                    session_id: sid,
                    absolute_deadline: absolute_deadline
                        .map(orbisync_domain::Timestamp::from_offset_date_time),
                })
            }
            RefreshRotation::ReuseDetected => Ok(RefreshRotationResult::ReuseDetected),
            RefreshRotation::Rejected => Ok(RefreshRotationResult::Rejected),
        }
    }
}

#[async_trait::async_trait]
impl orbisync_application::RefreshTokenCreationStore for IdentityAdministrationStore {
    async fn create(
        &self,
        command: orbisync_application::CreateRefreshTokenCommand,
    ) -> Result<(), IdentityPortError> {
        let token_id =
            Uuid::parse_str(&command.token_id).map_err(|_| IdentityPortError::InvalidRequest)?;
        let family_id =
            Uuid::parse_str(&command.family_id).map_err(|_| IdentityPortError::InvalidRequest)?;
        sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(token_id)
            .bind(command.session_id.as_uuid())
            .bind(family_id)
            .bind(command.digest.as_slice())
            .bind(command.issued_at.as_offset_date_time())
            .bind(command.expires_at.as_offset_date_time())
            .execute(&self.pool)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl IdentityAdministrationStorePort for IdentityAdministrationStore {
    async fn apply(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
    ) -> Result<(), IdentityPortError> {
        self.apply_port_mutation(mutation, audit, None, None)
            .await
            .map_err(|err| match err {
                StorageError::RoleNotFound => IdentityPortError::NotFound,
                StorageError::RevisionMismatch => IdentityPortError::Conflict,
                StorageError::InvalidRevision => IdentityPortError::Conflict,
                _ => IdentityPortError::Unavailable,
            })
    }

    async fn apply_with_source_ip(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        source_ip: Option<String>,
    ) -> Result<(), IdentityPortError> {
        self.apply_port_mutation(mutation, audit, None, source_ip)
            .await
            .map_err(|err| match err {
                StorageError::RoleNotFound => IdentityPortError::NotFound,
                StorageError::RevisionMismatch | StorageError::InvalidRevision => {
                    IdentityPortError::Conflict
                }
                _ => IdentityPortError::Unavailable,
            })
    }

    async fn apply_with_event(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        event: ExtensionEvent,
    ) -> Result<(), IdentityPortError> {
        self.apply_port_mutation(mutation, audit, Some(event), None)
            .await
            .map_err(|err| match err {
                StorageError::RoleNotFound => IdentityPortError::NotFound,
                StorageError::RevisionMismatch | StorageError::InvalidRevision => {
                    IdentityPortError::Conflict
                }
                _ => IdentityPortError::Unavailable,
            })
    }

    async fn apply_with_event_and_source_ip(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        event: ExtensionEvent,
        source_ip: Option<String>,
    ) -> Result<(), IdentityPortError> {
        self.apply_port_mutation(mutation, audit, Some(event), source_ip)
            .await
            .map_err(|err| match err {
                StorageError::RoleNotFound => IdentityPortError::NotFound,
                StorageError::RevisionMismatch | StorageError::InvalidRevision => {
                    IdentityPortError::Conflict
                }
                _ => IdentityPortError::Unavailable,
            })
    }

    async fn apply_with_idempotency(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        idempotency_key: String,
        idempotency_owner: String,
        completion: orbisync_application::IdempotencyCompletion,
    ) -> Result<(), IdentityPortError> {
        self.apply_port_mutation_with_idempotency(
            mutation,
            audit,
            idempotency_key,
            idempotency_owner,
            completion,
            None,
        )
        .await
        .map_err(|err| match err {
            StorageError::RoleNotFound => IdentityPortError::NotFound,
            StorageError::RevisionMismatch => IdentityPortError::Conflict,
            StorageError::InvalidRevision => IdentityPortError::Conflict,
            _ => IdentityPortError::Unavailable,
        })
    }

    async fn apply_with_idempotency_and_source_ip(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        idempotency_key: String,
        idempotency_owner: String,
        completion: orbisync_application::IdempotencyCompletion,
        source_ip: Option<String>,
    ) -> Result<(), IdentityPortError> {
        self.apply_port_mutation_with_idempotency(
            mutation,
            audit,
            idempotency_key,
            idempotency_owner,
            completion,
            source_ip,
        )
        .await
        .map_err(|err| match err {
            StorageError::RoleNotFound => IdentityPortError::NotFound,
            StorageError::RevisionMismatch | StorageError::InvalidRevision => {
                IdentityPortError::Conflict
            }
            _ => IdentityPortError::Unavailable,
        })
    }
}

impl IdentityAdministrationStore {
    async fn apply_port_mutation(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        extension_event: Option<ExtensionEvent>,
        source_ip: Option<String>,
    ) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::Database)?;
        match mutation {
            IdentityMutation::BootstrapAdministrator {
                user,
                credential,
                roles,
            } => {
                let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if existing != 0 {
                    return Err(StorageError::InvalidRevision);
                }
                let user_revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, 'active', true, $4, $5, $5)")
                    .bind(user.id().as_uuid())
                    .bind(user.login_id().as_str())
                    .bind(user.display_name())
                    .bind(user_revision)
                    .bind(user.created_at().as_offset_date_time())
                    .execute(&mut *tx).await.map_err(StorageError::Database)?;
                sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, $2, $3)")
                    .bind(user.id().as_uuid()).bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time()).execute(&mut *tx).await.map_err(StorageError::Database)?;
                for role in roles {
                    let role_revision = i64::try_from(role.revision().as_u64())
                        .map_err(|_| StorageError::InvalidRevision)?;
                    sqlx::query(
                        "INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, $3, $4)",
                    )
                    .bind(role.id().as_uuid())
                    .bind(role.name())
                    .bind(role.description())
                    .bind(role_revision)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    for permission in role.permissions() {
                        sqlx::query(
                            "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                        )
                        .bind(permission.as_str())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                        sqlx::query(
                            "INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)",
                        )
                        .bind(role.id().as_uuid())
                        .bind(permission.as_str())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                    }
                    sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
                        .bind(user.id().as_uuid())
                        .bind(role.id().as_uuid())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::CreateUser { user, credential } => {
                let status = match user.status() {
                    UserStatus::Active => "active",
                    UserStatus::Disabled => "disabled",
                };
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                    .bind(user.id().as_uuid())
                    .bind(user.login_id().as_str())
                    .bind(user.display_name())
                    .bind(status)
                    .bind(user.must_change_password())
                    .bind(revision)
                    .bind(user.created_at().as_offset_date_time())
                    .bind(user.updated_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at, failed_login_count, locked_until) VALUES ($1, $2, $3, $4, $5)")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .bind(i32::try_from(credential.failed_login_count()).map_err(|_| StorageError::InvalidLoginFailureCount)?)
                    .bind(credential.locked_until().map(orbisync_domain::Timestamp::as_offset_date_time))
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::ReplaceUserRoles { user_id, role_ids } => {
                sqlx::query("DELETE FROM user_roles WHERE user_id = $1")
                    .bind(user_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                for role_id in role_ids {
                    sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
                        .bind(user_id.as_uuid())
                        .bind(role_id.as_uuid())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::StoreRole { role } => {
                let revision = i64::try_from(role.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, revision = EXCLUDED.revision")
                    .bind(role.id().as_uuid())
                    .bind(role.name())
                    .bind(role.description())
                    .bind(revision)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                    .bind(role.id().as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                for permission in role.permissions() {
                    let name = permission.to_string();
                    sqlx::query(
                        "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                    )
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    sqlx::query(
                        "INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)",
                    )
                    .bind(role.id().as_uuid())
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::UpdateRole { role } => {
                // Same optimistic-lock shape as DeleteRole: SELECT ... FOR UPDATE
                // distinguishes "no such role" (404) from "revision mismatch" (409)
                // before the write, rather than inferring it from rows_affected==0
                // on a blind UPDATE.
                let revision = i64::try_from(role.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let expected = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT revision FROM roles WHERE id = $1 FOR UPDATE")
                        .bind(role.id().as_uuid())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                let Some((current_revision,)) = row else {
                    return Err(StorageError::RoleNotFound);
                };
                if current_revision != expected {
                    return Err(StorageError::RevisionMismatch);
                }
                let updated = sqlx::query(
                    "UPDATE roles SET name = $2, description = $3, revision = $4 WHERE id = $1 AND revision = $5",
                )
                .bind(role.id().as_uuid())
                .bind(role.name())
                .bind(role.description())
                .bind(revision)
                .bind(expected)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::RevisionMismatch);
                }
                sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                    .bind(role.id().as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                for permission in role.permissions() {
                    let name = permission.to_string();
                    sqlx::query(
                        "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                    )
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    sqlx::query(
                        "INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)",
                    )
                    .bind(role.id().as_uuid())
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::StoreUserStatus { user } => {
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let previous = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let status = match user.status() {
                    UserStatus::Active => "active",
                    UserStatus::Disabled => "disabled",
                };
                let updated = sqlx::query("UPDATE users SET status = $2, revision = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
                    .bind(user.id().as_uuid())
                    .bind(status)
                    .bind(revision)
                    .bind(user.updated_at().as_offset_date_time())
                    .bind(previous)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::InvalidRevision);
                }
                if user.status() == UserStatus::Disabled {
                    sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_disabled', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                        .bind(user.id().as_uuid())
                        .bind(user.updated_at().as_offset_date_time())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::UpdateUserProfile {
                user,
                expected_revision,
            } => {
                let expected =
                    i64::try_from(expected_revision).map_err(|_| StorageError::InvalidRevision)?;
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let status = match user.status() {
                    UserStatus::Active => "active",
                    UserStatus::Disabled => "disabled",
                };
                let updated = sqlx::query("UPDATE users SET display_name = $2, status = $3, revision = $4, updated_at = $5 WHERE id = $1 AND revision = $6")
                    .bind(user.id().as_uuid())
                    .bind(user.display_name())
                    .bind(status)
                    .bind(revision)
                    .bind(user.updated_at().as_offset_date_time())
                    .bind(expected)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::RevisionMismatch);
                }
                if user.status() == UserStatus::Disabled {
                    sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_disabled', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                        .bind(user.id().as_uuid())
                        .bind(user.updated_at().as_offset_date_time())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::StoreCredential { credential } => {
                sqlx::query("UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = 0, locked_until = NULL WHERE user_id = $1")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::PasswordChangeRejected { .. } => {
                // No state change – audit row is still written in same transaction
                // (failure path for wrong current_password).
            }
            IdentityMutation::StorePasswordChange { user, credential } => {
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let previous = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let updated =
                    sqlx::query("UPDATE users SET must_change_password = $2, revision = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
                        .bind(user.id().as_uuid())
                        .bind(user.must_change_password())
                        .bind(revision)
                        .bind(user.updated_at().as_offset_date_time())
                        .bind(previous)
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::InvalidRevision);
                }
                sqlx::query("UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = 0, locked_until = NULL WHERE user_id = $1")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                // Revoke all active sessions for this user atomically per §5.4 / design 84.
                // Alternative would be to reject refresh by comparing password_changed_at
                // vs session creation; revocation is chosen so that existing access tokens
                // and refresh tokens immediately lose validity and auth_sessions state
                // is explicit. Done in same transaction as password change (ADR-017).
                sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'password_changed', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                    .bind(user.id().as_uuid())
                    .bind(user.updated_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::ResetPassword { user, credential } => {
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let previous = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let updated =
                    sqlx::query("UPDATE users SET must_change_password = $2, revision = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
                        .bind(user.id().as_uuid())
                        .bind(user.must_change_password())
                        .bind(revision)
                        .bind(user.updated_at().as_offset_date_time())
                        .bind(previous)
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::InvalidRevision);
                }
                sqlx::query("UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = 0, locked_until = NULL WHERE user_id = $1")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                // PW-1 §2: 既存セッションの扱い – パスワードリセット時に全ての auth_sessions と
                // refresh_tokens を失効させる。攻撃者が奪取したセッションがリセット後も生き残ると
                // 追い出せないため、password_reset と同じトランザクションで revoke する (ADR-002)。
                // refresh_tokens は auth_sessions の revoke により間接的に無効化される
                // (rotate 時 session_status != 'active' で Rejected) ため、明示的な delete は不要だが
                // セッション失効により両方が失効する。
                sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'password_reset', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                    .bind(user.id().as_uuid())
                    .bind(user.updated_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::StoreSession { session } => {
                let (status, revoked_at) = match session.status() {
                    orbisync_domain::AuthSessionStatus::Active => ("active", None),
                    orbisync_domain::AuthSessionStatus::Revoked => (
                        "revoked",
                        session
                            .revoked_at()
                            .map(|value| value.as_offset_date_time()),
                    ),
                };
                sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revoked_at, revocation_reason, revision) VALUES ($1, $2, $3, $4, $5, $6, $7, 0) ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, revoked_at = EXCLUDED.revoked_at, revocation_reason = EXCLUDED.revocation_reason, revision = auth_sessions.revision + 1")
                    .bind(session.id().as_uuid())
                    .bind(session.user_id().as_uuid())
                    .bind(status)
                    .bind(session.created_at().as_offset_date_time())
                    .bind(session.expires_at().as_offset_date_time())
                    .bind(revoked_at)
                    .bind(if status == "revoked" { Some("logout") } else { None })
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::DeleteRole {
                role_id,
                expected_revision,
            } => {
                // Optimistic lock: fetch current revision and handle NotFound / Conflict.
                // Assigned-role decision: cascade delete assignments (see IdentityMutation::DeleteRole doc).
                let expected =
                    i64::try_from(expected_revision).map_err(|_| StorageError::InvalidRevision)?;
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT revision FROM roles WHERE id = $1 FOR UPDATE")
                        .bind(role_id.as_uuid())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                let Some((current_revision,)) = row else {
                    return Err(StorageError::RoleNotFound);
                };
                if current_revision != expected {
                    return Err(StorageError::RevisionMismatch);
                }
                // Cascade: remove assignments first to satisfy FK without cascade.
                sqlx::query("DELETE FROM user_roles WHERE role_id = $1")
                    .bind(role_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                    .bind(role_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                let deleted = sqlx::query("DELETE FROM roles WHERE id = $1 AND revision = $2")
                    .bind(role_id.as_uuid())
                    .bind(expected)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if deleted.rows_affected() != 1 {
                    return Err(StorageError::RevisionMismatch);
                }
            }
            IdentityMutation::RevokeUserSessions {
                user_id,
                occurred_at,
            } => {
                sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_sessions_revoked', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                    .bind(user_id.as_uuid())
                    .bind(occurred_at.as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
        }
        let event = audit_from_port(audit, source_ip);
        append_audit(&mut tx, &event).await?;
        if let Some(extension_event) = extension_event {
            append_extension_outbox(&mut tx, &extension_event).await?;
        }
        tx.commit().await.map_err(StorageError::Database)
    }

    async fn apply_port_mutation_with_idempotency(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        idempotency_key: String,
        idempotency_owner: String,
        completion: orbisync_application::IdempotencyCompletion,
        source_ip: Option<String>,
    ) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::Database)?;
        // Reuse the same mutation handling as apply_port_mutation but
        // keep the transaction open to also complete idempotency.
        match mutation {
            IdentityMutation::BootstrapAdministrator {
                user,
                credential,
                roles,
            } => {
                let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if existing != 0 {
                    return Err(StorageError::InvalidRevision);
                }
                let user_revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, 'active', true, $4, $5, $5)")
                    .bind(user.id().as_uuid())
                    .bind(user.login_id().as_str())
                    .bind(user.display_name())
                    .bind(user_revision)
                    .bind(user.created_at().as_offset_date_time())
                    .execute(&mut *tx).await.map_err(StorageError::Database)?;
                sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, $2, $3)")
                    .bind(user.id().as_uuid()).bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time()).execute(&mut *tx).await.map_err(StorageError::Database)?;
                for role in roles {
                    let role_revision = i64::try_from(role.revision().as_u64())
                        .map_err(|_| StorageError::InvalidRevision)?;
                    sqlx::query(
                        "INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, $3, $4)",
                    )
                    .bind(role.id().as_uuid())
                    .bind(role.name())
                    .bind(role.description())
                    .bind(role_revision)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    for permission in role.permissions() {
                        sqlx::query(
                            "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                        )
                        .bind(permission.as_str())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                        sqlx::query(
                            "INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)",
                        )
                        .bind(role.id().as_uuid())
                        .bind(permission.as_str())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                    }
                    sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
                        .bind(user.id().as_uuid())
                        .bind(role.id().as_uuid())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::CreateUser { user, credential } => {
                let status = match user.status() {
                    UserStatus::Active => "active",
                    UserStatus::Disabled => "disabled",
                };
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                    .bind(user.id().as_uuid())
                    .bind(user.login_id().as_str())
                    .bind(user.display_name())
                    .bind(status)
                    .bind(user.must_change_password())
                    .bind(revision)
                    .bind(user.created_at().as_offset_date_time())
                    .bind(user.updated_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at, failed_login_count, locked_until) VALUES ($1, $2, $3, $4, $5)")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .bind(i32::try_from(credential.failed_login_count()).map_err(|_| StorageError::InvalidLoginFailureCount)?)
                    .bind(credential.locked_until().map(orbisync_domain::Timestamp::as_offset_date_time))
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::ReplaceUserRoles { user_id, role_ids } => {
                sqlx::query("DELETE FROM user_roles WHERE user_id = $1")
                    .bind(user_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                for role_id in role_ids {
                    sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
                        .bind(user_id.as_uuid())
                        .bind(role_id.as_uuid())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::StoreRole { role } => {
                let revision = i64::try_from(role.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, revision = EXCLUDED.revision")
                    .bind(role.id().as_uuid())
                    .bind(role.name())
                    .bind(role.description())
                    .bind(revision)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                    .bind(role.id().as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                for permission in role.permissions() {
                    let name = permission.to_string();
                    sqlx::query(
                        "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                    )
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    sqlx::query(
                        "INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)",
                    )
                    .bind(role.id().as_uuid())
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::UpdateRole { role } => {
                // Same optimistic-lock shape as DeleteRole: SELECT ... FOR UPDATE
                // distinguishes "no such role" (404) from "revision mismatch" (409)
                // before the write, rather than inferring it from rows_affected==0
                // on a blind UPDATE.
                let revision = i64::try_from(role.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let expected = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT revision FROM roles WHERE id = $1 FOR UPDATE")
                        .bind(role.id().as_uuid())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                let Some((current_revision,)) = row else {
                    return Err(StorageError::RoleNotFound);
                };
                if current_revision != expected {
                    return Err(StorageError::RevisionMismatch);
                }
                let updated = sqlx::query(
                    "UPDATE roles SET name = $2, description = $3, revision = $4 WHERE id = $1 AND revision = $5",
                )
                .bind(role.id().as_uuid())
                .bind(role.name())
                .bind(role.description())
                .bind(revision)
                .bind(expected)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::RevisionMismatch);
                }
                sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                    .bind(role.id().as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                for permission in role.permissions() {
                    let name = permission.to_string();
                    sqlx::query(
                        "INSERT INTO permissions (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                    )
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                    sqlx::query(
                        "INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)",
                    )
                    .bind(role.id().as_uuid())
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::StoreUserStatus { user } => {
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let previous = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let status = match user.status() {
                    UserStatus::Active => "active",
                    UserStatus::Disabled => "disabled",
                };
                let updated = sqlx::query("UPDATE users SET status = $2, revision = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
                    .bind(user.id().as_uuid())
                    .bind(status)
                    .bind(revision)
                    .bind(user.updated_at().as_offset_date_time())
                    .bind(previous)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::InvalidRevision);
                }
                if user.status() == UserStatus::Disabled {
                    sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_disabled', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                        .bind(user.id().as_uuid())
                        .bind(user.updated_at().as_offset_date_time())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::UpdateUserProfile {
                user,
                expected_revision,
            } => {
                let expected =
                    i64::try_from(expected_revision).map_err(|_| StorageError::InvalidRevision)?;
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let status = match user.status() {
                    UserStatus::Active => "active",
                    UserStatus::Disabled => "disabled",
                };
                let updated = sqlx::query("UPDATE users SET display_name = $2, status = $3, revision = $4, updated_at = $5 WHERE id = $1 AND revision = $6")
                    .bind(user.id().as_uuid())
                    .bind(user.display_name())
                    .bind(status)
                    .bind(revision)
                    .bind(user.updated_at().as_offset_date_time())
                    .bind(expected)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::RevisionMismatch);
                }
                if user.status() == UserStatus::Disabled {
                    sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_disabled', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                        .bind(user.id().as_uuid())
                        .bind(user.updated_at().as_offset_date_time())
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                }
            }
            IdentityMutation::StoreCredential { credential } => {
                sqlx::query("UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = 0, locked_until = NULL WHERE user_id = $1")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::PasswordChangeRejected { .. } => {
                // No state change – audit row is still written in same transaction
            }
            IdentityMutation::StorePasswordChange { user, credential } => {
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let previous = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let updated =
                    sqlx::query("UPDATE users SET must_change_password = $2, revision = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
                        .bind(user.id().as_uuid())
                        .bind(user.must_change_password())
                        .bind(revision)
                        .bind(user.updated_at().as_offset_date_time())
                        .bind(previous)
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::InvalidRevision);
                }
                sqlx::query("UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = 0, locked_until = NULL WHERE user_id = $1")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'password_changed', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                    .bind(user.id().as_uuid())
                    .bind(user.updated_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::ResetPassword { user, credential } => {
                let revision = i64::try_from(user.revision().as_u64())
                    .map_err(|_| StorageError::InvalidRevision)?;
                let previous = revision
                    .checked_sub(1)
                    .ok_or(StorageError::InvalidRevision)?;
                let updated =
                    sqlx::query("UPDATE users SET must_change_password = $2, revision = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
                        .bind(user.id().as_uuid())
                        .bind(user.must_change_password())
                        .bind(revision)
                        .bind(user.updated_at().as_offset_date_time())
                        .bind(previous)
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                if updated.rows_affected() != 1 {
                    return Err(StorageError::InvalidRevision);
                }
                sqlx::query("UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = 0, locked_until = NULL WHERE user_id = $1")
                    .bind(credential.user_id().as_uuid())
                    .bind(credential.password_hash().expose_phc())
                    .bind(credential.password_changed_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'password_reset', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                    .bind(user.id().as_uuid())
                    .bind(user.updated_at().as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::StoreSession { session } => {
                let (status, revoked_at) = match session.status() {
                    orbisync_domain::AuthSessionStatus::Active => ("active", None),
                    orbisync_domain::AuthSessionStatus::Revoked => (
                        "revoked",
                        session
                            .revoked_at()
                            .map(|value| value.as_offset_date_time()),
                    ),
                };
                sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revoked_at, revocation_reason, revision) VALUES ($1, $2, $3, $4, $5, $6, $7, 0) ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, revoked_at = EXCLUDED.revoked_at, revocation_reason = EXCLUDED.revocation_reason, revision = auth_sessions.revision + 1")
                    .bind(session.id().as_uuid())
                    .bind(session.user_id().as_uuid())
                    .bind(status)
                    .bind(session.created_at().as_offset_date_time())
                    .bind(session.expires_at().as_offset_date_time())
                    .bind(revoked_at)
                    .bind(if status == "revoked" { Some("logout") } else { None })
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
            IdentityMutation::DeleteRole {
                role_id,
                expected_revision,
            } => {
                let expected =
                    i64::try_from(expected_revision).map_err(|_| StorageError::InvalidRevision)?;
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT revision FROM roles WHERE id = $1 FOR UPDATE")
                        .bind(role_id.as_uuid())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(StorageError::Database)?;
                let Some((current_revision,)) = row else {
                    return Err(StorageError::RoleNotFound);
                };
                if current_revision != expected {
                    return Err(StorageError::RevisionMismatch);
                }
                sqlx::query("DELETE FROM user_roles WHERE role_id = $1")
                    .bind(role_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
                    .bind(role_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                let deleted = sqlx::query("DELETE FROM roles WHERE id = $1 AND revision = $2")
                    .bind(role_id.as_uuid())
                    .bind(expected)
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
                if deleted.rows_affected() != 1 {
                    return Err(StorageError::RevisionMismatch);
                }
            }
            IdentityMutation::RevokeUserSessions {
                user_id,
                occurred_at,
            } => {
                sqlx::query("UPDATE auth_sessions SET status = 'revoked', revoked_at = $2, revocation_reason = 'user_sessions_revoked', revision = revision + 1 WHERE user_id = $1 AND status = 'active'")
                    .bind(user_id.as_uuid())
                    .bind(occurred_at.as_offset_date_time())
                    .execute(&mut *tx)
                    .await
                    .map_err(StorageError::Database)?;
            }
        }
        let event = audit_from_port(audit, source_ip);
        append_audit(&mut tx, &event).await?;
        // Complete idempotency in same transaction – if it affects 0 rows,
        // the whole transaction must rollback so password/session/audit are
        // not left committed without a replayable response (AUD-C2).
        let key_uuid =
            Uuid::parse_str(&idempotency_key).map_err(|_| StorageError::InvalidRevision)?;
        let owner_uuid =
            Uuid::parse_str(&idempotency_owner).map_err(|_| StorageError::InvalidRevision)?;
        let status_code =
            i16::try_from(completion.status_code).map_err(|_| StorageError::InvalidRevision)?;
        let result = sqlx::query(
            "UPDATE idempotency_records SET state = 'completed', status_code = $2, response_content_type = $3, response_body = $4 WHERE key = $1 AND owner_token = $5 AND state = 'in_progress'",
        )
        .bind(key_uuid)
        .bind(status_code)
        .bind(&completion.response_content_type)
        .bind(completion.response.into_bytes())
        .bind(owner_uuid)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::Database)?;
        if result.rows_affected() != 1 {
            // Explicit rollback so the mutation + audit are undone.
            tx.rollback().await.map_err(StorageError::Database)?;
            return Err(StorageError::Database(sqlx::Error::RowNotFound));
        }
        tx.commit().await.map_err(StorageError::Database)
    }
}

fn audit_from_port(audit: IdentityAuditEvent, source_ip: Option<String>) -> AuditEvent {
    let target_type = audit
        .action
        .split_once('.')
        .map(|(resource, _)| resource.to_owned());
    AuditEvent {
        id: Uuid::now_v7(),
        occurred_at: audit.occurred_at.as_offset_date_time(),
        actor_user_id: audit.actor_id.map(|id| id.as_uuid()),
        action: audit.action.to_owned(),
        target_type,
        target_id: audit.resource_id,
        request_id: Some(audit.request_id.to_string()),
        source_ip,
        result: if audit.succeeded {
            "success"
        } else {
            "failure"
        }
        .to_owned(),
        metadata: serde_json::json!({}),
    }
}

async fn append_audit(
    tx: &mut Transaction<'_, Postgres>,
    audit: &AuditEvent,
) -> Result<(), StorageError> {
    if let Err(e) = sqlx::query("INSERT INTO audit_events (id, occurred_at, actor_user_id, action, target_type, target_id, request_id, result, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
        .bind(audit.id)
        .bind(audit.occurred_at)
        .bind(audit.actor_user_id)
        .bind(&audit.action)
        .bind(&audit.target_type)
        .bind(&audit.target_id)
        .bind(&audit.request_id)
        .bind(&audit.result)
        .bind(&audit.metadata)
        .execute(&mut **tx)
        .await
    {
        tracing::warn!(error = %e, action = %audit.action, result = %audit.result, "failed to append audit event");
        return Err(StorageError::Database(e));
    }
    if let Some(source_ip) = &audit.source_ip {
        sqlx::query(
            "INSERT INTO audit_source_ips (audit_event_id, source_ip, created_at) VALUES ($1, $2::inet, $3)",
        )
        .bind(audit.id)
        .bind(source_ip)
        .bind(audit.occurred_at)
        .execute(&mut **tx)
        .await
        .map_err(StorageError::Database)?;
    }
    Ok(())
}

async fn append_extension_outbox(
    tx: &mut Transaction<'_, Postgres>,
    event: &ExtensionEvent,
) -> Result<(), StorageError> {
    let event_id = Uuid::now_v7();
    let created_at = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO outbox_events (id, event_id, owner_module, event_type, event_kind, payload, created_at, available_at) VALUES ($1, $1, 'extensions', $2, $2, $3, $4, $4)",
    )
    .bind(event_id)
    .bind(event.kind())
    .bind(event.payload())
    .bind(created_at)
    .execute(&mut **tx)
    .await
    .map_err(StorageError::Database)?;
    Ok(())
}
