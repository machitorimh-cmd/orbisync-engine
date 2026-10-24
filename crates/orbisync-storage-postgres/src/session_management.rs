//! Owner-scoped session queries and atomic revocation/audit transactions.

use orbisync_application::{
    IdentityPortError, Page, PageRequest, QueryCursor, SessionAccess, SessionManagementPort,
    SessionSelection, SessionView, pagination::CursorCodec, validate_device_label,
};
use orbisync_domain::{AuthSessionId, AuthSessionStatus, Timestamp};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

/// PostgreSQL session-management adapter. Never reads raw or digested tokens.
#[derive(Debug, Clone)]
pub struct PgSessionManagementStore {
    pool: PgPool,
    codec: CursorCodec,
}

impl PgSessionManagementStore {
    /// Creates the adapter with the independently configured pagination key.
    #[must_use]
    pub fn new(pool: PgPool, codec: CursorCodec) -> Self {
        Self { pool, codec }
    }

    fn decode_cursor(
        &self,
        access: &SessionAccess,
        page: &PageRequest,
    ) -> Result<Option<Uuid>, IdentityPortError> {
        let Some(cursor) = &page.after else {
            return Ok(None);
        };
        let decoded = self
            .codec
            .decode_users(&cursor.0)
            .map_err(|_| IdentityPortError::InvalidRequest)?;
        let prefix = format!("sessions:{}:", access.user_id);
        let value = decoded
            .strip_prefix(&prefix)
            .ok_or(IdentityPortError::InvalidRequest)?;
        let id = AuthSessionId::parse(value).map_err(|_| IdentityPortError::InvalidRequest)?;
        Ok(Some(id.as_uuid()))
    }

    async fn lock_owner(
        tx: &mut Transaction<'_, Postgres>,
        access: &SessionAccess,
    ) -> Result<(), IdentityPortError> {
        // Use the same users -> sessions order as login/reset/disable. Logins
        // committed before this lock are included; a later new login remains
        // possible because revoking sessions does not disable the account.
        let found: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
                .bind(access.user_id.as_uuid())
                .fetch_optional(&mut **tx)
                .await
                .map_err(unavailable)?;
        found.ok_or(IdentityPortError::NotFound)?;
        Ok(())
    }
}

type SessionRow = (
    Uuid,
    OffsetDateTime,
    OffsetDateTime,
    String,
    Option<OffsetDateTime>,
    Option<String>,
);

#[async_trait::async_trait]
impl SessionManagementPort for PgSessionManagementStore {
    async fn list(
        &self,
        access: &SessionAccess,
        page: &PageRequest,
    ) -> Result<Page<SessionView>, IdentityPortError> {
        access.validate()?;
        if !(1..=200).contains(&page.limit) {
            return Err(IdentityPortError::InvalidRequest);
        }
        let after = self.decode_cursor(access, page)?;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
            .bind(access.user_id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
        if !exists {
            return Err(IdentityPortError::NotFound);
        }
        let rows = sqlx::query_as::<_, SessionRow>("SELECT id,created_at,expires_at,status,revoked_at,device_label FROM auth_sessions WHERE user_id=$1 AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT $3")
            .bind(access.user_id.as_uuid()).bind(after).bind(i64::from(page.limit) + 1)
            .fetch_all(&mut *tx).await.map_err(unavailable)?;
        let has_more = rows.len() > page.limit as usize;
        let items = rows
            .into_iter()
            .take(page.limit as usize)
            .map(|(id, created, expires, status, revoked, label)| {
                Ok(SessionView {
                    id: AuthSessionId::new(id).map_err(|_| IdentityPortError::DataCorruption)?,
                    created_at: Timestamp::from_offset_date_time(created),
                    expires_at: Timestamp::from_offset_date_time(expires),
                    status: match status.as_str() {
                        "active" => AuthSessionStatus::Active,
                        "revoked" => AuthSessionStatus::Revoked,
                        _ => return Err(IdentityPortError::DataCorruption),
                    },
                    revoked_at: revoked.map(Timestamp::from_offset_date_time),
                    device_label: label,
                })
            })
            .collect::<Result<Vec<_>, IdentityPortError>>()?;
        let next = if has_more {
            let last = items.last().ok_or(IdentityPortError::DataCorruption)?;
            Some(QueryCursor(
                self.codec
                    .encode_users(&format!("sessions:{}:{}", access.user_id, last.id))
                    .map_err(|_| IdentityPortError::DataCorruption)?,
            ))
        } else {
            None
        };
        audit(
            &mut tx,
            access,
            "session.listed",
            None,
            serde_json::json!({"count": items.len()}),
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Page { items, next })
    }

    async fn revoke(
        &self,
        access: &SessionAccess,
        selection: SessionSelection,
    ) -> Result<u64, IdentityPortError> {
        access.validate()?;
        if matches!(selection, SessionSelection::All) && !access.administrator {
            return Err(IdentityPortError::InvalidRequest);
        }
        let (only, except) = match selection {
            SessionSelection::One(id) => (Some(id.as_uuid()), None),
            SessionSelection::OtherThan(id) => (None, Some(id.as_uuid())),
            SessionSelection::All => (None, None),
        };
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        Self::lock_owner(&mut tx, access).await?;
        // An already-revoked owned individual session is an idempotent success;
        // absent and differently-owned identifiers share exactly the same error.
        if let Some(id) = only {
            let found: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM auth_sessions WHERE user_id=$1 AND id=$2 FOR UPDATE",
            )
            .bind(access.user_id.as_uuid())
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
            if found.is_none() {
                return Err(IdentityPortError::NotFound);
            }
        }
        // Lock active rows in primary-key order without materializing the entire
        // user's session history in application memory for bulk revocation.
        let changed = sqlx::query("WITH selected AS (SELECT id FROM auth_sessions WHERE user_id=$1 AND status='active' AND ($2::uuid IS NULL OR id=$2) AND ($3::uuid IS NULL OR id<>$3) ORDER BY id FOR UPDATE) UPDATE auth_sessions s SET status='revoked',revoked_at=$4,revocation_reason='session_management',revision=s.revision+1 FROM selected WHERE s.id=selected.id")
            .bind(access.user_id.as_uuid()).bind(only).bind(except).bind(access.now.as_offset_date_time()).execute(&mut *tx).await.map_err(unavailable)?.rows_affected();
        let action = match selection {
            SessionSelection::One(_) => "session.revoked",
            SessionSelection::OtherThan(_) => "session.others_revoked",
            SessionSelection::All => "session.all_revoked",
        };
        audit(
            &mut tx,
            access,
            action,
            only,
            serde_json::json!({"revoked_count":changed}),
        )
        .await?;
        // Session status is the authoritative family revocation flag: access,
        // refresh, realtime tickets and existing WS all check the same row.
        tx.commit().await.map_err(unavailable)?;
        Ok(changed)
    }

    async fn set_label(
        &self,
        access: &SessionAccess,
        session: AuthSessionId,
        label: Option<String>,
    ) -> Result<(), IdentityPortError> {
        access.validate()?;
        if access.actor_id != access.user_id {
            return Err(IdentityPortError::NotFound);
        }
        validate_device_label(label.as_deref())?;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        Self::lock_owner(&mut tx, access).await?;
        let changed = sqlx::query("UPDATE auth_sessions SET device_label=$3,revision=revision+1 WHERE user_id=$1 AND id=$2")
            .bind(access.user_id.as_uuid()).bind(session.as_uuid()).bind(label)
            .execute(&mut *tx).await.map_err(unavailable)?.rows_affected();
        if changed == 0 {
            return Err(IdentityPortError::NotFound);
        }
        audit(
            &mut tx,
            access,
            "session.label_updated",
            Some(session.as_uuid()),
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }
}

fn unavailable(_: sqlx::Error) -> IdentityPortError {
    IdentityPortError::Unavailable
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    access: &SessionAccess,
    action: &str,
    session: Option<Uuid>,
    metadata: serde_json::Value,
) -> Result<(), IdentityPortError> {
    let audit_id = Uuid::now_v7();
    sqlx::query("INSERT INTO audit_events(id,occurred_at,actor_user_id,action,target_type,target_id,request_id,result,metadata) VALUES($1,$2,$3,$4,$5,$6,$7,'success',$8)")
        .bind(audit_id).bind(access.now.as_offset_date_time()).bind(access.actor_id.as_uuid()).bind(action)
        .bind(if session.is_some() { "auth_session" } else { "user" })
        .bind(session.map_or_else(|| access.user_id.to_string(), |id| id.to_string()))
        .bind(access.request_id.as_str()).bind(serde_json::json!({"administrator":access.administrator,"operation":metadata}))
        .execute(&mut **tx).await.map_err(unavailable)?;
    if let Some(ip) = &access.source_ip {
        sqlx::query("INSERT INTO audit_source_ips(audit_event_id,source_ip,created_at) VALUES($1,$2::inet,$3)")
            .bind(audit_id).bind(ip).bind(access.now.as_offset_date_time()).execute(&mut **tx).await.map_err(unavailable)?;
    }
    Ok(())
}
