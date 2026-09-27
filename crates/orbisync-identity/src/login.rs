//! Login business orchestration moved from HTTP handler.
//!
//! The service performs one lookup, one Argon2 verify, checks lock / status,
//! then delegates to [`LoginTransactionStore`] for the atomic persistence
//! demanded by `auth-authorization.md:317-330` and
//! `observability-and-config.md:260-268`. No password, raw token or raw
//! `LoginId` is written to audit, logs or metrics.

use std::sync::Arc;

use orbisync_application::{
    ApplicationError, ApplicationErrorKind, IdentityRepository, LoginAuditEvent, LoginCommit,
    LoginRefreshRecord, LoginSessionRecord, LoginTransactionStore, RequestId, SecretString,
};
use orbisync_domain::{AuthSession, AuthSessionId, LoginId, RefreshTokenFamilyId, UserStatus};
use uuid::Uuid;

use crate::{PasswordError, PasswordService, token::AccessTokenService};

/// Orchestrates the complete login use case.
pub struct LoginService {
    repository: Arc<dyn IdentityRepository>,
    transactions: Arc<dyn LoginTransactionStore>,
    passwords: PasswordService,
    tokens: Arc<AccessTokenService>,
    clock: Arc<dyn orbisync_domain::Clock>,
    refresh_hmac_key: Vec<u8>,
    access_ttl_seconds: u64,
    refresh_ttl_seconds: u64,
}

impl LoginService {
    /// Creates a service over injected ports.
    #[must_use]
    pub fn new(
        repository: Arc<dyn IdentityRepository>,
        transactions: Arc<dyn LoginTransactionStore>,
        passwords: PasswordService,
        tokens: Arc<AccessTokenService>,
        clock: Arc<dyn orbisync_domain::Clock>,
        refresh_hmac_key: Vec<u8>,
        access_ttl_seconds: u64,
        refresh_ttl_seconds: u64,
    ) -> Self {
        assert!(
            !refresh_hmac_key.is_empty(),
            "refresh HMAC key must not be empty"
        );
        Self {
            repository,
            transactions,
            passwords,
            tokens,
            clock,
            refresh_hmac_key,
            access_ttl_seconds,
            refresh_ttl_seconds,
        }
    }

    /// Authenticates, issues session / tokens and records an audit atomically.
    ///
    /// # Errors
    ///
    /// Returns `Unauthenticated` for missing, disabled, locked or mismatched
    /// credentials without revealing which condition held. Returns `RateLimited`
    /// when the bounded hashing workers are busy. Returns `PortFailure` when
    /// the transactional commit or the audit insert fails; the whole login then
    /// fails closed and no partial row remains.
    pub async fn login(
        &self,
        login_id: LoginId,
        password: SecretString,
        request_id: RequestId,
        source_ip: Option<String>,
    ) -> Result<orbisync_application::LoginResult, ApplicationError> {
        let now = self.clock.now();

        // Single logical lookup. Missing accounts use the dummy PHC so every
        // normal failure costs one Argon2 verify.
        let account = self
            .repository
            .find_login(&login_id)
            .await
            .map_err(|_| ApplicationError::port_failure("identity repository unavailable"))?;

        let hash = account.as_ref().map_or_else(
            || self.passwords.dummy_hash().clone(),
            |value| value.credential.password_hash().clone(),
        );
        let password_clone = password.clone();
        let verified = self
            .passwords
            .verify(password_clone.clone(), hash)
            .await
            .map_err(|error| match error {
                PasswordError::RateLimited => ApplicationError::new(
                    ApplicationErrorKind::RateLimited,
                    "password hashing capacity exhausted",
                ),
                _ => ApplicationError::port_failure("password verification unavailable"),
            })?;

        // Failure branch: unknown account – record enumeration-resistant audit
        // without mutating any credential row.
        let Some(account) = account else {
            let audit = LoginAuditEvent {
                occurred_at: now,
                request_id: request_id.clone(),
                source_ip: source_ip.clone(),
                action: "auth.login",
                actor_id: None,
                succeeded: false,
            };
            self.transactions
                .record_failure(None, audit)
                .await
                .map_err(|_| ApplicationError::port_failure("audit unavailable"))?;
            return Err(unauthenticated());
        };

        let locked = account.credential.is_locked_at(now);
        let enabled = account.user.status() == UserStatus::Active;
        if !verified || locked || !enabled {
            // Wrong password / locked / disabled: increment failure counter
            // atomically with audit. Actor is `None` for enumeration resistance –
            // all failures look identical in the audit table.
            let audit = LoginAuditEvent {
                occurred_at: now,
                request_id: request_id.clone(),
                source_ip: source_ip.clone(),
                action: "auth.login",
                actor_id: None,
                succeeded: false,
            };
            self.transactions
                .record_failure(Some(account.user.id()), audit)
                .await
                .map_err(|_| ApplicationError::port_failure("audit unavailable"))?;
            return Err(unauthenticated());
        }

        // Success branch: prepare session, refresh token, audit and optional rehash.
        let expected_failed = account.credential.failed_login_count();
        let expected_locked = account.credential.locked_until();
        let new_hash = if PasswordService::needs_rehash(account.credential.password_hash()) {
            let upgraded = self
                .passwords
                .rehash_existing(password_clone)
                .await
                .map_err(|error| match error {
                    PasswordError::RateLimited => ApplicationError::new(
                        ApplicationErrorKind::RateLimited,
                        "password hashing capacity exhausted",
                    ),
                    _ => ApplicationError::port_failure("password rehash unavailable"),
                })?;
            Some(upgraded)
        } else {
            None
        };

        let user_id = account.user.id();
        let session_id = AuthSessionId::generate();
        let access_token = self
            .tokens
            .issue(user_id, session_id, now)
            .map_err(|_| ApplicationError::port_failure("token issuance failed"))?;

        let refresh_ttl_millis = i64::try_from(
            self.refresh_ttl_seconds
                .checked_mul(1_000)
                .ok_or_else(|| ApplicationError::port_failure("refresh TTL overflow"))?,
        )
        .map_err(|_| ApplicationError::port_failure("refresh TTL overflow"))?;
        let expires_at = now
            .checked_add_millis(refresh_ttl_millis)
            .map_err(|_| ApplicationError::port_failure("session expiry overflow"))?;

        let session_record = LoginSessionRecord {
            id: session_id,
            user_id,
            created_at: now,
            expires_at,
        };

        let raw_refresh = generate_refresh_token();
        let digest = hmac_digest(&self.refresh_hmac_key, &raw_refresh);
        let token_id = Uuid::now_v7().to_string();
        let family_id = RefreshTokenFamilyId::generate().to_string();
        let refresh_record = LoginRefreshRecord {
            token_id: token_id.clone(),
            family_id: family_id.clone(),
            digest,
            issued_at: now,
            expires_at,
        };

        let audit = LoginAuditEvent {
            occurred_at: now,
            request_id: request_id.clone(),
            source_ip,
            action: "auth.login",
            actor_id: Some(user_id),
            succeeded: true,
        };

        let commit = LoginCommit {
            user_id,
            session: &session_record,
            refresh: &refresh_record,
            audit: &audit,
            expected_failed_count: expected_failed,
            expected_locked_until: expected_locked,
            new_password_hash: new_hash.as_ref(),
        };

        self.transactions
            .commit_success(commit)
            .await
            .map_err(|_| ApplicationError::port_failure("login transaction failed"))?;

        // Build domain session for the transport response (no persistence left).
        let session = AuthSession::new(session_id, user_id, now, expires_at)
            .map_err(|_| ApplicationError::port_failure("session invariant"))?;

        Ok(orbisync_application::LoginResult {
            user_id,
            session,
            access_token: SecretString::new(access_token.expose_secret().to_owned()),
            refresh_token: raw_refresh,
            expires_in: self.access_ttl_seconds,
        })
    }
}

fn unauthenticated() -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorKind::Unauthenticated,
        "authentication failed",
    )
}

// Refresh token generation and digesting live in `session_issuer` so that the
// credential path and the credential-less paths produce byte-identical token
// material (ADR-026 §2).
use crate::session_issuer::{generate_refresh_token, hmac_digest};
