//! Refresh-token rotation family and reuse detection.

use core::fmt;

use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use orbisync_application::SecretString;
use orbisync_domain::{AuthSessionId, RefreshTokenFamilyId, Timestamp};
use rand::Rng as _;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Refresh-token operation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    /// Secret key length or CSPRNG setup failed.
    #[error("refresh token service is unavailable")]
    Unavailable,
    /// Token is unknown, expired, or belongs to a revoked family.
    #[error("refresh token is invalid")]
    InvalidToken,
}

/// Result of presenting a refresh token.
#[derive(Clone, PartialEq, Eq)]
pub enum RotationOutcome {
    /// Active token was consumed and replaced.
    Rotated(SecretString),
    /// A consumed token was reused and the family was revoked.
    ReuseDetected,
}

impl fmt::Debug for RotationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rotated(_) => f.write_str("Rotated([REDACTED])"),
            Self::ReuseDetected => f.write_str("ReuseDetected"),
        }
    }
}

/// In-memory domain state for one refresh-token rotation family.
#[derive(Clone)]
pub struct RefreshTokenFamily {
    id: RefreshTokenFamilyId,
    session_id: AuthSessionId,
    hmac_key: Vec<u8>,
    active_digest: [u8; 32],
    consumed_digests: Vec<[u8; 32]>,
    expires_at: Timestamp,
    revoked: bool,
}

impl fmt::Debug for RefreshTokenFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshTokenFamily")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("hmac_key", &"[REDACTED]")
            .field("active_digest", &"[REDACTED]")
            .field("consumed_count", &self.consumed_digests.len())
            .field("expires_at", &self.expires_at)
            .field("revoked", &self.revoked)
            .finish()
    }
}

impl RefreshTokenFamily {
    /// Issues the first opaque 256-bit token.
    ///
    /// # Errors
    ///
    /// Returns an unavailable error if HMAC initialization fails.
    pub fn issue(
        session_id: AuthSessionId,
        hmac_key: Vec<u8>,
        expires_at: Timestamp,
    ) -> Result<(Self, SecretString), RefreshError> {
        let token = generate_token();
        let digest = digest(&hmac_key, &token)?;
        Ok((
            Self {
                id: RefreshTokenFamilyId::generate(),
                session_id,
                hmac_key,
                active_digest: digest,
                consumed_digests: Vec::new(),
                expires_at,
                revoked: false,
            },
            token,
        ))
    }

    /// Returns the family identifier.
    #[must_use]
    pub const fn id(&self) -> RefreshTokenFamilyId {
        self.id
    }

    /// Returns the bound identity session.
    #[must_use]
    pub const fn session_id(&self) -> AuthSessionId {
        self.session_id
    }

    /// Returns whether reuse or logout has revoked the family.
    #[must_use]
    pub const fn is_revoked(&self) -> bool {
        self.revoked
    }

    /// Atomically models consume-and-replace or consumed-token reuse.
    ///
    /// # Errors
    ///
    /// Returns invalid-token for unknown, expired or already revoked values.
    pub fn rotate(
        &mut self,
        presented: &SecretString,
        now: Timestamp,
    ) -> Result<RotationOutcome, RefreshError> {
        if self.revoked || now >= self.expires_at {
            return Err(RefreshError::InvalidToken);
        }
        let presented_digest = digest(&self.hmac_key, presented)?;
        if constant_time_equal(&presented_digest, &self.active_digest) {
            let old = self.active_digest;
            let replacement = generate_token();
            self.active_digest = digest(&self.hmac_key, &replacement)?;
            self.consumed_digests.push(old);
            return Ok(RotationOutcome::Rotated(replacement));
        }
        if self
            .consumed_digests
            .iter()
            .any(|candidate| constant_time_equal(&presented_digest, candidate))
        {
            self.revoked = true;
            return Ok(RotationOutcome::ReuseDetected);
        }
        Err(RefreshError::InvalidToken)
    }

    /// Revokes this family for logout or user disable.
    pub fn revoke(&mut self) {
        self.revoked = true;
    }
}

fn generate_token() -> SecretString {
    let mut bytes = [0_u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    SecretString::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(key: &[u8], token: &SecretString) -> Result<[u8; 32], RefreshError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| RefreshError::Unavailable)?;
    mac.update(token.expose_secret().as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    use subtle::ConstantTimeEq as _;
    bool::from(left.ct_eq(right))
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::{RefreshTokenFamily, RotationOutcome, digest};
    use orbisync_application::SecretString;
    use orbisync_domain::{AuthSessionId, Timestamp};

    fn hex_decode(s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len() / 2);
        for i in (0..s.len()).step_by(2) {
            let byte = u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex");
            out.push(byte);
        }
        out
    }

    const GOLDEN_VECTORS: &[(&str, &str, &str)] = &[
        (
            "test-only-refresh-golden-v1-TEST-NOT-PROD-32B!!",
            "fixed-refresh-token-for-golden-vector-001-TEST",
            "f622bf05d4dc44ed788f493c20ffa3c14bb3a8f4109a644a2472d74df73341d7",
        ),
        (
            "key",
            "The quick brown fox",
            "203d1e5cedd2d18f8c5a3beff0bd9c1ebcb97097dfcb288c46b00c9227fde2c0",
        ),
    ];

    #[test]
    fn rotation_and_reuse_revoke_the_family() {
        let now = Timestamp::from_unix_millis(1_000).expect("valid");
        let expiry = Timestamp::from_unix_millis(10_000).expect("valid");
        let (mut family, first) =
            RefreshTokenFamily::issue(AuthSessionId::generate(), vec![7; 32], expiry)
                .expect("issue");
        let second = match family.rotate(&first, now).expect("rotate") {
            RotationOutcome::Rotated(value) => value,
            RotationOutcome::ReuseDetected => panic!("fresh token cannot be reuse"),
        };
        assert!(matches!(
            family.rotate(&first, now).expect("reuse is detected"),
            RotationOutcome::ReuseDetected
        ));
        assert!(family.is_revoked());
        assert!(family.rotate(&second, now).is_err());
    }

    #[test]
    fn token_and_family_debug_are_redacted() {
        let expiry = Timestamp::from_unix_millis(10_000).expect("valid");
        let (family, token) =
            RefreshTokenFamily::issue(AuthSessionId::generate(), vec![8; 32], expiry)
                .expect("issue");
        assert!(!format!("{family:?}").contains(token.expose_secret()));
        assert_eq!(token.to_string(), "[REDACTED]");
    }

    #[test]
    fn golden_refresh_digest_matches_vector() {
        for &(key, token, expected_hex) in GOLDEN_VECTORS {
            let digest =
                digest(key.as_bytes(), &SecretString::new(token)).expect("digest must succeed");
            let expected = hex_decode(expected_hex);
            assert_eq!(expected.len(), 32);
            let mut exp_arr = [0u8; 32];
            exp_arr.copy_from_slice(&expected);
            assert_eq!(
                digest, exp_arr,
                "identity refresh digest golden must match for key=[REDACTED] (M4)"
            );
        }
    }

    #[test]
    fn golden_refresh_digest_is_not_plain_sha256() {
        let &(_key, token, expected_hex) = &GOLDEN_VECTORS[0];
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let plain = hasher.finalize();
        let plain_hex: String = plain.iter().map(|b| format!("{b:02x}")).collect();
        assert_ne!(
            plain_hex, expected_hex,
            "golden digest must differ from plain SHA-256 (M4)"
        );
    }
}
