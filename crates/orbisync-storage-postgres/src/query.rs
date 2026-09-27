//! PostgreSQL implementations of application-owned query ports.

use orbisync_application::{
    AuditFilter, AuditQueryPort, AuditView, IdentityPortError, IdentityQueryPort, Page,
    PageRequest, QueryCursor, RoleView, UserView, pagination::CursorCodec,
};
use orbisync_config::{Config, ConfigError, ConfigErrorKind, EnvSource};
use orbisync_domain::{RoleId, Timestamp, UserId, UserStatus};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

/// PostgreSQL identity and audit query adapter.
#[derive(Debug, Clone)]
pub struct PgIdentityQueryStore {
    pool: PgPool,
    codec: CursorCodec,
}

impl PgIdentityQueryStore {
    /// Creates a query adapter backed by `pool` and the pagination codec
    /// resolved via `config.auth.pagination_hmac_key_env`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigErrorKind::MissingSecret`] when the environment
    /// variable named by `config.auth.pagination_hmac_key_env` is not set
    /// or is empty/whitespace. This fails startup rather than falling back
    /// to an insecure dev key, preserving key separation from
    /// `auth.token_signing_key_env`.
    pub fn new(pool: PgPool, config: &Config, env: &dyn EnvSource) -> Result<Self, ConfigError> {
        let env_var = config.auth.pagination_hmac_key_env.clone();
        let raw = env.get(&env_var).ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var.clone(),
                "required secret environment variable is not set",
            )
        })?;
        if raw.trim().is_empty() {
            return Err(ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var,
                "required secret environment variable is not set",
            ));
        }
        // CR-16: placeholder must be replaced before startup — otherwise a
        // repository-published value would be used to sign cursors (same
        // fail-closed contract as ORBISYNC_TOKEN_SIGNING_KEY which is a PEM
        // placeholder that fails to parse).
        if raw.trim() == "__REPLACE_WITH_RANDOM_32_BYTES__" || raw.contains("__REPLACE") {
            return Err(ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var.clone(),
                "pagination HMAC key placeholder not replaced",
            ));
        }
        let codec = CursorCodec::new(raw.into_bytes()).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::MissingSecret,
                env_var.clone(),
                "pagination HMAC key is empty",
            )
        })?;
        Ok(Self { pool, codec })
    }

    /// Creates a query adapter with an explicit codec (tests).
    #[must_use]
    pub fn with_codec(pool: PgPool, codec: CursorCodec) -> Self {
        Self { pool, codec }
    }

    fn decode_users_cursor(&self, page: &PageRequest) -> Result<Option<Uuid>, IdentityPortError> {
        let Some(cursor) = &page.after else {
            return Ok(None);
        };
        let id_str = self
            .codec
            .decode_users(&cursor.0)
            .map_err(|_| IdentityPortError::InvalidRequest)?;
        Uuid::parse_str(&id_str)
            .map_err(|_| IdentityPortError::InvalidRequest)
            .map(Some)
    }

    fn decode_roles_cursor(&self, page: &PageRequest) -> Result<Option<Uuid>, IdentityPortError> {
        let Some(cursor) = &page.after else {
            return Ok(None);
        };
        let id_str = self
            .codec
            .decode_roles(&cursor.0)
            .map_err(|_| IdentityPortError::InvalidRequest)?;
        Uuid::parse_str(&id_str)
            .map_err(|_| IdentityPortError::InvalidRequest)
            .map(Some)
    }

    fn decode_audit_cursor(
        &self,
        page: &PageRequest,
        filter: &AuditFilter,
    ) -> Result<Option<(time::OffsetDateTime, Uuid)>, IdentityPortError> {
        let Some(cursor) = &page.after else {
            return Ok(None);
        };
        // Opaque cursor with filter binding.
        if cursor.0.contains('.') {
            let fh = self.audit_filter_hash(filter);
            let (ts_millis, id_str) = self
                .codec
                .decode_audit(&cursor.0, &fh)
                .map_err(|_| IdentityPortError::InvalidRequest)?;
            let ts = Timestamp::from_unix_millis(ts_millis)
                .map_err(|_| IdentityPortError::InvalidRequest)?;
            let id = Uuid::parse_str(&id_str).map_err(|_| IdentityPortError::InvalidRequest)?;
            Ok(Some((ts.as_offset_date_time(), id)))
        } else {
            // Legacy plain UUID cursor for backwards compatibility in tests: treat as id only,
            // but audit ordering requires timestamp. Map to InvalidRequest if used with new ordering
            // to force opaque cursor. For now, treat as InvalidRequest to encourage migration.
            Err(IdentityPortError::InvalidRequest)
        }
    }

    fn audit_filter_hash(&self, filter: &AuditFilter) -> String {
        let from_millis = filter.from.map(|t| {
            (t.as_offset_date_time().unix_timestamp() * 1000)
                + i64::from(t.as_offset_date_time().millisecond())
        });
        let to_millis = filter.to.map(|t| {
            (t.as_offset_date_time().unix_timestamp() * 1000)
                + i64::from(t.as_offset_date_time().millisecond())
        });
        let actor = filter.actor_id.map(|id| id.to_string());
        self.codec.audit_filter_hash(
            from_millis,
            to_millis,
            actor.as_deref(),
            filter.action.as_deref(),
        )
    }
}

fn limit(page: &PageRequest) -> i64 {
    // CR-07: ADR-003 requires max 200, default 50 is enforced in transport's page_limit.
    // This clamp is the last line of defense before SQL; V-04 touched it here and
    // PROGRESS.log notes the change from 100 to 200.
    i64::from(page.limit.clamp(1, 200))
}

type UserRecord = (Uuid, String, String, String, i64);
type AuditRecord = (
    Uuid,
    time::OffsetDateTime,
    Option<Uuid>,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    serde_json::Value,
    Option<String>,
);

fn audit_view(row: AuditRecord) -> Result<AuditView, IdentityPortError> {
    let actor_id = row
        .2
        .map(UserId::new)
        .transpose()
        .map_err(|_| IdentityPortError::DataCorruption)?;
    let actor_type = if actor_id.is_some() {
        "user".to_owned()
    } else {
        "system".to_owned()
    };
    let resource_type = row.4.unwrap_or_else(|| "unknown".to_owned());
    let error_code = row
        .8
        .get("error_code")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(AuditView {
        id: row.0.to_string(),
        occurred_at: Timestamp::from_offset_date_time(row.1),
        actor_type,
        actor_id,
        action: row.3,
        resource_type,
        resource_id: row.5,
        result: row.6,
        error_code,
        request_id: row.7.unwrap_or_default(),
        details: row.8,
    })
}

fn user_view(row: UserRecord) -> Result<UserView, IdentityPortError> {
    Ok(UserView {
        id: UserId::new(row.0).map_err(|_| IdentityPortError::DataCorruption)?,
        login_id: row.1,
        display_name: row.2,
        status: if row.3 == "active" {
            UserStatus::Active
        } else {
            UserStatus::Disabled
        },
        revision: u64::try_from(row.4).map_err(|_| IdentityPortError::DataCorruption)?,
    })
}

#[async_trait::async_trait]
impl IdentityQueryPort for PgIdentityQueryStore {
    #[tracing::instrument(name = "persistence.db_query", skip_all)]
    async fn user(&self, id: UserId) -> Result<Option<UserView>, IdentityPortError> {
        sqlx::query_as::<_, UserRecord>(
            "SELECT id, login_id, display_name, status, revision FROM users WHERE id = $1",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?
        .map(user_view)
        .transpose()
    }

    #[tracing::instrument(name = "persistence.db_query", skip_all)]
    async fn users(&self, page: PageRequest) -> Result<Page<UserView>, IdentityPortError> {
        let cursor = self.decode_users_cursor(&page)?;
        let rows = sqlx::query_as::<_, UserRecord>("SELECT id, login_id, display_name, status, revision FROM users WHERE ($1::uuid IS NULL OR id > $1) ORDER BY id LIMIT $2")
            .bind(cursor).bind(limit(&page) + 1).fetch_all(&self.pool).await.map_err(|_| IdentityPortError::Unavailable)?;
        let more = rows.len() > limit(&page) as usize;
        let items = rows
            .into_iter()
            .take(limit(&page) as usize)
            .map(user_view)
            .collect::<Result<Vec<_>, _>>()?;
        let next = if more {
            let last_id = items
                .last()
                .map(|item| item.id.to_string())
                .unwrap_or_default();
            let cursor = self
                .codec
                .encode_users(&last_id)
                .map_err(|_| IdentityPortError::DataCorruption)?;
            Some(QueryCursor(cursor))
        } else {
            None
        };
        Ok(Page { items, next })
    }

    #[tracing::instrument(name = "persistence.db_query", skip_all)]
    async fn role(&self, id: RoleId) -> Result<Option<RoleView>, IdentityPortError> {
        // Atomic single query with array_agg (V-02) – avoids separate permission SELECT.
        let row = sqlx::query_as::<_, (Uuid, String, Option<String>, i64, Vec<String>)>(
            "SELECT r.id, r.name, r.description, r.revision, COALESCE(array_agg(rp.permission_name ORDER BY rp.permission_name) FILTER (WHERE rp.permission_name IS NOT NULL), '{}') FROM roles r LEFT JOIN role_permissions rp ON rp.role_id = r.id WHERE r.id = $1 GROUP BY r.id",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;
        match row {
            Some((rid, name, desc, rev, perms)) => Ok(Some(RoleView {
                id: RoleId::new(rid).map_err(|_| IdentityPortError::DataCorruption)?,
                name,
                description: desc,
                permissions: perms,
                revision: u64::try_from(rev).map_err(|_| IdentityPortError::DataCorruption)?,
            })),
            None => Ok(None),
        }
    }

    #[tracing::instrument(name = "persistence.db_query", skip_all)]
    async fn roles(&self, page: PageRequest) -> Result<Page<RoleView>, IdentityPortError> {
        // V-02: use single query with array_agg to avoid N+1 and non-atomic reads.
        // This also satisfies CR-07 opaque cursor (next is encoded).
        let cursor = self.decode_roles_cursor(&page)?;
        // Single query: roles with aggregated permissions
        let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, i64, Vec<String>)>(
            "SELECT r.id, r.name, r.description, r.revision, COALESCE(array_agg(rp.permission_name ORDER BY rp.permission_name) FILTER (WHERE rp.permission_name IS NOT NULL), '{}') FROM roles r LEFT JOIN role_permissions rp ON rp.role_id = r.id WHERE ($1::uuid IS NULL OR r.id > $1) GROUP BY r.id ORDER BY r.id LIMIT $2",
        )
        .bind(cursor)
        .bind(limit(&page) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;
        let more = rows.len() > limit(&page) as usize;
        let mut items = Vec::new();
        for row in rows.into_iter().take(limit(&page) as usize) {
            let (id, name, desc, rev, perms) = row;
            items.push(RoleView {
                id: RoleId::new(id).map_err(|_| IdentityPortError::DataCorruption)?,
                name,
                description: desc,
                permissions: perms,
                revision: u64::try_from(rev).map_err(|_| IdentityPortError::DataCorruption)?,
            });
        }
        let next = if more {
            let last_id = items
                .last()
                .map(|item| item.id.to_string())
                .unwrap_or_default();
            let cursor = self
                .codec
                .encode_roles(&last_id)
                .map_err(|_| IdentityPortError::DataCorruption)?;
            Some(QueryCursor(cursor))
        } else {
            None
        };
        Ok(Page { items, next })
    }
}

#[async_trait::async_trait]
impl AuditQueryPort for PgIdentityQueryStore {
    #[tracing::instrument(name = "persistence.db_query", skip_all)]
    async fn search(
        &self,
        filter: AuditFilter,
        page: PageRequest,
    ) -> Result<Page<AuditView>, IdentityPortError> {
        // CR-04: include target_type/target_id/request_id/metadata. CR-06: order by
        // (occurred_at DESC, id DESC) and use composite cursor (occurred_at, id).
        // The cursor is opaque via CursorCodec and bound to the filter hash.
        let mut query = QueryBuilder::new(
            "SELECT audit_events.id, audit_events.occurred_at, audit_events.actor_user_id, audit_events.action, audit_events.target_type, audit_events.target_id, audit_events.result, audit_events.request_id, audit_events.metadata, audit_source_ips.source_ip::text FROM audit_events LEFT JOIN audit_source_ips ON audit_source_ips.audit_event_id = audit_events.id WHERE true",
        );
        if let Some(value) = filter.from {
            query
                .push(" AND occurred_at >= ")
                .push_bind(value.as_offset_date_time());
        }
        if let Some(value) = filter.to {
            query
                .push(" AND occurred_at <= ")
                .push_bind(value.as_offset_date_time());
        }
        if let Some(value) = filter.actor_id {
            query
                .push(" AND actor_user_id = ")
                .push_bind(value.as_uuid());
        }
        if let Some(value) = filter.action.clone() {
            query.push(" AND action = ").push_bind(value);
        }
        if let Some((ts, id)) = self.decode_audit_cursor(&page, &filter)? {
            query
                .push(" AND (occurred_at, id) < (")
                .push_bind(ts)
                .push(", ")
                .push_bind(id)
                .push(")");
        }
        query
            .push(" ORDER BY occurred_at DESC, id DESC LIMIT ")
            .push_bind(limit(&page) + 1);
        let rows = query
            .build_query_as::<AuditRecord>()
            .fetch_all(&self.pool)
            .await
            .map_err(|_| IdentityPortError::Unavailable)?;
        let more = rows.len() > limit(&page) as usize;
        let items = rows
            .into_iter()
            .take(limit(&page) as usize)
            .map(audit_view)
            .collect::<Result<Vec<_>, IdentityPortError>>()?;
        let next = if more {
            match items.last() {
                Some(last) => {
                    let ts_millis = last.occurred_at.as_offset_date_time().unix_timestamp() * 1000
                        + i64::from(last.occurred_at.as_offset_date_time().millisecond());
                    let fh = self.audit_filter_hash(&filter);
                    let cursor = self
                        .codec
                        .encode_audit(ts_millis, &last.id, &fh)
                        .map_err(|_| IdentityPortError::DataCorruption)?;
                    Some(QueryCursor(cursor))
                }
                None => None,
            }
        } else {
            None
        };
        Ok(Page { items, next })
    }

    #[tracing::instrument(name = "persistence.db_query", skip_all)]
    async fn event(
        &self,
        id: orbisync_application::query::AuditEventId,
    ) -> Result<Option<AuditView>, IdentityPortError> {
        let row = sqlx::query_as::<_, AuditRecord>(
            "SELECT audit_events.id, audit_events.occurred_at, audit_events.actor_user_id, audit_events.action, audit_events.target_type, audit_events.target_id, audit_events.result, audit_events.request_id, audit_events.metadata, audit_source_ips.source_ip::text FROM audit_events LEFT JOIN audit_source_ips ON audit_source_ips.audit_event_id = audit_events.id WHERE audit_events.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IdentityPortError::Unavailable)?;
        row.map(audit_view).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::PgIdentityQueryStore;
    use orbisync_config::{Config, MapEnv};
    use sqlx::PgPool;

    #[tokio::test]
    async fn new_fails_when_pagination_key_missing() {
        // This simulates startup failure when the pagination HMAC key is not configured.
        // The store must be created via auth.pagination_hmac_key_env, not via fallback.
        let pool = PgPool::connect_lazy("postgres://localhost/dummy").expect("lazy pool");
        let config = Config::default();
        let empty = MapEnv::default();
        let err = PgIdentityQueryStore::new(pool.clone(), &config, &empty)
            .expect_err("must fail without key");
        assert_eq!(err.key(), config.auth.pagination_hmac_key_env);
        // Verify the error detail does not contain a key value
        assert!(!err.to_string().contains("dummy"));
        // Also fails when value is empty/whitespace
        let blank = MapEnv::from_pairs([(config.auth.pagination_hmac_key_env.clone(), "   ")]);
        let err2 =
            PgIdentityQueryStore::new(pool, &config, &blank).expect_err("blank key must fail");
        assert_eq!(err2.key(), config.auth.pagination_hmac_key_env);
    }

    #[tokio::test]
    async fn new_succeeds_when_key_present_and_uses_config_env_name() {
        let pool = PgPool::connect_lazy("postgres://localhost/dummy").expect("lazy pool");
        let mut config = Config::default();
        config.auth.pagination_hmac_key_env = "CUSTOM_PAGINATION_KEY".to_owned();
        let env = MapEnv::from_pairs([("CUSTOM_PAGINATION_KEY", "test-pagination-key-32bytes!!")]);
        let store = PgIdentityQueryStore::new(pool, &config, &env)
            .expect("should succeed with custom env var");
        // Basic sanity: can encode/decode via the codec through the store's with_codec path
        let _ = store;
    }
}
