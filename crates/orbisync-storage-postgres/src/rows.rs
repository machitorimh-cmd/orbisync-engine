//! SQLx row representations kept inside the PostgreSQL adapter.

use time::OffsetDateTime;
use uuid::Uuid;

/// A row from `users`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    /// Stable user identifier.
    pub id: Uuid,
    /// Unique login identifier.
    pub login_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Persisted user status.
    pub status: String,
    /// Whether a password change is required.
    pub must_change_password: bool,
    /// Optimistic concurrency revision.
    pub revision: i64,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last update timestamp.
    pub updated_at: OffsetDateTime,
}

/// A row from `auth_sessions`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuthSessionRow {
    /// Stable session identifier.
    pub id: Uuid,
    /// Owning user.
    pub user_id: Uuid,
    /// Persisted session status.
    pub status: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Expiry timestamp.
    pub expires_at: OffsetDateTime,
    /// Revocation timestamp, when revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// Non-secret machine-readable revocation reason.
    pub revocation_reason: Option<String>,
    /// Optimistic concurrency revision.
    pub revision: i64,
}

/// A row from `refresh_tokens`; it contains only a digest, never a raw token.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RefreshTokenRow {
    /// Stable token row identifier.
    pub id: Uuid,
    /// Owning authentication session.
    pub session_id: Uuid,
    /// Rotation family identifier.
    pub family_id: Uuid,
    /// Keyed digest of the raw token.
    pub token_digest: Vec<u8>,
    /// Issue timestamp.
    pub issued_at: OffsetDateTime,
    /// Expiry timestamp.
    pub expires_at: OffsetDateTime,
    /// Consumption timestamp.
    pub consumed_at: Option<OffsetDateTime>,
    /// Replacement token row.
    pub replaced_by: Option<Uuid>,
    /// Token reuse detection timestamp.
    pub reuse_detected_at: Option<OffsetDateTime>,
}

/// A row from `idempotency_records`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IdempotencyRecordRow {
    /// Client-provided idempotency key.
    pub key: Uuid,
    /// Authenticated actor, when present.
    pub actor_user_id: Option<Uuid>,
    /// Stable operation name.
    pub operation: String,
    /// Digest of the canonical request.
    pub request_hash: Vec<u8>,
    /// Processing state.
    pub state: String,
    /// Stored response status.
    pub status_code: Option<i16>,
    /// Stored response media type.
    pub response_content_type: Option<String>,
    /// Application-encrypted response body.
    pub response_body: Option<Vec<u8>>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Fixed 24-hour expiry timestamp.
    pub expires_at: OffsetDateTime,
    /// Owner token for lease validation (P2-C2).
    pub owner_token: Option<Uuid>,
    /// Lease expiry for InProgress handling (P2-C2).
    pub lease_until: Option<OffsetDateTime>,
}
