//! Ed25519 JWT access-token issue and cryptographic validation.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use orbisync_application::{IdentityRepository, SecretString};
use orbisync_domain::{AccessTokenId, AuthSessionId, Timestamp, UserId};
use serde::{Deserialize, Serialize};

const ACCESS_TOKEN_LIFETIME_SECONDS: i64 = 15 * 60;

/// Realtime ticket lifetime. Short on purpose: tickets are not consumed
/// (D-7), so the replay window is bounded by this value alone.
const REALTIME_TICKET_LIFETIME_SECONDS: i64 = 60;

/// Audience marking a token as a realtime ticket. Must differ from the
/// access-token audience so neither is accepted where the other is expected.
const REALTIME_TICKET_AUDIENCE_SUFFIX: &str = ".realtime";

const CLOCK_SKEW_SECONDS: u64 = 30;

/// Access-token issue or validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccessTokenError {
    /// Key material, claims or signing failed.
    #[error("access token service is unavailable")]
    Unavailable,
    /// Token signature, algorithm, issuer, audience or claims are invalid.
    #[error("access token is invalid")]
    InvalidToken,
}

/// Validated JWT claims from ADR-002.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    sub: String,
    sid: String,
    jti: String,
    iss: String,
    aud: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}

impl AccessTokenClaims {
    /// Parses the authenticated user identifier.
    ///
    /// # Errors
    ///
    /// Returns invalid-token if the claim is not canonical UUIDv7.
    pub fn user_id(&self) -> Result<UserId, AccessTokenError> {
        UserId::parse(&self.sub).map_err(|_| AccessTokenError::InvalidToken)
    }

    /// Parses the bound session identifier.
    ///
    /// # Errors
    ///
    /// Returns invalid-token if the claim is not canonical UUIDv7.
    pub fn session_id(&self) -> Result<AuthSessionId, AccessTokenError> {
        AuthSessionId::parse(&self.sid).map_err(|_| AccessTokenError::InvalidToken)
    }

    /// Returns the expiry in Unix seconds.
    #[must_use]
    pub const fn expires_at_unix_seconds(&self) -> i64 {
        self.exp
    }
}

/// Ed25519 JWT service with an explicit issuer, audience and key ID.
pub struct AccessTokenService {
    encoding_key: EncodingKey,
    decoding_keys: HashMap<String, DecodingKey>,
    issuer: String,
    audience: String,
    key_id: String,
    access_token_ttl_seconds: u64,
}

/// Verification key identified by the JWT `kid` header.
pub struct VerificationKey<'a> {
    /// Stable, non-secret key identifier.
    pub key_id: &'a str,
    /// Ed25519 public key in PEM form.
    pub public_key_pem: &'a [u8],
}

impl core::fmt::Debug for AccessTokenService {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AccessTokenService")
            .field("encoding_key", &"[REDACTED]")
            .field("verification_key_count", &self.decoding_keys.len())
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("key_id", &self.key_id)
            .finish()
    }
}

impl AccessTokenService {
    /// Creates a service from an Ed25519 PKCS#8 private PEM, deriving the public key.
    ///
    /// The public key is derived from the private key via scalar multiplication,
    /// so only the private PEM needs to be supplied. The PEM may contain
    /// escaped `\n` sequences (as they appear when stored in a single-line env
    /// var) or real newlines; both are accepted. The key is never logged.
    ///
    /// # Errors
    ///
    /// Returns unavailable when the key cannot be parsed or the key id is empty.
    pub fn from_ed25519_private_pem(
        private_key_pem: &[u8],
        issuer: impl Into<String>,
        audience: impl Into<String>,
        key_id: impl Into<String>,
    ) -> Result<Self, AccessTokenError> {
        let key_id = key_id.into();
        if key_id.is_empty() {
            return Err(AccessTokenError::Unavailable);
        }
        // Normalise: the env var may contain literal `\n` escapes when set via
        // a single-line .env file or `docker compose --env-file`. Replace them
        // with real newlines before parsing, without logging the material.
        let pem_string = String::from_utf8_lossy(private_key_pem);
        let normalized = if pem_string.contains("\\n") {
            pem_string.replace("\\n", "\n")
        } else {
            pem_string.into_owned()
        };
        let normalized_bytes = normalized.as_bytes();
        // Derive public key via ed25519-dalek.
        let signing_key = ed25519_dalek::SigningKey::from_pkcs8_pem(&normalized)
            .map_err(|_| AccessTokenError::Unavailable)?;
        let verifying_key = signing_key.verifying_key();
        let public_pem = verifying_key_to_pem(&verifying_key);
        let decoding_key = DecodingKey::from_ed_pem(public_pem.as_bytes())
            .map_err(|_| AccessTokenError::Unavailable)?;
        let mut decoding_keys = HashMap::new();
        decoding_keys.insert(key_id.clone(), decoding_key);
        Ok(Self {
            encoding_key: EncodingKey::from_ed_pem(normalized_bytes)
                .map_err(|_| AccessTokenError::Unavailable)?,
            decoding_keys,
            issuer: issuer.into(),
            audience: audience.into(),
            key_id,
            access_token_ttl_seconds: ACCESS_TOKEN_LIFETIME_SECONDS as u64,
        })
    }

    /// Creates a service from Ed25519 PKCS#8 private and public PEM values.
    ///
    /// # Errors
    ///
    /// Returns unavailable when either key cannot be parsed.
    pub fn from_ed25519_pem(
        private_key_pem: &[u8],
        public_key_pem: &[u8],
        issuer: impl Into<String>,
        audience: impl Into<String>,
        key_id: impl Into<String>,
    ) -> Result<Self, AccessTokenError> {
        let key_id = key_id.into();
        if key_id.is_empty() {
            return Err(AccessTokenError::Unavailable);
        }
        let mut decoding_keys = HashMap::new();
        decoding_keys.insert(
            key_id.clone(),
            DecodingKey::from_ed_pem(public_key_pem).map_err(|_| AccessTokenError::Unavailable)?,
        );
        Ok(Self {
            encoding_key: EncodingKey::from_ed_pem(private_key_pem)
                .map_err(|_| AccessTokenError::Unavailable)?,
            decoding_keys,
            issuer: issuer.into(),
            audience: audience.into(),
            key_id,
            access_token_ttl_seconds: ACCESS_TOKEN_LIFETIME_SECONDS as u64,
        })
    }

    /// Creates a service with one active signing key and multiple verification keys.
    ///
    /// Rotation changes the active private key while retaining old public keys
    /// until every token signed by them has expired.
    ///
    /// # Errors
    ///
    /// Returns unavailable for an empty key id, a duplicate id, an absent active
    /// verification key, or malformed Ed25519 PEM.
    pub fn from_key_ring<'a>(
        active_private_key_pem: &[u8],
        active_key_id: impl Into<String>,
        verification_keys: impl IntoIterator<Item = VerificationKey<'a>>,
        issuer: impl Into<String>,
        audience: impl Into<String>,
    ) -> Result<Self, AccessTokenError> {
        let key_id = active_key_id.into();
        if key_id.is_empty() {
            return Err(AccessTokenError::Unavailable);
        }
        let mut decoding_keys = HashMap::new();
        for key in verification_keys {
            if key.key_id.is_empty()
                || decoding_keys
                    .insert(
                        key.key_id.to_owned(),
                        DecodingKey::from_ed_pem(key.public_key_pem)
                            .map_err(|_| AccessTokenError::Unavailable)?,
                    )
                    .is_some()
            {
                return Err(AccessTokenError::Unavailable);
            }
        }
        if !decoding_keys.contains_key(&key_id) {
            return Err(AccessTokenError::Unavailable);
        }
        Ok(Self {
            encoding_key: EncodingKey::from_ed_pem(active_private_key_pem)
                .map_err(|_| AccessTokenError::Unavailable)?,
            decoding_keys,
            issuer: issuer.into(),
            audience: audience.into(),
            key_id,
            access_token_ttl_seconds: ACCESS_TOKEN_LIFETIME_SECONDS as u64,
        })
    }

    /// Returns the configured access-token TTL in seconds.
    #[must_use]
    pub const fn access_token_ttl_seconds(&self) -> u64 {
        self.access_token_ttl_seconds
    }

    /// Overrides the access-token TTL (from `auth.access_token_ttl_seconds`).
    ///
    /// The caller (composition root) wires `Config::auth.access_token_ttl_seconds`.
    #[must_use]
    pub fn with_access_token_ttl(mut self, ttl_seconds: u64) -> Self {
        self.access_token_ttl_seconds = ttl_seconds;
        self
    }

    /// Issues a JWT containing every mandatory ADR-002 claim.
    ///
    /// The lifetime is `self.access_token_ttl_seconds`, wired from
    /// `auth.access_token_ttl_seconds` (`Config::auth.access_token_ttl_seconds`).
    ///
    /// # Errors
    ///
    /// Returns unavailable for timestamp conversion or signing failure.
    pub fn issue(
        &self,
        user_id: UserId,
        session_id: AuthSessionId,
        now: Timestamp,
    ) -> Result<SecretString, AccessTokenError> {
        self.issue_with_deadline(user_id, session_id, now, None)
            .map(|(token, _)| token)
    }

    /// Issues a signed JWT capped by the subject's absolute deadline.
    ///
    /// Returns the token and its actual `exp - iat` lifetime in seconds.
    /// Deadlines are rounded down to JWT seconds, never beyond the subject.
    ///
    /// # Errors
    ///
    /// Returns invalid-token when the deadline leaves no valid JWT lifetime,
    /// or unavailable on timestamp overflow or signing failure.
    pub fn issue_with_deadline(
        &self,
        user_id: UserId,
        session_id: AuthSessionId,
        now: Timestamp,
        absolute_deadline: Option<Timestamp>,
    ) -> Result<(SecretString, u64), AccessTokenError> {
        let issued_millis = now
            .to_unix_millis()
            .map_err(|_| AccessTokenError::Unavailable)?;
        let issued = issued_millis.div_euclid(1_000);
        let ttl = i64::try_from(self.access_token_ttl_seconds)
            .map_err(|_| AccessTokenError::Unavailable)?;
        let mut expires = issued
            .checked_add(ttl)
            .ok_or(AccessTokenError::Unavailable)?;
        if let Some(deadline) = absolute_deadline {
            let deadline_seconds = deadline
                .to_unix_millis()
                .map_err(|_| AccessTokenError::Unavailable)?
                .div_euclid(1_000);
            if deadline <= now || deadline_seconds <= issued {
                return Err(AccessTokenError::InvalidToken);
            }
            expires = expires.min(deadline_seconds);
        }
        let expires_in =
            u64::try_from(expires - issued).map_err(|_| AccessTokenError::Unavailable)?;
        let claims = AccessTokenClaims {
            sub: user_id.to_string(),
            sid: session_id.to_string(),
            jti: AccessTokenId::generate().to_string(),
            iss: self.issuer.clone(),
            aud: self.audience.clone(),
            iat: issued,
            nbf: issued,
            exp: expires,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.key_id.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding_key)
            .map(|token| (SecretString::new(token), expires_in))
            .map_err(|_| AccessTokenError::Unavailable)
    }

    /// Validates signature, EdDSA algorithm, issuer, audience and time claims.
    ///
    /// Session active-state validation remains a repository lookup performed by
    /// the identity application service after this cryptographic step.
    ///
    /// # Errors
    ///
    /// Returns invalid-token for every externally supplied failure.
    pub fn validate(
        &self,
        token: &SecretString,
        now: Timestamp,
    ) -> Result<AccessTokenClaims, AccessTokenError> {
        let header = jsonwebtoken::decode_header(token.expose_secret())
            .map_err(|_| AccessTokenError::InvalidToken)?;
        if header.alg != Algorithm::EdDSA {
            return Err(AccessTokenError::InvalidToken);
        }
        let key_id = header
            .kid
            .as_deref()
            .ok_or(AccessTokenError::InvalidToken)?;
        let decoding_key = self
            .decoding_keys
            .get(key_id)
            .ok_or(AccessTokenError::InvalidToken)?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.algorithms = vec![Algorithm::EdDSA];
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.leeway = CLOCK_SKEW_SECONDS;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        // Tighten defaults that became looser in jsonwebtoken 10/11 if left at default:
        // - leeway default is 60 but we need 30 (CLOCK_SKEW_SECONDS)
        // - validate_aud default is true but make it explicit to avoid future loosening
        validation.validate_aud = true;
        validation.reject_tokens_expiring_in_less_than = 0;
        validation.required_spec_claims = HashSet::from_iter([
            String::from("sub"),
            String::from("sid"),
            String::from("jti"),
            String::from("iss"),
            String::from("aud"),
            String::from("iat"),
            String::from("nbf"),
            String::from("exp"),
        ]);
        let claims = jsonwebtoken::decode::<AccessTokenClaims>(
            token.expose_secret(),
            decoding_key,
            &validation,
        )
        .map(|data| data.claims)
        .map_err(|_| AccessTokenError::InvalidToken)?;
        validate_times(&claims, now)?;
        Ok(claims)
    }

    /// Validates JWT claims and confirms the bound session remains active.
    ///
    /// # Errors
    ///
    /// Returns invalid-token for a missing, revoked, expired or differently
    /// bound session, and unavailable for a repository failure.
    pub async fn validate_active<R: IdentityRepository>(
        &self,
        token: &SecretString,
        now: Timestamp,
        repository: &R,
    ) -> Result<AccessTokenClaims, AccessTokenError> {
        let claims = self.validate(token, now)?;
        let session_id = claims.session_id()?;
        let user_id = claims.user_id()?;
        let session = repository
            .find_session(session_id)
            .await
            .map_err(|_| AccessTokenError::Unavailable)?
            .ok_or(AccessTokenError::InvalidToken)?;
        if !session.is_active_at(now) || session.user_id() != user_id {
            return Err(AccessTokenError::InvalidToken);
        }
        Ok(claims)
    }

    /// Issues a 60-second realtime ticket bound to the same user and session.
    ///
    /// The ticket uses a distinct audience (`<audience>.realtime`) so an access
    /// token is never accepted as a ticket and vice versa. Existing `issue` /
    /// `validate` behaviour is unchanged.
    ///
    /// # Errors
    ///
    /// Returns unavailable for timestamp conversion or signing failure.
    pub fn issue_realtime_ticket(
        &self,
        user_id: UserId,
        session_id: AuthSessionId,
        now: Timestamp,
    ) -> Result<SecretString, AccessTokenError> {
        let issued_millis = now
            .to_unix_millis()
            .map_err(|_| AccessTokenError::Unavailable)?;
        let issued = issued_millis.div_euclid(1_000);
        let claims = AccessTokenClaims {
            sub: user_id.to_string(),
            sid: session_id.to_string(),
            jti: AccessTokenId::generate().to_string(),
            iss: self.issuer.clone(),
            aud: format!("{}{}", self.audience, REALTIME_TICKET_AUDIENCE_SUFFIX),
            iat: issued,
            nbf: issued,
            exp: issued + REALTIME_TICKET_LIFETIME_SECONDS,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.key_id.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding_key)
            .map(SecretString::new)
            .map_err(|_| AccessTokenError::Unavailable)
    }

    /// Validates a realtime ticket's signature, audience, and time claims.
    ///
    /// Rejects access tokens because their audience lacks the realtime suffix.
    ///
    /// # Errors
    ///
    /// Returns invalid-token for every externally supplied failure.
    pub fn validate_realtime_ticket(
        &self,
        ticket: &SecretString,
        now: Timestamp,
    ) -> Result<AccessTokenClaims, AccessTokenError> {
        let header = jsonwebtoken::decode_header(ticket.expose_secret())
            .map_err(|_| AccessTokenError::InvalidToken)?;
        if header.alg != Algorithm::EdDSA {
            return Err(AccessTokenError::InvalidToken);
        }
        let key_id = header
            .kid
            .as_deref()
            .ok_or(AccessTokenError::InvalidToken)?;
        let decoding_key = self
            .decoding_keys
            .get(key_id)
            .ok_or(AccessTokenError::InvalidToken)?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.algorithms = vec![Algorithm::EdDSA];
        validation.set_issuer(&[self.issuer.as_str()]);
        let realtime_audience = format!("{}{}", self.audience, REALTIME_TICKET_AUDIENCE_SUFFIX);
        validation.set_audience(&[realtime_audience.as_str()]);
        validation.leeway = CLOCK_SKEW_SECONDS;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = true;
        validation.reject_tokens_expiring_in_less_than = 0;
        validation.required_spec_claims = HashSet::from_iter([
            String::from("sub"),
            String::from("sid"),
            String::from("jti"),
            String::from("iss"),
            String::from("aud"),
            String::from("iat"),
            String::from("nbf"),
            String::from("exp"),
        ]);
        let claims = jsonwebtoken::decode::<AccessTokenClaims>(
            ticket.expose_secret(),
            decoding_key,
            &validation,
        )
        .map(|data| data.claims)
        .map_err(|_| AccessTokenError::InvalidToken)?;
        validate_times(&claims, now)?;
        Ok(claims)
    }

    /// Returns the realtime audience for this service (exposed for testing).
    #[must_use]
    pub fn realtime_audience(&self) -> String {
        format!("{}{}", self.audience, REALTIME_TICKET_AUDIENCE_SUFFIX)
    }
}

fn verifying_key_to_pem(verifying_key: &ed25519_dalek::VerifyingKey) -> String {
    // Ed25519 SPKI: SEQUENCE( SEQUENCE(OID 1.3.101.112), BIT STRING(32 bytes) )
    // DER prefix taken from RFC 8410 §10.1.
    const SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = Vec::with_capacity(44);
    der.extend_from_slice(&SPKI_PREFIX);
    der.extend_from_slice(&verifying_key.to_bytes());
    let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    let mut start = 0;
    while start < b64.len() {
        let end = (start + 64).min(b64.len());
        pem.push_str(&b64[start..end]);
        pem.push('\n');
        start = end;
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    pem
}

fn validate_times(claims: &AccessTokenClaims, now: Timestamp) -> Result<(), AccessTokenError> {
    let now_seconds = now
        .to_unix_millis()
        .map_err(|_| AccessTokenError::Unavailable)?
        .div_euclid(1_000);
    let skew = i64::try_from(CLOCK_SKEW_SECONDS).map_err(|_| AccessTokenError::Unavailable)?;
    if claims.exp.saturating_add(skew) < now_seconds
        || claims.nbf > now_seconds.saturating_add(skew)
        || claims.iat > now_seconds.saturating_add(skew)
        || claims.exp <= claims.iat
    {
        return Err(AccessTokenError::InvalidToken);
    }
    Ok(())
}

#[cfg(test)]
mod realtime_ticket_tests {
    use super::AccessTokenService;
    use orbisync_domain::{AuthSessionId, Timestamp, UserId};

    // Test PEM pair generated via cryptography.ed25519 (deterministic for tests)
    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn service() -> AccessTokenService {
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("test service must build")
    }

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid")
    }

    #[test]
    fn access_token_is_rejected_as_realtime_ticket() {
        let svc = service();
        let token = svc
            .issue(UserId::generate(), AuthSessionId::generate(), now())
            .expect("issue access token");
        let result = svc.validate_realtime_ticket(&token, now());
        assert_eq!(result.unwrap_err(), super::AccessTokenError::InvalidToken);
    }

    #[test]
    fn realtime_ticket_is_rejected_as_access_token() {
        let svc = service();
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now())
            .expect("issue ticket");
        let result = svc.validate(&ticket, now());
        assert_eq!(result.unwrap_err(), super::AccessTokenError::InvalidToken);
    }

    #[test]
    fn realtime_ticket_expires_after_sixty_seconds() {
        let svc = service();
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now())
            .expect("issue ticket");
        assert!(svc.validate_realtime_ticket(&ticket, now()).is_ok());
        // 30 s clock skew means 61 s is still within leeway (60 + 30 = 90). Use 91 s.
        let later = Timestamp::from_unix_millis(1_700_000_000_000 + 91_000).expect("valid");
        let result = svc.validate_realtime_ticket(&ticket, later);
        assert_eq!(result.unwrap_err(), super::AccessTokenError::InvalidToken);
    }

    #[test]
    fn access_token_ttl_is_fifteen_minutes_and_ticket_is_sixty_seconds() {
        let svc = service();
        let now_ts = now();
        let access = svc
            .issue(UserId::generate(), AuthSessionId::generate(), now_ts)
            .expect("access");
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now_ts)
            .expect("ticket");
        let access_claims = svc.validate(&access, now_ts).expect("access valid");
        let ticket_claims = svc
            .validate_realtime_ticket(&ticket, now_ts)
            .expect("ticket valid");
        assert_eq!(
            access_claims.expires_at_unix_seconds() - access_claims.exp + access_claims.exp,
            access_claims.expires_at_unix_seconds()
        );
        assert_eq!(
            ticket_claims.exp - ticket_claims.iat,
            super::REALTIME_TICKET_LIFETIME_SECONDS
        );
        assert_eq!(
            access_claims.exp - access_claims.iat,
            super::ACCESS_TOKEN_LIFETIME_SECONDS
        );
    }

    #[test]
    fn same_audience_would_break_isolation() {
        // Mutation test: if issue_realtime_ticket used the same audience as access tokens,
        // both cross validations would succeed. This test documents that the current
        // implementation uses a distinct audience.
        let svc = service();
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now())
            .expect("ticket");
        // Try to validate ticket as access token with same audience (simulated by checking
        // that validate fails due to audience mismatch). If someone changes audience to be same,
        // this test will fail.
        let header = jsonwebtoken::decode_header(ticket.expose_secret()).expect("header");
        assert!(header.kid.is_some());
        // Ensure realtime audience is indeed different
        assert_ne!(svc.realtime_audience(), "orbisync-api");
        assert_eq!(svc.realtime_audience(), "orbisync-api.realtime");
        assert!(svc.validate(&ticket, now()).is_err());
        assert!(svc.validate_realtime_ticket(&ticket, now()).is_ok());
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::{AccessTokenError, AccessTokenService};
    use orbisync_application::SecretString;
    use orbisync_domain::{AuthSessionId, Timestamp, UserId};

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn service() -> AccessTokenService {
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test-key-1",
        )
        .expect("test service must build")
    }

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid")
    }

    #[test]
    fn signed_deadline_rounding_and_permanent_ttl() {
        let svc = service();
        let issued = now().checked_add_millis(900).expect("time");
        for (offset, expected) in [(Some(29_200), 30), (Some(3_600_000), 900), (None, 900)] {
            let deadline = offset.map(|ms| issued.checked_add_millis(ms).expect("deadline"));
            let (token, expires_in) = svc
                .issue_with_deadline(
                    UserId::generate(),
                    AuthSessionId::generate(),
                    issued,
                    deadline,
                )
                .expect("issue");
            let claims = svc.validate(&token, issued).expect("signature and claims");
            assert_eq!(claims.exp - claims.iat, expected);
            assert_eq!(expires_in, expected as u64);
            if let Some(deadline) = deadline {
                assert!(claims.exp * 1_000 <= deadline.to_unix_millis().expect("millis"));
            }
        }
    }

    #[test]
    fn session_issuer_refuses_elapsed_or_unrepresentable_deadlines() {
        let issued = now().checked_add_millis(500).expect("time");
        let issuer = crate::SessionIssuer::new(
            std::sync::Arc::new(orbisync_testkit::FakeIdentityStore::new()),
            std::sync::Arc::new(service()),
            std::sync::Arc::new(orbisync_testkit::FixedClock::new(issued)),
            vec![7; 32],
            900,
            2_592_000,
        );
        for offset in [-1_000, 0, 100] {
            let deadline = issued.checked_add_millis(offset).expect("deadline");
            assert!(
                issuer
                    .issue_parts(UserId::generate(), issued, Some(deadline))
                    .is_err()
            );
        }
        let parts = issuer
            .issue_parts(UserId::generate(), issued, None)
            .expect("permanent subject");
        let claims = service()
            .validate(&parts.access_token, issued)
            .expect("signed token");
        assert_eq!(claims.exp - claims.iat, 900);
        assert_eq!(parts.expires_in, 900);
        assert_eq!(
            parts.session_record.expires_at,
            issued.checked_add_millis(2_592_000_000).expect("expiry")
        );
    }

    fn corrupt_signature(token: &SecretString) -> SecretString {
        let s = token.expose_secret();
        // JWT is header.payload.signature ; corrupt 1 byte of signature
        let mut chars: Vec<char> = s.chars().collect();
        if let Some(pos) = s.rfind('.') {
            let sig_start = pos + 1;
            if sig_start < chars.len() {
                let orig = chars[sig_start];
                chars[sig_start] = if orig != 'A' { 'A' } else { 'B' };
            }
        } else if !chars.is_empty() {
            chars[0] = if chars[0] != 'A' { 'A' } else { 'B' };
        }
        SecretString::new(chars.into_iter().collect::<String>())
    }

    /// M1: 署名を1バイト壊したトークンは 401 (InvalidToken) になる
    #[test]
    fn m1_corrupted_signature_is_rejected() {
        let svc = service();
        let token = svc
            .issue(UserId::generate(), AuthSessionId::generate(), now())
            .expect("issue");
        let corrupted = corrupt_signature(&token);
        let err = svc.validate(&corrupted, now()).unwrap_err();
        assert_eq!(err, AccessTokenError::InvalidToken);
        // realtime ticketも同様に署名偽造を拒否する
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now())
            .expect("ticket");
        let corrupted_ticket = corrupt_signature(&ticket);
        let err2 = svc
            .validate_realtime_ticket(&corrupted_ticket, now())
            .unwrap_err();
        assert_eq!(err2, AccessTokenError::InvalidToken);
    }

    /// M2: exp を過去にしたトークンは 401 になる
    #[test]
    fn m2_expired_token_is_rejected() {
        let svc = service();
        let token = svc
            .issue(UserId::generate(), AuthSessionId::generate(), now())
            .expect("issue");
        // access token TTL 900s + leeway 30s = 930s 許容。931s 後は必ず InvalidToken
        let later_millis = 1_700_000_000_000 + 931_000;
        let later = Timestamp::from_unix_millis(later_millis).expect("valid");
        let err = svc.validate(&token, later).unwrap_err();
        assert_eq!(err, AccessTokenError::InvalidToken);

        // リアルタイムチケットも同様（TTL 60s + 30s = 90s 許容、91s 後は失効）
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now())
            .expect("ticket");
        let later_ticket = Timestamp::from_unix_millis(1_700_000_000_000 + 91_000).expect("valid");
        let err2 = svc
            .validate_realtime_ticket(&ticket, later_ticket)
            .unwrap_err();
        assert_eq!(err2, AccessTokenError::InvalidToken);
    }

    /// M3: aud を別の値にしたトークンは 401 になる
    #[test]
    fn m3_wrong_audience_is_rejected() {
        let svc = service();
        let token = svc
            .issue(UserId::generate(), AuthSessionId::generate(), now())
            .expect("issue");
        // 別の audience を期待するサービスで検証すると失敗する
        let svc_other_aud = AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "other-audience",
            "test-key-1",
        )
        .expect("other aud service");
        let err = svc_other_aud.validate(&token, now()).unwrap_err();
        assert_eq!(err, AccessTokenError::InvalidToken);

        // リアルタイムチケットでも audience 不一致は拒否される
        let ticket = svc
            .issue_realtime_ticket(UserId::generate(), AuthSessionId::generate(), now())
            .expect("ticket");
        let err2 = svc.validate(&ticket, now()).unwrap_err();
        assert_eq!(err2, AccessTokenError::InvalidToken);
        let err3 = svc_other_aud
            .validate_realtime_ticket(&ticket, now())
            .unwrap_err();
        assert_eq!(err3, AccessTokenError::InvalidToken);
    }
}
