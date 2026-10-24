//! Secret-free session inventory and scoped, audited session management.

use orbisync_domain::{AuthSessionId, AuthSessionStatus, Timestamp, UserId};

use crate::{IdentityPortError, Page, PageRequest, RequestId};

/// Public inventory projection; deliberately contains no token material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    /// Stable session identifier.
    pub id: AuthSessionId,
    /// Session creation time.
    pub created_at: Timestamp,
    /// Absolute session deadline.
    pub expires_at: Timestamp,
    /// Stored lifecycle state (expiry is derived from the deadline).
    pub status: AuthSessionStatus,
    /// Revocation instant, when revoked.
    pub revoked_at: Option<Timestamp>,
    /// User-supplied advisory label; never an authentication factor.
    pub device_label: Option<String>,
}

/// Server-authenticated actor and explicit user scope for a mutation.
#[derive(Debug, Clone)]
pub struct SessionAccess {
    /// Authenticated actor.
    pub actor_id: UserId,
    /// Owner of the sessions being managed.
    pub user_id: UserId,
    /// Whether the transport checked the separate administrative permission.
    pub administrator: bool,
    /// Request correlation identifier.
    pub request_id: RequestId,
    /// Already trusted-proxy-filtered source IP.
    pub source_ip: Option<String>,
    /// Mutation instant.
    pub now: Timestamp,
}

impl SessionAccess {
    /// Rejects cross-user access unless the administrative boundary approved it.
    pub fn validate(&self) -> Result<(), IdentityPortError> {
        if self.actor_id != self.user_id && !self.administrator {
            return Err(IdentityPortError::NotFound);
        }
        Ok(())
    }
}

/// Session subset selected by a revocation operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSelection {
    /// One session, always scoped to the supplied owner.
    One(AuthSessionId),
    /// Every other session of the owner.
    OtherThan(AuthSessionId),
    /// All sessions of the owner (administrative operation).
    All,
}

/// Session management boundary. All mutations and mandatory audits are atomic.
#[async_trait::async_trait]
pub trait SessionManagementPort: Send + Sync + 'static {
    /// Lists one owner's sessions in stable identifier order with a scoped cursor.
    async fn list(
        &self,
        access: &SessionAccess,
        page: &PageRequest,
    ) -> Result<Page<SessionView>, IdentityPortError>;

    /// Revokes selected active sessions; returns the number newly revoked.
    /// A missing or differently-owned individual session is `NotFound`.
    async fn revoke(
        &self,
        access: &SessionAccess,
        selection: SessionSelection,
    ) -> Result<u64, IdentityPortError>;

    /// Changes an advisory label for a session owned by the authenticated user.
    async fn set_label(
        &self,
        access: &SessionAccess,
        session: AuthSessionId,
        label: Option<String>,
    ) -> Result<(), IdentityPortError>;
}

/// Validates the optional advisory device label at every adapter boundary.
pub fn validate_device_label(label: Option<&str>) -> Result<(), IdentityPortError> {
    if label.is_some_and(|value| {
        value.trim().is_empty()
            || value.chars().count() > 128
            || value.chars().any(char::is_control)
    }) {
        return Err(IdentityPortError::InvalidRequest);
    }
    Ok(())
}
