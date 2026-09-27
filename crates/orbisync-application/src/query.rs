//! Transport-neutral identity and audit query contracts.

use orbisync_domain::{RoleId, Timestamp, UserId, UserStatus};

use crate::IdentityPortError;

/// Opaque cursor used for stable keyset pagination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryCursor(pub String);

/// Requested page size and optional continuation cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    /// Maximum number of records to return.
    pub limit: u32,
    /// Cursor returned by the previous page.
    pub after: Option<QueryCursor>,
}

/// One page of query results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// Records in stable ascending order.
    pub items: Vec<T>,
    /// Cursor for the next page, absent at the end.
    pub next: Option<QueryCursor>,
}

/// Secret-free user projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserView {
    /// Stable user identifier.
    pub id: UserId,
    /// Administrator-visible login identifier.
    pub login_id: String,
    /// User-facing display name.
    pub display_name: String,
    /// Account lifecycle state.
    pub status: UserStatus,
    /// Optimistic concurrency revision.
    pub revision: u64,
}

/// Role projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleView {
    /// Stable role identifier.
    pub id: RoleId,
    /// Unique role name.
    pub name: String,
    /// Optional administrator description.
    pub description: Option<String>,
    /// Sorted allow-only permission names.
    pub permissions: Vec<String>,
    /// Optimistic concurrency revision, at least 1 per OpenAPI `minimum: 1`.
    pub revision: u64,
}

/// Filters accepted by the append-only audit query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuditFilter {
    /// Earliest included timestamp.
    pub from: Option<Timestamp>,
    /// Latest included timestamp.
    pub to: Option<Timestamp>,
    /// Optional authenticated actor.
    pub actor_id: Option<UserId>,
    /// Optional exact action name.
    pub action: Option<String>,
}

/// Secret-free audit projection matching `AuditEvent` schema.
///
/// `actor_type` is derived as `"user"` when `actor_id` is present and
/// `"system"` otherwise; no additional actor classification is persisted.
/// `resource_type` comes from `audit_events.target_type` (or `"unknown"`
/// when NULL), `request_id` from `audit_events.request_id`, and `details`
/// from `audit_events.metadata`. `error_code` is extracted from
/// `metadata.error_code` when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditView {
    /// Event identifier encoded as canonical UUID.
    pub id: String,
    /// Event occurrence time.
    pub occurred_at: Timestamp,
    /// Actor classification (`user` | `system`).
    pub actor_type: String,
    /// Acting user, when authenticated.
    pub actor_id: Option<UserId>,
    /// Stable action name.
    pub action: String,
    /// Target resource type.
    pub resource_type: String,
    /// Target resource identifier.
    pub resource_id: Option<String>,
    /// Stable operation result (`success` | `failure`).
    pub result: String,
    /// Optional machine error code from metadata.
    pub error_code: Option<String>,
    /// Server-generated request correlation identifier.
    pub request_id: String,
    /// Non-secret structured details from `metadata`.
    pub details: serde_json::Value,
}

/// Read-side port for user and role administration.
#[async_trait::async_trait]
pub trait IdentityQueryPort: Send + Sync + 'static {
    /// Finds a user by immutable identifier.
    async fn user(&self, id: UserId) -> Result<Option<UserView>, IdentityPortError>;
    /// Lists users using stable keyset pagination.
    async fn users(&self, page: PageRequest) -> Result<Page<UserView>, IdentityPortError>;
    /// Finds a role by immutable identifier.
    async fn role(&self, id: RoleId) -> Result<Option<RoleView>, IdentityPortError>;
    /// Lists roles using stable keyset pagination.
    async fn roles(&self, page: PageRequest) -> Result<Page<RoleView>, IdentityPortError>;
}

/// Stable audit event identifier (UUIDv7).
pub type AuditEventId = uuid::Uuid;

/// Read-side port for audit search.
#[async_trait::async_trait]
pub trait AuditQueryPort: Send + Sync + 'static {
    /// Searches audit records in descending occurrence order.
    async fn search(
        &self,
        filter: AuditFilter,
        page: PageRequest,
    ) -> Result<Page<AuditView>, IdentityPortError>;

    /// Fetches a single audit event by id.
    async fn event(&self, id: AuditEventId) -> Result<Option<AuditView>, IdentityPortError>;
}
