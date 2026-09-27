//! Atomic login persistence for P2-C4.
//!
//! `commit_success` and `record_failure` both keep audit and credential
//! mutation inside the same local transaction. No password, token or raw login
//! identifier is persisted to audit, and the audit row is required for the
//! transaction to commit – a write failure makes the whole login fail-closed.

use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use orbisync_application::metrics::{Histogram, MetricsRecorder};
use orbisync_application::{
    IdentityPortError, LoginAuditEvent, LoginCommit, LoginTransactionStore,
};
use std::sync::Arc;
use std::time::Instant;

use crate::{StorageError, identity::AuditEvent};

/// PostgreSQL implementation of [`LoginTransactionStore`].
#[derive(Clone)]
pub struct PgLoginStore {
    pool: PgPool,
    /// Optional metrics recorder for `db_query_duration_seconds`.
    metrics: Option<Arc<dyn MetricsRecorder>>,
    failure_threshold: u32,
    lockout_duration_seconds: u64,
}

impl std::fmt::Debug for PgLoginStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLoginStore")
            .field("pool", &self.pool)
            .field("metrics", &self.metrics.is_some())
            .finish()
    }
}

impl PgLoginStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            metrics: None,
            failure_threshold: 5,
            lockout_duration_seconds: 900,
        }
    }

    /// Attaches a metrics recorder.
    #[must_use]
    pub fn with_metrics(self, metrics: Arc<dyn MetricsRecorder>) -> Self {
        Self {
            metrics: Some(metrics),
            ..self
        }
    }

    /// Configures failed-login threshold and lockout duration.
    #[must_use]
    pub fn with_login_failure_policy(mut self, threshold: u32, duration_seconds: u64) -> Self {
        self.failure_threshold = threshold.max(1);
        self.lockout_duration_seconds = duration_seconds.max(1);
        self
    }
}

fn audit_from_login(event: &LoginAuditEvent) -> AuditEvent {
    let target_type = event
        .action
        .split_once('.')
        .map(|(resource, _)| resource.to_owned());
    AuditEvent {
        id: Uuid::now_v7(),
        occurred_at: event.occurred_at.as_offset_date_time(),
        actor_user_id: event.actor_id.map(|id| id.as_uuid()),
        action: event.action.to_owned(),
        target_type,
        target_id: None,
        request_id: Some(event.request_id.to_string()),
        source_ip: event.source_ip.clone(),
        result: if event.succeeded {
            "success".to_owned()
        } else {
            "failure".to_owned()
        },
        metadata: serde_json::json!({}),
    }
}

async fn append_audit(
    tx: &mut Transaction<'_, Postgres>,
    audit: &AuditEvent,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO audit_events (id, occurred_at, actor_user_id, action, target_type, target_id, request_id, result, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
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
        .map_err(StorageError::Database)?;
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

#[async_trait::async_trait]
impl LoginTransactionStore for PgLoginStore {
    async fn commit_success(&self, commit: LoginCommit<'_>) -> Result<(), IdentityPortError> {
        let start = Instant::now();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

        // Optimistic reset of failure state. The predicate is evaluated atomically;
        // if a concurrent failure incremented the counter after `find_login`,
        // this UPDATE touches 0 rows and the concurrent failure is preserved
        // while the login still succeeds (RV-B semantics). The return value is
        // intentionally ignored – failure to reset does not fail the login.
        if let Some(new_hash) = commit.new_password_hash {
            // Update including rehash.
            let res = sqlx::query(
                "UPDATE user_credentials SET failed_login_count = 0, locked_until = NULL, password_hash = $4, password_changed_at = $5 WHERE user_id = $1 AND failed_login_count = $2 AND ((locked_until = $3) OR (locked_until IS NULL AND $3 IS NULL))",
            )
            .bind(commit.user_id.as_uuid())
            .bind(i32::try_from(commit.expected_failed_count).map_err(|_| IdentityPortError::DataCorruption)?)
            .bind(commit.expected_locked_until.map(|v| v.as_offset_date_time()))
            .bind(new_hash.expose_phc())
            .bind(commit.audit.occurred_at.as_offset_date_time())
            .execute(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
            let _ = res.rows_affected();
        } else {
            let res = sqlx::query(
                "UPDATE user_credentials SET failed_login_count = 0, locked_until = NULL WHERE user_id = $1 AND failed_login_count = $2 AND ((locked_until = $3) OR (locked_until IS NULL AND $3 IS NULL))",
            )
            .bind(commit.user_id.as_uuid())
            .bind(i32::try_from(commit.expected_failed_count).map_err(|_| IdentityPortError::DataCorruption)?)
            .bind(commit.expected_locked_until.map(|v| v.as_offset_date_time()))
            .execute(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
            let _ = res.rows_affected();
        }

        // Persist session.
        let session = commit.session;
        sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revoked_at, revocation_reason, revision) VALUES ($1, $2, 'active', $3, $4, NULL, NULL, 0)",
        )
        .bind(session.id.as_uuid())
        .bind(session.user_id.as_uuid())
        .bind(session.created_at.as_offset_date_time())
        .bind(session.expires_at.as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        // Persist refresh token (digest only).
        let refresh = commit.refresh;
        let token_id =
            Uuid::parse_str(&refresh.token_id).map_err(|_| IdentityPortError::InvalidRequest)?;
        let family_id =
            Uuid::parse_str(&refresh.family_id).map_err(|_| IdentityPortError::InvalidRequest)?;
        sqlx::query(
            "INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(token_id)
        .bind(session.id.as_uuid())
        .bind(family_id)
        .bind(refresh.digest.as_slice())
        .bind(refresh.issued_at.as_offset_date_time())
        .bind(refresh.expires_at.as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        // Mandatory audit – a failure here rolls back the whole commit so no
        // orphaned session remains (P2-C4 atomicity).
        let audit = audit_from_login(commit.audit);
        append_audit(&mut tx, &audit)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

        tx.commit()
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        if let Some(metrics) = &self.metrics {
            metrics.observe(Histogram::DbQueryDuration, start.elapsed().as_secs_f64());
        }
        Ok(())
    }

    async fn commit_subject_success(
        &self,
        commit: orbisync_application::SubjectCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        let start = Instant::now();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

        // The user row is created only for a subject seen for the first time.
        // No `user_credentials` row is written for any of these methods, which
        // is what keeps them off the password login path: `find_login` INNER
        // JOINs that table, so a subject without one cannot be looked up there.
        if let Some(new_user) = commit.new_user {
            sqlx::query(
                "INSERT INTO users (id, login_id, display_name, status, must_change_password, kind, revision, created_at, updated_at) \
                 VALUES ($1, $2, $3, 'active', FALSE, $4, 1, $5, $5)",
            )
            .bind(commit.user_id.as_uuid())
            .bind(new_user.login_id.as_str())
            .bind(&new_user.display_name)
            .bind(new_user.kind.as_str())
            .bind(new_user.created_at.as_offset_date_time())
            .execute(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

            // Roles come from configuration resolved at startup. Nothing in the
            // request reaches this list.
            for role_id in commit.grant_roles {
                sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
                    .bind(commit.user_id.as_uuid())
                    .bind(role_id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(|_| IdentityPortError::Unavailable)?;
            }
        }

        // Absolute deadline and world boundary for a temporary subject. Written
        // once here and never updated, so refresh rotation can only read it.
        if let Some(ephemeral) = commit.ephemeral {
            sqlx::query(
                "INSERT INTO ephemeral_subjects (user_id, method, created_at, expires_at, allowed_worlds) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(commit.user_id.as_uuid())
            .bind(ephemeral.method.as_str())
            .bind(ephemeral.created_at.as_offset_date_time())
            .bind(ephemeral.expires_at.as_offset_date_time())
            .bind(&ephemeral.allowed_worlds)
            .execute(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        }

        // INSERT waits for a competing transaction on the unique pair. If it
        // wins, resolve its committed mapping before allowing any session to
        // persist. Roll back ALL provisional user/role writes on a mismatch;
        // the caller must issue fresh token material for the winning user.
        if let Some(external) = commit.external {
            sqlx::query(
                "INSERT INTO external_identities (user_id, issuer, subject, created_at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (issuer, subject) DO NOTHING",
            )
            .bind(commit.user_id.as_uuid())
            .bind(&external.issuer)
            .bind(&external.subject)
            .bind(external.created_at.as_offset_date_time())
            .execute(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
            let mapped_user: Uuid = sqlx::query_scalar(
                "SELECT user_id FROM external_identities WHERE issuer = $1 AND subject = $2",
            )
            .bind(&external.issuer)
            .bind(&external.subject)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
            if mapped_user != commit.user_id.as_uuid() {
                tx.rollback()
                    .await
                    .map_err(|_| IdentityPortError::Unavailable)?;
                return Err(IdentityPortError::Conflict);
            }
        }

        let session = commit.session;
        sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revoked_at, revocation_reason, revision) VALUES ($1, $2, 'active', $3, $4, NULL, NULL, 0)",
        )
        .bind(session.id.as_uuid())
        .bind(session.user_id.as_uuid())
        .bind(session.created_at.as_offset_date_time())
        .bind(session.expires_at.as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        let refresh = commit.refresh;
        let token_id =
            Uuid::parse_str(&refresh.token_id).map_err(|_| IdentityPortError::InvalidRequest)?;
        let family_id =
            Uuid::parse_str(&refresh.family_id).map_err(|_| IdentityPortError::InvalidRequest)?;
        sqlx::query(
            "INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(token_id)
        .bind(session.id.as_uuid())
        .bind(family_id)
        .bind(refresh.digest.as_slice())
        .bind(refresh.issued_at.as_offset_date_time())
        .bind(refresh.expires_at.as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        // Same fail-closed rule as the credential path: losing the audit rolls
        // back the subject rather than leaving an unaudited session.
        let audit = audit_from_login(commit.audit);
        append_audit(&mut tx, &audit)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

        tx.commit()
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        if let Some(metrics) = &self.metrics {
            metrics.observe(Histogram::DbQueryDuration, start.elapsed().as_secs_f64());
        }
        Ok(())
    }

    async fn record_failure(
        &self,
        user_id: Option<orbisync_domain::UserId>,
        audit: LoginAuditEvent,
    ) -> Result<(), IdentityPortError> {
        let start = Instant::now();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

        if let Some(uid) = user_id {
            // Increment failure counter atomically. The threshold / duration are
            // domain constants (DM-05) and are hard-coded here for consistency
            // with PgIdentityRepository.
            let lock_deadline = audit
                .occurred_at
                .checked_add_millis(
                    i64::try_from(self.lockout_duration_seconds.saturating_mul(1_000))
                        .map_err(|_| IdentityPortError::DataCorruption)?,
                )
                .map(|v| v.as_offset_date_time())
                .map_err(|_| IdentityPortError::DataCorruption)?;
            let threshold = i32::try_from(self.failure_threshold)
                .map_err(|_| IdentityPortError::DataCorruption)?;
            // Use same atomic pattern as record_login_failure.
            let row = sqlx::query_as::<_, (i32, Option<OffsetDateTime>)>(
                "UPDATE user_credentials SET failed_login_count = failed_login_count + 1, locked_until = CASE WHEN failed_login_count + 1 >= $2 THEN $3 ELSE locked_until END WHERE user_id = $1 RETURNING failed_login_count, locked_until",
            )
            .bind(uid.as_uuid())
            .bind(threshold)
            .bind(lock_deadline)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
            // Unknown user cannot happen here because caller already verified
            // existence, but treat missing row as unavailable to fail closed.
            if row.is_none() {
                tx.rollback()
                    .await
                    .map_err(|_| IdentityPortError::Unavailable)?;
                return Err(IdentityPortError::Unavailable);
            }
        }

        // Failure audit – must succeed for the transaction to commit, otherwise
        // the increment above is rolled back and the login fails closed.
        let audit_row = audit_from_login(&audit);
        append_audit(&mut tx, &audit_row)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;

        tx.commit()
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        if let Some(metrics) = &self.metrics {
            metrics.observe(Histogram::DbQueryDuration, start.elapsed().as_secs_f64());
        }
        Ok(())
    }
}
