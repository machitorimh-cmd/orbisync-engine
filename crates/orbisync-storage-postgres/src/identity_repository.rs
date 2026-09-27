//! PostgreSQL implementation of `IdentityRepository`.

use orbisync_application::{IdentityPortError, IdentityRepository, LoginAccount};
use orbisync_domain::{
    AuthSession, AuthSessionId, AuthSessionStatus, Credential, LoginId, PasswordHash, Revision,
    Timestamp, User, UserId, UserStatus,
};
use sqlx::PgPool;
use std::collections::BTreeSet;
use time::OffsetDateTime;
use uuid::Uuid;

/// PostgreSQL-backed identity repository.
#[derive(Debug, Clone)]
pub struct PgIdentityRepository {
    pool: PgPool,
}

impl PgIdentityRepository {
    /// Creates a repository backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IdentityRepository for PgIdentityRepository {
    async fn find_account(
        &self,
        user_id: UserId,
    ) -> Result<Option<LoginAccount>, IdentityPortError> {
        let row = sqlx::query_as::<_, (Uuid, String, String, String, bool, i64, OffsetDateTime, OffsetDateTime, String, OffsetDateTime, i32, Option<OffsetDateTime>)>(
            "SELECT u.id, u.login_id, u.display_name, u.status, u.must_change_password, u.revision, u.created_at, u.updated_at, c.password_hash, c.password_changed_at, c.failed_login_count, c.locked_until FROM users u JOIN user_credentials c ON c.user_id = u.id WHERE u.id = $1"
        )
        .bind(user_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        let Some((
            uid,
            login,
            display,
            status_str,
            must_change,
            revision_raw,
            created_at,
            updated_at,
            phc,
            pwd_changed,
            failed,
            locked,
        )) = row
        else {
            return Ok(None);
        };

        let user_id = UserId::new(uid).map_err(|_| IdentityPortError::DataCorruption)?;
        let domain_login = LoginId::new(login).map_err(|_| IdentityPortError::DataCorruption)?;
        let status = match status_str.as_str() {
            "active" => UserStatus::Active,
            "disabled" => UserStatus::Disabled,
            _ => return Err(IdentityPortError::DataCorruption),
        };
        let revision = Revision::from_u64(
            u64::try_from(revision_raw).map_err(|_| IdentityPortError::DataCorruption)?,
        );
        let created = Timestamp::from_offset_date_time(created_at);
        let updated = Timestamp::from_offset_date_time(updated_at);
        let pwd_changed_ts = Timestamp::from_offset_date_time(pwd_changed);
        let locked_ts = locked.map(Timestamp::from_offset_date_time);

        let role_rows =
            sqlx::query_scalar::<_, Uuid>("SELECT role_id FROM user_roles WHERE user_id = $1")
                .bind(uid)
                .fetch_all(&self.pool)
                .await
                .map_err(|_| IdentityPortError::Unavailable)?;
        let mut roles = BTreeSet::new();
        for rid in role_rows {
            let role_id =
                orbisync_domain::RoleId::new(rid).map_err(|_| IdentityPortError::DataCorruption)?;
            roles.insert(role_id);
        }

        let user = User::reconstitute(
            user_id,
            domain_login,
            display,
            status,
            must_change,
            roles,
            created,
            updated,
            revision,
        );

        let hash = PasswordHash::new(phc).map_err(|_| IdentityPortError::DataCorruption)?;
        let credential = Credential::reconstitute(
            user_id,
            hash,
            pwd_changed_ts,
            u32::try_from(failed).map_err(|_| IdentityPortError::DataCorruption)?,
            locked_ts,
        );

        Ok(Some(LoginAccount { user, credential }))
    }

    async fn find_login(
        &self,
        login_id: &LoginId,
    ) -> Result<Option<LoginAccount>, IdentityPortError> {
        // Fetch user + credential
        let row = sqlx::query_as::<_, (Uuid, String, String, String, bool, i64, OffsetDateTime, OffsetDateTime, String, OffsetDateTime, i32, Option<OffsetDateTime>)>(
            "SELECT u.id, u.login_id, u.display_name, u.status, u.must_change_password, u.revision, u.created_at, u.updated_at, c.password_hash, c.password_changed_at, c.failed_login_count, c.locked_until FROM users u JOIN user_credentials c ON c.user_id = u.id WHERE u.login_id = $1"
        )
        .bind(login_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        let Some((
            uid,
            login,
            display,
            status_str,
            must_change,
            revision_raw,
            created_at,
            updated_at,
            phc,
            pwd_changed,
            failed,
            locked,
        )) = row
        else {
            return Ok(None);
        };

        let user_id = UserId::new(uid).map_err(|_| IdentityPortError::DataCorruption)?;
        let domain_login = LoginId::new(login).map_err(|_| IdentityPortError::DataCorruption)?;
        let status = match status_str.as_str() {
            "active" => UserStatus::Active,
            "disabled" => UserStatus::Disabled,
            _ => return Err(IdentityPortError::DataCorruption),
        };
        let revision = Revision::from_u64(
            u64::try_from(revision_raw).map_err(|_| IdentityPortError::DataCorruption)?,
        );
        let created = Timestamp::from_offset_date_time(created_at);
        let updated = Timestamp::from_offset_date_time(updated_at);
        let pwd_changed_ts = Timestamp::from_offset_date_time(pwd_changed);
        let locked_ts = locked.map(Timestamp::from_offset_date_time);

        // Roles: fetch user_roles for this user
        let role_rows =
            sqlx::query_scalar::<_, Uuid>("SELECT role_id FROM user_roles WHERE user_id = $1")
                .bind(uid)
                .fetch_all(&self.pool)
                .await
                .map_err(|_| IdentityPortError::Unavailable)?;
        let mut roles = BTreeSet::new();
        for rid in role_rows {
            let role_id =
                orbisync_domain::RoleId::new(rid).map_err(|_| IdentityPortError::DataCorruption)?;
            roles.insert(role_id);
        }

        let user = User::reconstitute(
            user_id,
            domain_login,
            display,
            status,
            must_change,
            roles,
            created,
            updated,
            revision,
        );

        let hash = PasswordHash::new(phc).map_err(|_| IdentityPortError::DataCorruption)?;
        let credential = Credential::reconstitute(
            user_id,
            hash,
            pwd_changed_ts,
            u32::try_from(failed).map_err(|_| IdentityPortError::DataCorruption)?,
            locked_ts,
        );

        Ok(Some(LoginAccount { user, credential }))
    }

    async fn save_credential(&self, credential: &Credential) -> Result<(), IdentityPortError> {
        sqlx::query(
            "UPDATE user_credentials SET password_hash = $2, password_changed_at = $3, failed_login_count = $4, locked_until = $5 WHERE user_id = $1",
        )
        .bind(credential.user_id().as_uuid())
        .bind(credential.password_hash().expose_phc())
        .bind(credential.password_changed_at().as_offset_date_time())
        .bind(i32::try_from(credential.failed_login_count()).map_err(|_| IdentityPortError::DataCorruption)?)
        .bind(credential.locked_until().map(|v| v.as_offset_date_time()))
        .execute(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;
        Ok(())
    }

    async fn record_login_failure(
        &self,
        user_id: UserId,
        now: Timestamp,
    ) -> Result<orbisync_application::LoginFailureOutcome, IdentityPortError> {
        let lock_deadline = now
            .checked_add_millis(Credential::LOGIN_LOCKOUT_MILLIS)
            .map(|t| t.as_offset_date_time())
            .map_err(|_| IdentityPortError::DataCorruption)?;
        let threshold_i32 = i32::try_from(Credential::LOGIN_FAILURE_THRESHOLD)
            .map_err(|_| IdentityPortError::DataCorruption)?;
        let row = sqlx::query_as::<_, (i32, Option<OffsetDateTime>)>(
            "UPDATE user_credentials SET failed_login_count = failed_login_count + 1, locked_until = CASE WHEN failed_login_count + 1 >= $2 THEN $3 ELSE locked_until END WHERE user_id = $1 RETURNING failed_login_count, locked_until",
        )
        .bind(user_id.as_uuid())
        .bind(threshold_i32)
        .bind(lock_deadline)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;
        let Some((failed, locked)) = row else {
            return Err(IdentityPortError::NotFound);
        };
        let failed_u32 = u32::try_from(failed).map_err(|_| IdentityPortError::DataCorruption)?;
        let locked_ts = locked.map(Timestamp::from_offset_date_time);
        Ok(orbisync_application::LoginFailureOutcome {
            failed_login_count: failed_u32,
            locked_until: locked_ts,
        })
    }

    async fn reset_login_success(
        &self,
        user_id: UserId,
        expected_failed_count: u32,
        expected_locked_until: Option<Timestamp>,
        now: Timestamp,
        new_password_hash: Option<PasswordHash>,
    ) -> Result<bool, IdentityPortError> {
        let expected_locked_odt = expected_locked_until.map(|v| v.as_offset_date_time());
        let expected_failed_i32 =
            i32::try_from(expected_failed_count).map_err(|_| IdentityPortError::DataCorruption)?;
        let rows = if let Some(hash) = new_password_hash {
            let pwd_changed = now.as_offset_date_time();
            sqlx::query(
                "UPDATE user_credentials SET failed_login_count = 0, locked_until = NULL, password_hash = $4, password_changed_at = $5 WHERE user_id = $1 AND failed_login_count = $2 AND ((locked_until = $3) OR (locked_until IS NULL AND $3 IS NULL))",
            )
            .bind(user_id.as_uuid())
            .bind(expected_failed_i32)
            .bind(expected_locked_odt)
            .bind(hash.expose_phc())
            .bind(pwd_changed)
            .execute(&self.pool)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?
        } else {
            sqlx::query(
                "UPDATE user_credentials SET failed_login_count = 0, locked_until = NULL WHERE user_id = $1 AND failed_login_count = $2 AND ((locked_until = $3) OR (locked_until IS NULL AND $3 IS NULL))",
            )
            .bind(user_id.as_uuid())
            .bind(expected_failed_i32)
            .bind(expected_locked_odt)
            .execute(&self.pool)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?
        };
        Ok(rows.rows_affected() == 1)
    }

    async fn roles_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Vec<orbisync_domain::Role>, IdentityPortError> {
        // Resolve the subject from server-owned state on every authorization.
        // Role edits/assignments must never lift a temporary subject's ceiling.
        // Missing/expired ledgers and disabled temporary subjects fail closed.
        // Keep permanent-subject role inspection/management unchanged.
        let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, i64, String, Vec<String>)>(
            "SELECT r.id, r.name, r.description, r.revision, u.kind, \
             COALESCE(array_agg(rp.permission_name) FILTER (WHERE rp.permission_name IS NOT NULL), '{}') \
             FROM users u JOIN user_roles ur ON ur.user_id = u.id \
             JOIN roles r ON r.id = ur.role_id \
             LEFT JOIN role_permissions rp ON rp.role_id = r.id \
             LEFT JOIN ephemeral_subjects e ON e.user_id = u.id \
             WHERE u.id = $1 AND \
             (u.kind IN ('account', 'external') OR \
              (u.kind IN ('guest', 'name_only') AND u.status = 'active' AND e.method = u.kind \
               AND e.expires_at > statement_timestamp())) \
             GROUP BY r.id, u.kind",
        )
        .bind(user_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        let mut roles = Vec::new();
        for (id, name, description, revision_raw, kind, perms) in rows {
            let role_id =
                orbisync_domain::RoleId::new(id).map_err(|_| IdentityPortError::DataCorruption)?;
            let permissions = perms
                .into_iter()
                .filter(|permission| {
                    !matches!(kind.as_str(), "guest" | "name_only")
                        || orbisync_application::world::EPHEMERAL_SUBJECT_PERMISSION_ALLOWLIST
                            .contains(&permission.as_str())
                })
                .map(|p| {
                    orbisync_domain::Permission::new(p)
                        .map_err(|_| IdentityPortError::DataCorruption)
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            let revision = Revision::from_u64(
                u64::try_from(revision_raw).map_err(|_| IdentityPortError::DataCorruption)?,
            );
            let role = orbisync_domain::Role::reconstitute(
                role_id,
                name,
                description,
                permissions,
                revision,
            );
            roles.push(role);
        }
        Ok(roles)
    }

    async fn find_session(
        &self,
        session_id: AuthSessionId,
    ) -> Result<Option<AuthSession>, IdentityPortError> {
        let row = sqlx::query_as::<_, (Uuid, Uuid, String, OffsetDateTime, OffsetDateTime, Option<OffsetDateTime>)>(
            "SELECT id, user_id, status, created_at, expires_at, revoked_at FROM auth_sessions WHERE id = $1",
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;

        let Some((id, uid, status_str, created, expires, revoked)) = row else {
            return Ok(None);
        };
        let sid = AuthSessionId::new(id).map_err(|_| IdentityPortError::DataCorruption)?;
        let user_id = UserId::new(uid).map_err(|_| IdentityPortError::DataCorruption)?;
        let status = match status_str.as_str() {
            "active" => AuthSessionStatus::Active,
            "revoked" => AuthSessionStatus::Revoked,
            _ => return Err(IdentityPortError::DataCorruption),
        };
        let created_ts = Timestamp::from_offset_date_time(created);
        let expires_ts = Timestamp::from_offset_date_time(expires);
        let revoked_ts = revoked.map(Timestamp::from_offset_date_time);
        let session =
            AuthSession::reconstitute(sid, user_id, status, created_ts, expires_ts, revoked_ts);
        Ok(Some(session))
    }

    async fn save_session(&self, session: &AuthSession) -> Result<(), IdentityPortError> {
        let (status_str, revoked) = match session.status() {
            AuthSessionStatus::Active => ("active", None),
            AuthSessionStatus::Revoked => ("revoked", session.revoked_at()),
        };
        sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revoked_at, revocation_reason, revision) VALUES ($1, $2, $3, $4, $5, $6, $7, 0) ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, revoked_at = EXCLUDED.revoked_at, revocation_reason = EXCLUDED.revocation_reason, revision = auth_sessions.revision + 1",
        )
        .bind(session.id().as_uuid())
        .bind(session.user_id().as_uuid())
        .bind(status_str)
        .bind(session.created_at().as_offset_date_time())
        .bind(session.expires_at().as_offset_date_time())
        .bind(revoked.map(|v| v.as_offset_date_time()))
        .bind(if status_str == "revoked" { Some("manual") } else { None::<&str> })
        .execute(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;
        Ok(())
    }
}
