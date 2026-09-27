//! The single session-issuing path shared by every authentication method.
//!
//! ADR-026 §2 requires the four methods to differ only in how they establish
//! *who* the subject is. Everything after that -- session id, access token,
//! refresh token and its digest, the atomic commit, and the returned
//! `LoginResult` -- happens here, once. If each method assembled its own
//! session and tokens, the paths would drift apart and the guarantee that a
//! guest reaches exactly the same authorization and audit route as a local
//! account would hold only by inspection.

use std::sync::Arc;

use orbisync_application::{
    ApplicationError, EphemeralSubjectRecord, ExternalIdentityRecord, LoginAuditEvent,
    LoginRefreshRecord, LoginSessionRecord, LoginTransactionStore, NewSubjectUser, RequestId,
    SecretString, SubjectCommit,
};
use orbisync_domain::{AuthSession, AuthSessionId, RoleId, Timestamp, UserId};
use uuid::Uuid;

use crate::token::AccessTokenService;

/// A subject whose identity has been established, ready to receive a session.
#[derive(Debug, Clone)]
pub struct IssuableSubject {
    /// The subject's user id. Always server-generated or server-resolved:
    /// no request field reaches this value.
    pub user_id: UserId,
    /// The `users` row to create, or `None` when the subject already exists.
    pub new_user: Option<NewSubjectUser>,
    /// Roles to grant on creation, resolved from configuration at startup.
    pub grant_roles: Vec<RoleId>,
    /// Temporary-subject ledger row, for guest and name-only.
    pub ephemeral: Option<EphemeralSubjectRecord>,
    /// External identity mapping, for external subjects.
    pub external: Option<ExternalIdentityRecord>,
}

/// Issues sessions and tokens for subjects that hold no password credential.
///
/// The local login path performs its own credential verification and then
/// reuses [`SessionIssuer::issue_parts`] for the token material, so both
/// routes compute expiry, digests and audit the same way.
pub struct SessionIssuer {
    transactions: Arc<dyn LoginTransactionStore>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<dyn orbisync_domain::Clock>,
    refresh_hmac_key: Vec<u8>,
    access_ttl_seconds: u64,
    refresh_ttl_seconds: u64,
}

/// The token material issued for one session.
///
/// Returned so the local login path can reuse the same computation without
/// going through [`SessionIssuer::issue`], whose commit shape is specific to
/// credential-less subjects.
pub struct IssuedTokens {
    /// Newly generated session id.
    pub session_id: AuthSessionId,
    /// Signed access token.
    pub access_token: SecretString,
    /// Actual signed access-token lifetime in seconds.
    pub expires_in: u64,
    /// Raw refresh token, returned to the caller exactly once.
    pub refresh_token: SecretString,
    /// Refresh row to persist; carries only the digest.
    pub refresh_record: LoginRefreshRecord,
    /// Session row to persist.
    pub session_record: LoginSessionRecord,
}

impl SessionIssuer {
    /// Creates an issuer over injected ports.
    ///
    /// # Panics
    ///
    /// Panics when `refresh_hmac_key` is empty, matching `LoginService`: an
    /// empty key would make every refresh digest forgeable, so this fails at
    /// startup rather than at the first login.
    #[must_use]
    pub fn new(
        transactions: Arc<dyn LoginTransactionStore>,
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
            transactions,
            tokens,
            clock,
            refresh_hmac_key,
            access_ttl_seconds,
            refresh_ttl_seconds,
        }
    }

    /// Returns the current time from the injected clock.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Builds the session and token material for `user_id`.
    ///
    /// `absolute_deadline` is the temporary subject's cap. When present, the
    /// access, session and refresh expiry are clamped to it, so the very first token
    /// pair already cannot outlive the subject. Issuing the first refresh
    /// token at the global lifetime and only clamping later rotations would
    /// leave one token valid past the deadline.
    ///
    /// # Errors
    ///
    /// Returns a port failure when the configured lifetimes overflow the
    /// timestamp range or the access token cannot be signed.
    pub fn issue_parts(
        &self,
        user_id: UserId,
        now: Timestamp,
        absolute_deadline: Option<Timestamp>,
    ) -> Result<IssuedTokens, ApplicationError> {
        let session_id = AuthSessionId::generate();
        let (access_token, expires_in) = self
            .tokens
            .issue_with_deadline(user_id, session_id, now, absolute_deadline)
            .map_err(|_| ApplicationError::port_failure("token issuance failed"))?;

        let refresh_ttl_millis = i64::try_from(
            self.refresh_ttl_seconds
                .checked_mul(1_000)
                .ok_or_else(|| ApplicationError::port_failure("refresh TTL overflow"))?,
        )
        .map_err(|_| ApplicationError::port_failure("refresh TTL overflow"))?;
        let mut expires_at = now
            .checked_add_millis(refresh_ttl_millis)
            .map_err(|_| ApplicationError::port_failure("session expiry overflow"))?;
        if let Some(deadline) = absolute_deadline
            && deadline < expires_at
        {
            expires_at = deadline;
        }

        let raw_refresh = generate_refresh_token();
        let digest = hmac_digest(&self.refresh_hmac_key, &raw_refresh);
        Ok(IssuedTokens {
            session_id,
            expires_in,
            access_token: SecretString::new(access_token.expose_secret().to_owned()),
            refresh_token: raw_refresh,
            refresh_record: LoginRefreshRecord {
                token_id: Uuid::now_v7().to_string(),
                family_id: orbisync_domain::RefreshTokenFamilyId::generate().to_string(),
                digest,
                issued_at: now,
                expires_at,
            },
            session_record: LoginSessionRecord {
                id: session_id,
                user_id,
                created_at: now,
                expires_at,
            },
        })
    }

    /// Issues a session for a credential-less subject and commits it.
    ///
    /// The user row, role grants, ledger row, session, refresh token and audit
    /// all commit together; a failure leaves no partially created subject.
    ///
    /// # Errors
    ///
    /// Returns a port failure when token issuance or the transaction fails.
    pub async fn issue(
        &self,
        subject: IssuableSubject,
        request_id: RequestId,
        source_ip: Option<String>,
        action: &'static str,
    ) -> Result<orbisync_application::LoginResult, ApplicationError> {
        let now = self.clock.now();
        let deadline = subject.ephemeral.as_ref().map(|value| value.expires_at);
        let parts = self.issue_parts(subject.user_id, now, deadline)?;

        let audit = LoginAuditEvent {
            occurred_at: now,
            request_id,
            source_ip,
            action,
            actor_id: Some(subject.user_id),
            succeeded: true,
        };

        self.transactions
            .commit_subject_success(SubjectCommit {
                user_id: subject.user_id,
                new_user: subject.new_user.as_ref(),
                grant_roles: &subject.grant_roles,
                ephemeral: subject.ephemeral.as_ref(),
                external: subject.external.as_ref(),
                session: &parts.session_record,
                refresh: &parts.refresh_record,
                audit: &audit,
            })
            .await
            .map_err(|error| {
                if subject.external.is_some()
                    && error == orbisync_application::IdentityPortError::Conflict
                {
                    ApplicationError::new(
                        orbisync_application::ApplicationErrorKind::Conflict,
                        "external identity mapping changed",
                    )
                } else {
                    ApplicationError::port_failure("subject issue transaction failed")
                }
            })?;

        let session = AuthSession::new(
            parts.session_id,
            subject.user_id,
            now,
            parts.session_record.expires_at,
        )
        .map_err(|_| ApplicationError::port_failure("session invariant"))?;

        Ok(orbisync_application::LoginResult {
            user_id: subject.user_id,
            session,
            access_token: parts.access_token,
            refresh_token: parts.refresh_token,
            expires_in: parts.expires_in,
        })
    }
}

impl core::fmt::Debug for SessionIssuer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionIssuer")
            .field("access_ttl_seconds", &self.access_ttl_seconds)
            .field("refresh_ttl_seconds", &self.refresh_ttl_seconds)
            .field("refresh_hmac_key", &"[REDACTED]")
            .finish()
    }
}

/// Generates a 256-bit opaque refresh token.
pub(crate) fn generate_refresh_token() -> SecretString {
    use base64::Engine as _;
    use rand::Rng as _;
    let mut bytes = [0_u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    SecretString::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Computes the stored digest of a refresh token. The raw value is never
/// persisted.
pub(crate) fn hmac_digest(key: &[u8], token: &SecretString) -> [u8; 32] {
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    #[allow(clippy::expect_used)]
    let mut mac = HmacSha256::new_from_slice(key).expect("non-empty HMAC key");
    mac.update(token.expose_secret().as_bytes());
    mac.finalize().into_bytes().into()
}
