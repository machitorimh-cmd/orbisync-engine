//! A local signing issuer, for exercising the external authentication path.
//!
//! This is a *signing* helper only. It mints tokens and publishes the matching
//! JWKS file; verification is done by the production adapter, unchanged. That
//! keeps the tested code path identical to the deployed one -- a test-only
//! verifier would prove nothing about the checks that matter.

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use serde::Serialize;

/// Claims a test token carries.
#[derive(Debug, Clone, Serialize)]
pub struct TestClaims {
    /// Issuer.
    pub iss: String,
    /// Subject.
    pub sub: String,
    /// Audience.
    pub aud: String,
    /// Expiry, seconds since the epoch.
    pub exp: i64,
    /// Not-before, seconds since the epoch.
    pub nbf: i64,
    /// Issued-at, seconds since the epoch.
    pub iat: i64,
    /// A self-asserted address, included so tests can prove it is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// A self-asserted role, included so tests can prove it is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// An Ed25519 signing issuer that publishes a JWKS file.
pub struct LocalSigningIssuer {
    key_id: String,
    signing_key: SigningKey,
}

impl LocalSigningIssuer {
    /// Creates an issuer with a deterministic key for the given seed.
    #[must_use]
    pub fn new(key_id: &str, seed: [u8; 32]) -> Self {
        Self {
            key_id: key_id.to_owned(),
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    /// Returns the key id this issuer signs with.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Returns the JWKS document publishing this issuer's public key.
    #[must_use]
    pub fn jwks(&self) -> String {
        let public = self.signing_key.verifying_key();
        let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.to_bytes());
        serde_json::json!({
            "keys": [{
                "kid": self.key_id,
                "kty": "OKP",
                "crv": "Ed25519",
                "alg": "EdDSA",
                "x": x,
            }]
        })
        .to_string()
    }

    /// Writes the JWKS document to `path`.
    ///
    /// # Panics
    ///
    /// Panics when the file cannot be written; a test that cannot publish its
    /// keys has nothing meaningful left to assert.
    pub fn write_jwks(&self, path: &std::path::Path) {
        std::fs::write(path, self.jwks())
            .unwrap_or_else(|error| panic!("failed to write JWKS to {}: {error}", path.display()));
    }

    /// Signs `claims` into a compact JWT.
    #[must_use]
    pub fn sign(&self, claims: &TestClaims) -> String {
        self.sign_with_header(claims, "EdDSA", Some(&self.key_id))
    }

    /// Signs with an explicit header, for tests that need a wrong `alg` or a
    /// missing or unknown `kid`.
    #[must_use]
    pub fn sign_with_header(
        &self,
        claims: &TestClaims,
        algorithm: &str,
        key_id: Option<&str>,
    ) -> String {
        let header = match key_id {
            Some(kid) => serde_json::json!({ "alg": algorithm, "typ": "JWT", "kid": kid }),
            None => serde_json::json!({ "alg": algorithm, "typ": "JWT" }),
        };
        let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
        let header_part = encode(header.to_string().as_bytes());
        let claims_part = encode(
            serde_json::to_string(claims)
                .unwrap_or_else(|error| panic!("claims must serialize: {error}"))
                .as_bytes(),
        );
        let signing_input = format!("{header_part}.{claims_part}");
        let signature = self.signing_key.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", encode(&signature.to_bytes()))
    }

    /// Returns a token whose payload has been altered after signing.
    ///
    /// The signature is left intact, so verification must reject it.
    #[must_use]
    pub fn sign_then_tamper(&self, claims: &TestClaims, tampered_subject: &str) -> String {
        let token = self.sign(claims);
        let mut parts = token.split('.');
        let header = parts.next().unwrap_or_default().to_owned();
        let _original = parts.next();
        let signature = parts.next().unwrap_or_default().to_owned();
        let mut altered = claims.clone();
        altered.sub = tampered_subject.to_owned();
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_string(&altered)
                .unwrap_or_else(|error| panic!("claims must serialize: {error}"))
                .as_bytes(),
        );
        format!("{header}.{payload}.{signature}")
    }
}

impl core::fmt::Debug for LocalSigningIssuer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalSigningIssuer")
            .field("key_id", &self.key_id)
            .field("signing_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalSigningIssuer, TestClaims};

    fn claims() -> TestClaims {
        TestClaims {
            iss: "https://idp.test".to_owned(),
            sub: "subject-1".to_owned(),
            aud: "orbisync".to_owned(),
            exp: 4_000_000_000,
            nbf: 0,
            iat: 0,
            email: None,
            role: None,
        }
    }

    #[test]
    fn signed_tokens_have_three_parts_and_publish_a_matching_key() {
        let issuer = LocalSigningIssuer::new("key-1", [7_u8; 32]);
        let token = issuer.sign(&claims());
        assert_eq!(token.split('.').count(), 3);
        let jwks: serde_json::Value =
            serde_json::from_str(&issuer.jwks()).expect("jwks is valid json");
        assert_eq!(jwks["keys"][0]["kid"], "key-1");
        assert_eq!(jwks["keys"][0]["alg"], "EdDSA");
    }

    #[test]
    fn tampering_keeps_the_original_signature() {
        let issuer = LocalSigningIssuer::new("key-1", [7_u8; 32]);
        let original = issuer.sign(&claims());
        let tampered = issuer.sign_then_tamper(&claims(), "someone-else");
        assert_ne!(original, tampered);
        // Same signature segment, different payload: exactly the forgery a
        // verifier has to catch.
        assert_eq!(
            original.rsplit('.').next(),
            tampered.rsplit('.').next(),
            "tampering must reuse the signature"
        );
    }
}
