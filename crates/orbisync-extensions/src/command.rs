//! Scoped service-token authentication and inbound command gateway (ADR-007).
use hmac::{Hmac, Mac};
use orbisync_application::extension_command::{
    AUDIT_READ, ENTITY_READ, ExtensionCommand, ExtensionCommandApi, ExtensionCommandUseCase,
    ExtensionReadPort, ExtensionTokenStore, valid_scope,
};
use orbisync_application::{ApplicationError, ApplicationErrorKind, SecretString};
use sha2::Sha256;
use std::{collections::BTreeSet, sync::Arc};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// Environment-only HMAC key. It is independent of user authentication keys.
pub const TOKEN_KEY_ENV: &str = "ORBISYNC_EXTENSION_TOKEN_HMAC_KEY";
/// Token lifetime fixed by ADR-007.
pub const TOKEN_LIFETIME_DAYS: i64 = 30;

/// Gateway owns token authentication; the application owns authorization and reads.
pub struct ExtensionGateway {
    store: Arc<dyn ExtensionTokenStore>,
    key: Vec<u8>,
    commands: ExtensionCommandUseCase,
}

impl ExtensionGateway {
    /// Refuses weak or empty HMAC keys.
    pub fn new(
        store: Arc<dyn ExtensionTokenStore>,
        reads: Arc<dyn ExtensionReadPort>,
        key: Vec<u8>,
    ) -> Result<Self, ApplicationError> {
        if key.len() < 32 {
            return Err(ApplicationError::new(
                ApplicationErrorKind::DomainRule,
                "extension HMAC key must contain at least 32 bytes",
            ));
        }
        Ok(Self {
            store,
            key,
            commands: ExtensionCommandUseCase::new(reads),
        })
    }
    fn digest(&self, token: &str) -> Result<Vec<u8>, ApplicationError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key)
            .map_err(|_| ApplicationError::port_failure("extension key unavailable"))?;
        mac.update(b"orbisync.extension.token.v1\0");
        mac.update(token.as_bytes());
        Ok(mac.finalize().into_bytes().to_vec())
    }
    /// Operator-only issuance/rotation: one active token per extension, old token revoked atomically.
    pub async fn issue(
        &self,
        extension_id: Uuid,
        scopes: BTreeSet<String>,
    ) -> Result<SecretString, ApplicationError> {
        if scopes.is_empty()
            || scopes.len() > 258
            || scopes.iter().any(|scope| !valid_scope(scope))
            || !scopes
                .iter()
                .any(|scope| scope == ENTITY_READ || scope == AUDIT_READ)
            || (scopes.contains(ENTITY_READ)
                && !scopes.iter().any(|scope| scope.starts_with("instances:")))
        {
            return Err(ApplicationError::new(
                ApplicationErrorKind::DomainRule,
                "invalid extension scopes",
            ));
        }
        let bytes: [u8; 32] = rand::random();
        let token = format!(
            "orb_ext_{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let now = OffsetDateTime::now_utc();
        self.store
            .replace(
                extension_id,
                &self.digest(&token)?,
                &scopes,
                now,
                now + Duration::days(TOKEN_LIFETIME_DAYS),
            )
            .await?;
        Ok(SecretString::new(token))
    }
    /// Operator-only revocation.
    pub async fn revoke(&self, extension_id: Uuid) -> Result<(), ApplicationError> {
        self.store.revoke(extension_id).await
    }
}

#[async_trait::async_trait]
impl ExtensionCommandApi for ExtensionGateway {
    async fn execute(
        &self,
        token: SecretString,
        command: ExtensionCommand,
    ) -> Result<serde_json::Value, ApplicationError> {
        let raw = token.expose_secret();
        let invalid = || {
            ApplicationError::new(
                ApplicationErrorKind::Unauthenticated,
                "invalid extension credential",
            )
        };
        if raw.len() != 72
            || !raw.starts_with("orb_ext_")
            || !raw.as_bytes()[8..]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(invalid());
        }
        let grant = self
            .store
            .resolve(&self.digest(raw)?, OffsetDateTime::now_utc())
            .await?
            .ok_or_else(invalid)?;
        self.commands.execute(grant, command).await
    }
}
