//! Participation boundary and deadline for temporary subjects (ADR-026 §6).
//!
//! This backs a new authorization point. Before ADR-026 nothing in the join
//! path authorized which world a client asked for: the handler checked that
//! the instance existed, had room and could be restored, and the runtime
//! checked only that the instance id matched its own. Converging the new
//! methods onto the existing route would therefore not have bounded them.

use orbisync_application::{
    ApplicationError, EphemeralSubjectScope, ExternalIdentityStore, ScopeDecision,
};
use orbisync_domain::{Timestamp, UserId};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Reads the temporary-subject ledger.
#[derive(Debug, Clone)]
pub struct PgEphemeralSubjectScope {
    pool: PgPool,
}

impl PgEphemeralSubjectScope {
    /// Creates the adapter over `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl EphemeralSubjectScope for PgEphemeralSubjectScope {
    async fn decide(
        &self,
        user: UserId,
        world: Uuid,
        now: Timestamp,
    ) -> Result<ScopeDecision, ApplicationError> {
        let row: Option<(OffsetDateTime, Vec<Uuid>)> = sqlx::query_as(
            "SELECT expires_at, allowed_worlds FROM ephemeral_subjects WHERE user_id = $1",
        )
        .bind(user.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| ApplicationError::port_failure("participation lookup unavailable"))?;

        // No ledger row means a permanent subject, which this boundary does not
        // govern. Local accounts keep behaving exactly as before.
        let Some((expires_at, allowed_worlds)) = row else {
            return Ok(ScopeDecision::NotEphemeral);
        };

        // The deadline is enforced here, not only by the revocation job. A job
        // that is late, stopped or still inside its grace window must not
        // extend what the subject can do.
        if expires_at <= now.as_offset_date_time() {
            return Ok(ScopeDecision::Denied);
        }
        if allowed_worlds.contains(&world) {
            Ok(ScopeDecision::Allowed)
        } else {
            Ok(ScopeDecision::Denied)
        }
    }
}

/// Resolves an external `(issuer, subject)` pair to its internal user.
#[derive(Debug, Clone)]
pub struct PgExternalIdentityStore {
    pool: PgPool,
}

impl PgExternalIdentityStore {
    /// Creates the adapter over `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ExternalIdentityStore for PgExternalIdentityStore {
    async fn find_user(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<UserId>, ApplicationError> {
        // Both columns are part of the key. Matching on `subject` alone would
        // let one issuer claim another issuer's users.
        let row: Option<(Uuid,)> = sqlx::query_as(
            "SELECT user_id FROM external_identities WHERE issuer = $1 AND subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| ApplicationError::port_failure("external identity lookup unavailable"))?;
        row.map(|(id,)| {
            UserId::new(id)
                .map_err(|_| ApplicationError::port_failure("stored external identity is invalid"))
        })
        .transpose()
    }
}

/// Resolves configured role names to role ids at startup (ADR-026 §7).
///
/// Startup fails rather than continuing when a name does not resolve or a role
/// carries a permission a temporary subject may not hold. Runtime authorization
/// independently enforces that ceiling after role edits; this startup check
/// catches misconfiguration before issuing subjects with unusable permissions.
///
/// # Errors
///
/// Returns a port failure when the lookup fails, a role is missing, or a role
/// holds a permission outside the allowlist.
pub async fn resolve_ephemeral_roles(
    pool: &PgPool,
    method: &str,
    role_names: &[String],
) -> Result<Vec<orbisync_domain::RoleId>, ApplicationError> {
    let mut resolved = Vec::with_capacity(role_names.len());
    for name in role_names {
        let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM roles WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|_| ApplicationError::port_failure("role lookup unavailable"))?;
        let Some((role_id,)) = row else {
            return Err(ApplicationError::port_failure(format!(
                "auth.{method}.role_names names role `{name}`, which does not exist"
            )));
        };

        let permissions: Vec<(String,)> =
            sqlx::query_as("SELECT permission_name FROM role_permissions WHERE role_id = $1")
                .bind(role_id)
                .fetch_all(pool)
                .await
                .map_err(|_| {
                    ApplicationError::port_failure("role permission lookup unavailable")
                })?;
        let names: Vec<&str> = permissions
            .iter()
            .map(|(permission,)| permission.as_str())
            .collect();
        if let Err(rejected) =
            orbisync_application::world::ephemeral_permissions_are_allowed(names.iter().copied())
        {
            return Err(ApplicationError::port_failure(format!(
                "auth.{method}.role_names names role `{name}`, which holds `{rejected}`; \
                 a temporary subject may hold only {}",
                orbisync_application::world::EPHEMERAL_SUBJECT_PERMISSION_ALLOWLIST.join(", ")
            )));
        }
        resolved.push(
            orbisync_domain::RoleId::new(role_id)
                .map_err(|_| ApplicationError::port_failure("stored role id is invalid"))?,
        );
    }
    Ok(resolved)
}

/// How many temporary subjects one revocation pass affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RevocationOutcome {
    /// Subjects whose sessions, tickets and roles were removed.
    pub revoked: u64,
    /// Role assignments deleted.
    pub roles_removed: u64,
}

/// Revokes temporary subjects whose grace period has elapsed (ADR-026 §5).
///
/// Removes everything that lets the subject act -- active sessions, refresh
/// tokens by cascade, outstanding realtime tickets, and every role assignment
/// -- and marks the user disabled. The `users` row itself is deliberately kept.
///
/// Deleting it is not an option: `persistent_entities.owner_id` references it,
/// and `instance_checkpoints.data` carries owner ids inside JSONB with no
/// foreign key at all, which `validate_owner_references` checks on save *and*
/// on load. A row removed here would therefore make the instance that subject
/// built things in fail to restore, and nothing in SQL can tell us in advance
/// whether a checkpoint still names it.
///
/// This pass is bookkeeping, not the thing that ends access: every path
/// compares the clock against the stored deadline, so a subject is already
/// refused everywhere the moment it expires, whether or not this has run.
///
/// # Errors
///
/// Returns a port failure when the transaction cannot be completed. Nothing is
/// applied in that case; the next pass retries.
pub async fn revoke_expired_ephemeral_subjects(
    pool: &PgPool,
    now: OffsetDateTime,
    retention: time::Duration,
    batch_limit: i64,
) -> Result<RevocationOutcome, ApplicationError> {
    let cutoff = now - retention;
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ApplicationError::port_failure("revocation transaction unavailable"))?;

    // Select the subjects to act on first and lock them, so two servers running
    // this pass do not both try to revoke the same rows.
    let due: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT e.user_id FROM ephemeral_subjects e \
         JOIN users u ON u.id = e.user_id \
         WHERE e.expires_at <= $1 AND u.status = 'active' \
         ORDER BY e.expires_at \
         LIMIT $2 FOR UPDATE OF e SKIP LOCKED",
    )
    .bind(cutoff)
    .bind(batch_limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(|_| ApplicationError::port_failure("expired subject lookup failed"))?;

    if due.is_empty() {
        tx.rollback()
            .await
            .map_err(|_| ApplicationError::port_failure("revocation rollback failed"))?;
        return Ok(RevocationOutcome::default());
    }
    let ids: Vec<Uuid> = due.into_iter().map(|(id,)| id).collect();

    sqlx::query(
        "UPDATE auth_sessions SET status = 'revoked', \
         revoked_at = COALESCE(revoked_at, $2), \
         revocation_reason = 'ephemeral_subject_expired', \
         revision = revision + 1 \
         WHERE user_id = ANY($1) AND status = 'active'",
    )
    .bind(&ids)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|_| ApplicationError::port_failure("session revocation failed"))?;

    sqlx::query("DELETE FROM realtime_tickets WHERE user_id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(|_| ApplicationError::port_failure("ticket cleanup failed"))?;

    // Remove the assignments as cleanup as well as disabling the subject.
    // Authorization independently checks subject status and the deadline, so
    // a late cleanup cannot extend access or lift the permission ceiling.
    let roles_removed = sqlx::query("DELETE FROM user_roles WHERE user_id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(|_| ApplicationError::port_failure("role revocation failed"))?
        .rows_affected();

    let revoked = sqlx::query(
        "UPDATE users SET status = 'disabled', updated_at = $2, revision = revision + 1 \
         WHERE id = ANY($1) AND status = 'active'",
    )
    .bind(&ids)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|_| ApplicationError::port_failure("subject disable failed"))?
    .rows_affected();

    tx.commit()
        .await
        .map_err(|_| ApplicationError::port_failure("revocation commit failed"))?;

    Ok(RevocationOutcome {
        revoked,
        roles_removed,
    })
}
