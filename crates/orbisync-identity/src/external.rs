//! External identity through signed-token verification (ADR-026 §8).
//!
//! The design documents describe an `IdentityProvider` trait but never
//! implemented one; this is the first working adapter. It verifies a signed
//! JWT of the shape an OIDC ID Token has, without the discovery or code flow
//! around it.
//!
//! Keys come from a static JWKS file on disk and are never fetched over HTTP.
//! That keeps the whole path exercisable offline against a local signing
//! issuer, and removes the fetch, cache, retry and staleness questions a
//! network-backed key source would add.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use orbisync_application::{
    ApplicationError, ApplicationErrorKind, ExternalAuthError, ExternalIdentity,
    ExternalIdentityRecord, ExternalIdentityStore, IdentityProvider, LoginResult, NewSubjectUser,
    RequestId,
};
use orbisync_domain::{AuthMethod, LoginId, RoleId, Timestamp, UserId, UserKind};
use serde::Deserialize;

use crate::session_issuer::{IssuableSubject, SessionIssuer};

/// One verification key, pinned to the algorithm it may be used with.
struct VerificationKey {
    key: DecodingKey,
    algorithm: Algorithm,
}

/// A JSON Web Key as read from the configured file.
#[derive(Debug, Deserialize)]
struct JwkEntry {
    kid: String,
    alg: String,
    #[serde(default)]
    kty: String,
    #[serde(default)]
    crv: Option<String>,
    /// Ed25519 public key, base64url. Present for `OKP` keys.
    #[serde(default)]
    x: Option<String>,
    /// RSA modulus, base64url.
    #[serde(default)]
    n: Option<String>,
    /// RSA exponent, base64url.
    #[serde(default)]
    e: Option<String>,
    /// EC x coordinate is `x`; the y coordinate is here.
    #[serde(default)]
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<JwkEntry>,
}

/// Verifies signed tokens from one configured issuer.
pub struct JwtIdentityProvider {
    issuer: String,
    audience: String,
    expected_algorithm: Algorithm,
    leeway_seconds: u64,
    path: PathBuf,
    keys: RwLock<Arc<ActiveKeys>>,
    reload_gate: Mutex<()>,
}

struct ActiveKeys {
    generation: u64,
    keys: HashMap<String, VerificationKey>,
}

fn load_keys(
    path: &std::path::Path,
    expected_algorithm: Algorithm,
) -> Result<HashMap<String, VerificationKey>, ApplicationError> {
    const MAX_BYTES: u64 = 262_144;
    let file = std::fs::File::open(path)
        .map_err(|_| config_error("identity provider key file could not be read"))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| config_error("identity provider key file could not be read"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(config_error("identity provider key file exceeds 256 KiB"));
    }
    let set: JwkSet = serde_json::from_slice(&bytes)
        .map_err(|_| config_error("identity provider key file is invalid JSON"))?;
    if set.keys.is_empty() || set.keys.len() > 64 {
        return Err(config_error(
            "identity provider must have between one and 64 keys",
        ));
    }
    let mut keys = HashMap::new();
    for entry in set.keys {
        if entry.kid.is_empty() || entry.kid.len() > 128 || entry.kid.chars().any(char::is_control)
        {
            return Err(config_error("identity provider key identifier is invalid"));
        }
        let algorithm = parse_algorithm(&entry.alg)
            .ok_or_else(|| config_error("identity provider key algorithm is unsupported"))?;
        if algorithm != expected_algorithm {
            return Err(config_error(
                "identity provider key algorithm differs from configuration",
            ));
        }
        let key = decoding_key(&entry, algorithm)?;
        if keys
            .insert(entry.kid, VerificationKey { key, algorithm })
            .is_some()
        {
            return Err(config_error("duplicate identity provider key identifier"));
        }
    }
    Ok(keys)
}

impl JwtIdentityProvider {
    /// Loads the provider from a static JWKS file.
    ///
    /// Every key is pinned to the algorithm named in its own entry, and an
    /// entry whose algorithm differs from the configured one is refused at load
    /// time. Verification later requires the token's `alg` to equal the pinned
    /// algorithm, so a token cannot select a weaker algorithm than the key was
    /// published for -- the shape of an algorithm-confusion attack.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the file cannot be read or parsed,
    /// when it holds no usable key, or when the configured algorithm is not
    /// one this build accepts. Startup fails rather than running with a key
    /// set that cannot verify anything.
    pub fn from_jwks_file(
        path: &std::path::Path,
        issuer: String,
        audience: String,
        algorithm: &str,
        leeway_seconds: u64,
    ) -> Result<Self, ApplicationError> {
        let expected_algorithm = parse_algorithm(algorithm).ok_or_else(|| {
            config_error(format!("`{algorithm}` is not an accepted token algorithm"))
        })?;
        let keys = load_keys(path, expected_algorithm)?;
        Ok(Self {
            issuer,
            audience,
            expected_algorithm,
            leeway_seconds,
            path: path.to_path_buf(),
            keys: RwLock::new(Arc::new(ActiveKeys {
                generation: 1,
                keys,
            })),
            reload_gate: Mutex::new(()),
        })
    }

    /// Returns the configured issuer, so startup can compare it against the
    /// internal token issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Validates the configured local file, audits the attempt and atomically
    /// publishes it. Requests cannot select a path or change issuer/audience.
    /// Include old and new public keys in the file for an overlap period;
    /// removing a key stops subsequent authentications using that key.
    /// Already issued OrbiSync sessions retain their ordinary expiry/revocation.
    pub async fn reload_keys(
        &self,
        audit: &dyn orbisync_application::ExternalKeyAuditStore,
        actor_id: UserId,
        request_id: RequestId,
        now: Timestamp,
    ) -> Result<orbisync_application::ExternalKeyReloadSummary, ApplicationError> {
        let _gate = self.reload_gate.lock().await;
        let path = self.path.clone();
        let algorithm = self.expected_algorithm;
        let loaded = tokio::task::spawn_blocking(move || load_keys(&path, algorithm))
            .await
            .map_err(|_| config_error("identity provider key loading failed"))?;
        let active = Arc::clone(&*self.keys.read().await);
        let candidate = match loaded {
            Ok(keys) => keys,
            Err(_) => {
                audit
                    .record_reload(orbisync_application::ExternalKeyReloadAudit {
                        actor_id,
                        request_id,
                        occurred_at: now,
                        succeeded: false,
                        generation: active.generation,
                        key_count: active.keys.len(),
                    })
                    .await
                    .map_err(|_| config_error("key reload audit unavailable"))?;
                return Err(ApplicationError::new(
                    ApplicationErrorKind::DomainRule,
                    "configured external key file is invalid",
                ));
            }
        };
        let generation = active
            .generation
            .checked_add(1)
            .ok_or_else(|| config_error("key generation exhausted"))?;
        let mut key_ids: Vec<_> = candidate.keys().cloned().collect();
        key_ids.sort();
        let summary = orbisync_application::ExternalKeyReloadSummary {
            generation,
            key_count: candidate.len(),
            key_ids,
        };
        audit
            .record_reload(orbisync_application::ExternalKeyReloadAudit {
                actor_id,
                request_id,
                occurred_at: now,
                succeeded: true,
                generation,
                key_count: candidate.len(),
            })
            .await
            .map_err(|_| config_error("key reload audit unavailable"))?;
        *self.keys.write().await = Arc::new(ActiveKeys {
            generation,
            keys: candidate,
        });
        Ok(summary)
    }
}

/// Returns the key type an algorithm's keys must declare.
const fn expected_key_type(algorithm: Algorithm) -> &'static str {
    match algorithm {
        Algorithm::RS256 => "RSA",
        Algorithm::ES256 => "EC",
        _ => "OKP",
    }
}

impl core::fmt::Debug for JwtIdentityProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Key material is never rendered: these are public verification keys,
        // but printing them in a log or a panic message serves no purpose and
        // makes it harder to keep key handling auditable.
        f.debug_struct("JwtIdentityProvider")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("algorithm", &self.expected_algorithm)
            .field(
                "key_count",
                &self.keys.try_read().ok().map(|keys| keys.keys.len()),
            )
            .finish()
    }
}

fn decoding_key(entry: &JwkEntry, algorithm: Algorithm) -> Result<DecodingKey, ApplicationError> {
    validate_key_material(entry, algorithm)?;
    // A key whose declared type does not match its algorithm is a
    // misconfiguration; loading it anyway would mean verifying against a key
    // the issuer published for a different purpose.
    let expected_kty = expected_key_type(algorithm);
    if !entry.kty.is_empty() && entry.kty != expected_kty {
        return Err(config_error(format!(
            "key `{}` declares kty `{}` but {} keys must be `{expected_kty}`",
            entry.kid, entry.kty, entry.alg
        )));
    }
    let missing = |field: &str| {
        config_error(format!(
            "key `{}` is missing the `{field}` parameter",
            entry.kid
        ))
    };
    match algorithm {
        Algorithm::EdDSA => {
            let x = entry.x.as_deref().ok_or_else(|| missing("x"))?;
            Ok(DecodingKey::from_ed_components(x).map_err(|error| {
                config_error(format!(
                    "key `{}` is not a valid Ed25519 key: {error}",
                    entry.kid
                ))
            })?)
        }
        Algorithm::RS256 => {
            let n = entry.n.as_deref().ok_or_else(|| missing("n"))?;
            let e = entry.e.as_deref().ok_or_else(|| missing("e"))?;
            Ok(DecodingKey::from_rsa_components(n, e).map_err(|error| {
                config_error(format!(
                    "key `{}` is not a valid RSA key: {error}",
                    entry.kid
                ))
            })?)
        }
        Algorithm::ES256 => {
            let x = entry.x.as_deref().ok_or_else(|| missing("x"))?;
            let y = entry.y.as_deref().ok_or_else(|| missing("y"))?;
            Ok(DecodingKey::from_ec_components(x, y).map_err(|error| {
                config_error(format!(
                    "key `{}` is not a valid EC key: {error}",
                    entry.kid
                ))
            })?)
        }
        _ => Err(config_error(format!(
            "key `{}` uses an algorithm this build does not accept",
            entry.kid
        ))),
    }
}

fn validate_key_material(entry: &JwkEntry, algorithm: Algorithm) -> Result<(), ApplicationError> {
    let invalid = || config_error("identity provider public key material is invalid");
    let component = |value: &Option<String>| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())
    };
    match algorithm {
        Algorithm::EdDSA => {
            if entry.crv.as_deref().is_some_and(|curve| curve != "Ed25519") {
                return Err(invalid());
            }
            let bytes: [u8; 32] = component(&entry.x)?.try_into().map_err(|_| invalid())?;
            let key = ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| invalid())?;
            if key.is_weak() {
                return Err(invalid());
            }
        }
        Algorithm::ES256 => {
            if entry.crv.as_deref().is_some_and(|curve| curve != "P-256") {
                return Err(invalid());
            }
            let x = component(&entry.x)?;
            let y = component(&entry.y)?;
            if x.len() != 32 || y.len() != 32 {
                return Err(invalid());
            }
            let mut point = Vec::with_capacity(65);
            point.push(4);
            point.extend(x);
            point.extend(y);
            aws_lc_rs::signature::ParsedPublicKey::new(
                &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED,
                &point,
            )
            .map_err(|_| invalid())?;
        }
        Algorithm::RS256 => {
            let n = component(&entry.n)?;
            let e = component(&entry.e)?;
            if n.is_empty()
                || n.len() > 1024
                || n[0] == 0
                || e.is_empty()
                || e.len() > 4
                || e[0] == 0
                || n.last().is_none_or(|b| b & 1 == 0)
                || e.last().is_none_or(|b| b & 1 == 0)
            {
                return Err(invalid());
            }
            let bits = n.len() * 8 - n[0].leading_zeros() as usize;
            let exponent = e
                .iter()
                .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte));
            if !(2048..=8192).contains(&bits) || exponent < 3 {
                return Err(invalid());
            }
            use aws_lc_rs::encoding::{AsDer as _, PublicKeyX509Der};
            let components = aws_lc_rs::rsa::PublicKeyComponents { n: &n, e: &e };
            let der: PublicKeyX509Der<'static> = components.as_der().map_err(|_| invalid())?;
            aws_lc_rs::rsa::PublicKey::from_der(der.as_ref()).map_err(|_| invalid())?;
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

/// Parses an accepted algorithm name.
///
/// `none` is absent by construction, together with every symmetric algorithm:
/// an HMAC algorithm would let anyone holding the published verification key
/// mint tokens.
fn parse_algorithm(value: &str) -> Option<Algorithm> {
    match value {
        "EdDSA" => Some(Algorithm::EdDSA),
        "RS256" => Some(Algorithm::RS256),
        "ES256" => Some(Algorithm::ES256),
        _ => None,
    }
}

fn config_error(detail: impl Into<String>) -> ApplicationError {
    ApplicationError::new(ApplicationErrorKind::PortFailure, detail)
}

/// The claims this adapter reads.
///
/// Only `iss` and `sub` carry meaning. `email`, `name`, `groups` and `role`
/// are not declared here at all, so there is no path by which an issuer's
/// self-asserted claim could reach identity resolution or authorization.
#[derive(Debug, Deserialize)]
struct ExternalClaims {
    iss: String,
    sub: String,
}

#[async_trait::async_trait]
impl IdentityProvider for JwtIdentityProvider {
    async fn verify(
        &self,
        token: &str,
        _now: Timestamp,
    ) -> Result<ExternalIdentity, ExternalAuthError> {
        let header =
            jsonwebtoken::decode_header(token).map_err(|_| ExternalAuthError::Malformed)?;
        let key_id = header.kid.as_deref().ok_or(ExternalAuthError::UnknownKey)?;
        let snapshot = Arc::clone(&*self.keys.read().await);
        let verification = snapshot
            .keys
            .get(key_id)
            .ok_or(ExternalAuthError::UnknownKey)?;
        // The token's algorithm must equal the one its key was published for.
        // Checking the header before handing the token to the decoder makes an
        // algorithm substitution fail here rather than depending on the
        // decoder's own defaults.
        if header.alg != verification.algorithm || header.alg != self.expected_algorithm {
            return Err(ExternalAuthError::UnacceptedAlgorithm);
        }

        let mut validation = Validation::new(verification.algorithm);
        validation.algorithms = vec![verification.algorithm];
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.leeway = self.leeway_seconds;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.validate_aud = true;
        validation.reject_tokens_expiring_in_less_than = 0;
        validation.required_spec_claims = HashSet::from_iter([
            String::from("iss"),
            String::from("aud"),
            String::from("exp"),
            String::from("sub"),
        ]);

        let claims = jsonwebtoken::decode::<ExternalClaims>(token, &verification.key, &validation)
            .map(|data| data.claims)
            .map_err(map_decode_error)?;

        if claims.sub.trim().is_empty() {
            return Err(ExternalAuthError::MissingClaim);
        }
        // Re-check the issuer against the configured value rather than relying
        // only on the decoder, so the stored mapping key is provably the
        // issuer this server accepts.
        if claims.iss != self.issuer {
            return Err(ExternalAuthError::UnacceptedIssuer);
        }
        Ok(ExternalIdentity {
            issuer: claims.iss,
            subject: claims.sub,
        })
    }
}

fn map_decode_error(error: jsonwebtoken::errors::Error) -> ExternalAuthError {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::InvalidSignature => ExternalAuthError::InvalidSignature,
        ErrorKind::InvalidIssuer => ExternalAuthError::UnacceptedIssuer,
        ErrorKind::InvalidAudience => ExternalAuthError::UnacceptedAudience,
        ErrorKind::ExpiredSignature | ErrorKind::ImmatureSignature => {
            ExternalAuthError::OutsideValidityWindow
        }
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
            ExternalAuthError::UnacceptedAlgorithm
        }
        ErrorKind::MissingRequiredClaim(_) => ExternalAuthError::MissingClaim,
        _ => ExternalAuthError::Malformed,
    }
}

/// Turns a verified external token into a session.
pub struct ExternalAuthService {
    provider: Arc<dyn IdentityProvider>,
    store: Arc<dyn ExternalIdentityStore>,
    issuer: Arc<SessionIssuer>,
    grant_roles: Vec<RoleId>,
}

impl ExternalAuthService {
    /// Creates the service over injected ports.
    #[must_use]
    pub const fn new(
        provider: Arc<dyn IdentityProvider>,
        store: Arc<dyn ExternalIdentityStore>,
        issuer: Arc<SessionIssuer>,
        grant_roles: Vec<RoleId>,
    ) -> Self {
        Self {
            provider,
            store,
            issuer,
            grant_roles,
        }
    }

    /// Verifies `token` and issues a session for the subject it proves.
    ///
    /// A pair seen before resolves to its existing user; a new pair creates a
    /// new user. It never attaches to an existing local account: the only key
    /// is `(issuer, subject)`, so an issuer asserting someone's email address
    /// cannot take over that person's account, and two issuers using the same
    /// subject string stay separate users.
    ///
    /// # Errors
    ///
    /// Returns `Unauthenticated` for every token that fails verification, with
    /// the same message regardless of which check failed.
    pub async fn authenticate(
        &self,
        token: &str,
        request_id: RequestId,
        source_ip: Option<String>,
    ) -> Result<LoginResult, ApplicationError> {
        let now = self.issuer.now();
        let identity = self.provider.verify(token, now).await.map_err(|error| {
            tracing::warn!(
                event = "auth.external_rejected",
                reason = %error,
                "external token rejected"
            );
            ApplicationError::new(
                ApplicationErrorKind::Unauthenticated,
                "authentication failed",
            )
        })?;

        // A concurrent first login can bind this pair after find_user. Its
        // losing transaction rolls back completely, including the candidate
        // user. Re-read once and sign new token material for the committed id.
        for attempt in 0..2 {
            let existing = self
                .store
                .find_user(&identity.issuer, &identity.subject)
                .await?;

            // An existing pair reuses its user id; nothing about the user row is
            // rewritten from the token, so a later change to a self-asserted claim
            // cannot alter the stored subject.
            let (user_id, new_user) = match existing {
                Some(user_id) => (user_id, None),
                None => {
                    let user_id = UserId::generate();
                    let login_id = LoginId::for_generated_subject(AuthMethod::External, user_id)
                        .map_err(|_| {
                            ApplicationError::port_failure("login identifier generation failed")
                        })?;
                    (
                        user_id,
                        Some(NewSubjectUser {
                            login_id,
                            // The display name is derived from the identifier the
                            // issuer authenticated, never from a self-asserted
                            // `name` or `email` claim.
                            display_name: display_name_for(&identity.subject),
                            kind: UserKind::External,
                            created_at: now,
                        }),
                    )
                }
            };

            let result = self
                .issuer
                .issue(
                    IssuableSubject {
                        user_id,
                        new_user,
                        grant_roles: self.grant_roles.clone(),
                        // External subjects are permanent: they keep the existing
                        // session lifetime rules rather than an absolute cap.
                        ephemeral: None,
                        external: Some(ExternalIdentityRecord {
                            issuer: identity.issuer.clone(),
                            subject: identity.subject.clone(),
                            created_at: now,
                        }),
                    },
                    request_id.clone(),
                    source_ip.clone(),
                    "auth.external",
                )
                .await;
            match result {
                Err(error) if attempt == 0 && error.kind() == ApplicationErrorKind::Conflict => {}
                result => return result,
            }
        }
        Err(ApplicationError::port_failure(
            "external identity did not stabilize",
        ))
    }
}

impl core::fmt::Debug for ExternalAuthService {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExternalAuthService")
            .field("grant_roles", &self.grant_roles.len())
            .finish()
    }
}

/// Builds a display name from the verified subject identifier.
fn display_name_for(subject: &str) -> String {
    let trimmed = subject.trim();
    let truncated: String = trimmed.chars().take(128).collect();
    if truncated.is_empty() {
        "External user".to_owned()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::{display_name_for, parse_algorithm};
    use jsonwebtoken::Algorithm;

    #[test]
    fn unsigned_and_symmetric_algorithms_are_not_accepted() {
        // `none` would accept an unsigned token; an HMAC algorithm would let
        // anyone holding the published verification key mint tokens.
        for rejected in ["none", "None", "NONE", "HS256", "HS384", "HS512", ""] {
            assert!(
                parse_algorithm(rejected).is_none(),
                "{rejected} must not be accepted"
            );
        }
    }

    #[test]
    fn accepted_algorithms_are_asymmetric() {
        assert_eq!(parse_algorithm("EdDSA"), Some(Algorithm::EdDSA));
        assert_eq!(parse_algorithm("RS256"), Some(Algorithm::RS256));
        assert_eq!(parse_algorithm("ES256"), Some(Algorithm::ES256));
    }

    #[test]
    fn display_names_are_bounded_and_never_empty() {
        assert_eq!(display_name_for("  user-1  "), "user-1");
        assert_eq!(display_name_for("   "), "External user");
        assert_eq!(display_name_for(&"x".repeat(300)).chars().count(), 128);
    }
}
