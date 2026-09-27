//! Application boundary for scoped, out-of-process extension commands.

use crate::{ApplicationError, ApplicationErrorKind, SecretString};
use orbisync_domain::{EntityId, InstanceId};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};
use time::OffsetDateTime;
use uuid::Uuid;

/// Capability for reading entities in explicitly granted instances.
pub const ENTITY_READ: &str = "commands:entity:read";
/// Capability for reading the deployment's secret-free audit projection.
pub const AUDIT_READ: &str = "commands:audit:read";

/// Validates a closed set of scopes. Unknown and administrative scopes fail closed.
pub fn valid_scope(scope: &str) -> bool {
    matches!(scope, ENTITY_READ | AUDIT_READ)
        || scope
            .strip_prefix("instances:")
            .is_some_and(|id| InstanceId::parse(id).is_ok_and(|value| value.to_string() == id))
}

/// Current registration permissions intersected with a token's issued scopes.
#[derive(Debug, Clone)]
pub struct ExtensionGrant {
    /// Extension identity, never a user or authentication session.
    pub extension_id: Uuid,
    /// Current manifest capabilities.
    pub capabilities: BTreeSet<String>,
    /// Current manifest scopes intersected with issued scopes.
    pub scopes: BTreeSet<String>,
}

/// Digest-only persistence. Adapters must check expiry and manifest state atomically.
#[async_trait::async_trait]
pub trait ExtensionTokenStore: Send + Sync {
    /// Replaces the active token atomically after checking current manifest grants.
    async fn replace(
        &self,
        extension_id: Uuid,
        digest: &[u8],
        scopes: &BTreeSet<String>,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<(), ApplicationError>;
    /// Resolves a valid digest, rejecting suspended, revoked and expired credentials.
    async fn resolve(
        &self,
        digest: &[u8],
        now: OffsetDateTime,
    ) -> Result<Option<ExtensionGrant>, ApplicationError>;
    /// Revokes the extension's active token.
    async fn revoke(&self, extension_id: Uuid) -> Result<(), ApplicationError>;
}

/// Closed v1 command vocabulary. No administration or runtime mutation variant exists.
#[derive(Debug, Clone)]
pub enum ExtensionCommand {
    /// Read one live entity in a granted instance.
    EntityGet {
        /// Explicitly granted live instance.
        instance_id: InstanceId,
        /// Entity belonging to that instance.
        entity_id: EntityId,
    },
    /// Read one audit entry by its UUIDv7 identifier.
    AuditGet {
        /// Audit record UUIDv7.
        event_id: Uuid,
    },
}

/// Authorized read adapters; extensions never receive their handles.
#[async_trait::async_trait]
pub trait ExtensionReadPort: Send + Sync {
    /// Reads one live entity without mutating or activating an instance.
    async fn entity(
        &self,
        instance_id: InstanceId,
        entity_id: EntityId,
    ) -> Result<Option<Value>, ApplicationError>;
    /// Reads the public, secret-free projection of one audit event.
    async fn audit(&self, event_id: Uuid) -> Result<Option<Value>, ApplicationError>;
}

/// Use case that enforces capabilities before invoking any read port.
pub struct ExtensionCommandUseCase {
    reads: Arc<dyn ExtensionReadPort>,
}

impl ExtensionCommandUseCase {
    /// Creates the use case with application read ports.
    pub fn new(reads: Arc<dyn ExtensionReadPort>) -> Self {
        Self { reads }
    }
    /// Checks both capability and token scope, including resource restriction.
    pub async fn execute(
        &self,
        grant: ExtensionGrant,
        command: ExtensionCommand,
    ) -> Result<Value, ApplicationError> {
        let capability = match &command {
            ExtensionCommand::EntityGet { .. } => ENTITY_READ,
            ExtensionCommand::AuditGet { .. } => AUDIT_READ,
        };
        if !grant.capabilities.contains(capability) || !grant.scopes.contains(capability) {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "extension scope denied",
            ));
        }
        let result = match command {
            ExtensionCommand::EntityGet {
                instance_id,
                entity_id,
            } => {
                if !grant.scopes.contains(&format!("instances:{instance_id}")) {
                    return Err(ApplicationError::new(
                        ApplicationErrorKind::NotAuthorized,
                        "extension instance denied",
                    ));
                }
                self.reads.entity(instance_id, entity_id).await?
            }
            ExtensionCommand::AuditGet { event_id } => self.reads.audit(event_id).await?,
        };
        result.ok_or_else(|| {
            ApplicationError::new(ApplicationErrorKind::NotFound, "resource not found")
        })
    }
}

/// Inbound gateway port exposed to HTTP. Authentication stays in extension_gateway.
#[async_trait::async_trait]
pub trait ExtensionCommandApi: Send + Sync {
    /// Authenticates the opaque token, then invokes the authorized application use case.
    async fn execute(
        &self,
        token: SecretString,
        command: ExtensionCommand,
    ) -> Result<Value, ApplicationError>;
}
