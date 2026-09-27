//! Verification tests for the external identity adapter (ADR-026 §8).
//!
//! Every token here is signed by a local issuer and checked by the production
//! adapter, so these exercise the real verification rules rather than a
//! test-only stand-in.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use orbisync_application::{ExternalAuthError, IdentityProvider};
use orbisync_domain::Timestamp;
use orbisync_identity::JwtIdentityProvider;
use orbisync_testkit::external_idp::{LocalSigningIssuer, TestClaims};

const ISSUER: &str = "https://idp.test";
const AUDIENCE: &str = "orbisync";

fn now() -> Timestamp {
    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid timestamp")
}

fn claims() -> TestClaims {
    TestClaims {
        iss: ISSUER.to_owned(),
        sub: "subject-1".to_owned(),
        aud: AUDIENCE.to_owned(),
        // Comfortably after `now()`.
        exp: 1_800_000_000,
        nbf: 1_600_000_000,
        iat: 1_600_000_000,
        email: None,
        role: None,
    }
}

/// Builds a provider trusting `issuer`'s published key.
fn provider_for(issuer: &LocalSigningIssuer) -> (JwtIdentityProvider, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("jwks.json");
    issuer.write_jwks(&path);
    let provider = JwtIdentityProvider::from_jwks_file(
        &path,
        ISSUER.to_owned(),
        AUDIENCE.to_owned(),
        "EdDSA",
        60,
    )
    .expect("provider loads");
    (provider, dir)
}

fn verify(
    provider: &JwtIdentityProvider,
    token: &str,
) -> Result<orbisync_application::ExternalIdentity, ExternalAuthError> {
    pollster::block_on(provider.verify(token, now()))
}

#[test]
fn a_valid_token_resolves_to_its_issuer_and_subject() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let identity = verify(&provider, &issuer.sign(&claims())).expect("valid token");
    assert_eq!(identity.issuer, ISSUER);
    assert_eq!(identity.subject, "subject-1");
}

#[test]
fn a_tampered_payload_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let forged = issuer.sign_then_tamper(&claims(), "someone-else");
    assert_eq!(
        verify(&provider, &forged).expect_err("tampered token must fail"),
        ExternalAuthError::InvalidSignature
    );
}

#[test]
fn a_token_signed_by_another_key_is_rejected() {
    let trusted = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&trusted);
    // Same key id, different private key: the signature must not verify.
    let impostor = LocalSigningIssuer::new("key-1", [2_u8; 32]);
    assert_eq!(
        verify(&provider, &impostor.sign(&claims())).expect_err("foreign signature must fail"),
        ExternalAuthError::InvalidSignature
    );
}

#[test]
fn an_unknown_key_id_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let token = issuer.sign_with_header(&claims(), "EdDSA", Some("key-rotated-away"));
    assert_eq!(
        verify(&provider, &token).expect_err("unknown kid must fail"),
        ExternalAuthError::UnknownKey
    );
    let headerless = issuer.sign_with_header(&claims(), "EdDSA", None);
    assert_eq!(
        verify(&provider, &headerless).expect_err("missing kid must fail"),
        ExternalAuthError::UnknownKey
    );
}

#[test]
fn the_none_algorithm_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    for algorithm in ["none", "None", "HS256"] {
        let token = issuer.sign_with_header(&claims(), algorithm, Some("key-1"));
        let error = verify(&provider, &token).expect_err("unsigned or symmetric alg must fail");
        assert!(
            matches!(
                error,
                ExternalAuthError::UnacceptedAlgorithm | ExternalAuthError::Malformed
            ),
            "{algorithm} produced {error:?}"
        );
    }
}

#[test]
fn a_wrong_issuer_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let mut wrong = claims();
    wrong.iss = "https://evil.test".to_owned();
    assert_eq!(
        verify(&provider, &issuer.sign(&wrong)).expect_err("wrong issuer must fail"),
        ExternalAuthError::UnacceptedIssuer
    );
}

#[test]
fn a_wrong_audience_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let mut wrong = claims();
    wrong.aud = "another-service".to_owned();
    assert_eq!(
        verify(&provider, &issuer.sign(&wrong)).expect_err("wrong audience must fail"),
        ExternalAuthError::UnacceptedAudience
    );
}

#[test]
fn an_expired_or_not_yet_valid_token_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);

    let mut expired = claims();
    expired.exp = 1_600_000_100;
    assert_eq!(
        verify(&provider, &issuer.sign(&expired)).expect_err("expired token must fail"),
        ExternalAuthError::OutsideValidityWindow
    );

    let mut future = claims();
    future.nbf = 1_900_000_000;
    future.exp = 1_950_000_000;
    assert_eq!(
        verify(&provider, &issuer.sign(&future)).expect_err("premature token must fail"),
        ExternalAuthError::OutsideValidityWindow
    );
}

#[test]
fn an_empty_subject_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let mut blank = claims();
    blank.sub = "   ".to_owned();
    assert_eq!(
        verify(&provider, &issuer.sign(&blank)).expect_err("blank subject must fail"),
        ExternalAuthError::MissingClaim
    );
}

#[test]
fn self_asserted_email_and_role_claims_do_not_reach_the_identity() {
    // The adapter must resolve identity from `sub` alone. An issuer that
    // asserts a local user's address or an administrative role must gain
    // nothing from it.
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    let mut boastful = claims();
    boastful.email = Some("admin@example.org".to_owned());
    boastful.role = Some("Administrator".to_owned());
    let identity = verify(&provider, &issuer.sign(&boastful)).expect("valid token");
    assert_eq!(identity.subject, "subject-1");
    assert_eq!(identity.issuer, ISSUER);
    // The identity type carries no room for either claim.
    let rendered = format!("{identity:?}");
    assert!(!rendered.contains("admin@example.org"));
    assert!(!rendered.contains("Administrator"));
}

#[test]
fn a_malformed_token_is_rejected() {
    let issuer = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&issuer);
    for malformed in ["", "not-a-token", "a.b", "a.b.c.d"] {
        verify(&provider, malformed).expect_err("malformed token must fail");
    }
}

#[test]
fn an_unreadable_key_file_fails_startup_rather_than_accepting_tokens() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("absent.json");
    JwtIdentityProvider::from_jwks_file(
        &missing,
        ISSUER.to_owned(),
        AUDIENCE.to_owned(),
        "EdDSA",
        60,
    )
    .expect_err("a missing key file must fail closed");

    let empty = dir.path().join("empty.json");
    std::fs::write(&empty, r#"{"keys":[]}"#).expect("write");
    JwtIdentityProvider::from_jwks_file(
        &empty,
        ISSUER.to_owned(),
        AUDIENCE.to_owned(),
        "EdDSA",
        60,
    )
    .expect_err("a key file with no keys must fail closed");
}

#[test]
fn a_key_published_for_another_algorithm_is_refused_at_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("jwks.json");
    // An Ed25519 key advertised as RS256: loading it would mean verifying
    // against a key its issuer published for a different algorithm.
    std::fs::write(
        &path,
        r#"{"keys":[{"kid":"key-1","kty":"OKP","crv":"Ed25519","alg":"RS256","x":"AAAA"}]}"#,
    )
    .expect("write");
    JwtIdentityProvider::from_jwks_file(&path, ISSUER.to_owned(), AUDIENCE.to_owned(), "EdDSA", 60)
        .expect_err("algorithm mismatch must fail closed");
}

#[test]
fn the_same_subject_from_a_different_issuer_is_a_different_identity() {
    // Two issuers may legitimately use the same subject string. The pair, not
    // the subject alone, is what identifies a user.
    let first = LocalSigningIssuer::new("key-1", [1_u8; 32]);
    let (provider, _dir) = provider_for(&first);
    let identity = verify(&provider, &first.sign(&claims())).expect("valid token");

    let other_issuer_url = "https://other-idp.test";
    let second = LocalSigningIssuer::new("key-1", [9_u8; 32]);
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("jwks.json");
    second.write_jwks(&path);
    let other_provider = JwtIdentityProvider::from_jwks_file(
        &path,
        other_issuer_url.to_owned(),
        AUDIENCE.to_owned(),
        "EdDSA",
        60,
    )
    .expect("provider loads");
    let mut other_claims = claims();
    other_claims.iss = other_issuer_url.to_owned();
    let other_identity = verify(&other_provider, &second.sign(&other_claims)).expect("valid token");

    assert_eq!(identity.subject, other_identity.subject);
    assert_ne!(
        identity.issuer, other_identity.issuer,
        "the issuer must distinguish the two identities"
    );
}
