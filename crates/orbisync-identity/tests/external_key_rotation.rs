//! Local key rotation keeps overlap explicit and refuses bad replacement sets.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use orbisync_application::{
    ExternalKeyAuditStore, ExternalKeyReloadAudit, IdentityPortError, IdentityProvider, RequestId,
};
use orbisync_domain::{Timestamp, UserId};
use orbisync_identity::JwtIdentityProvider;
use orbisync_testkit::external_idp::{LocalSigningIssuer, TestClaims};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
#[derive(Default)]
struct Audit {
    events: Mutex<Vec<ExternalKeyReloadAudit>>,
    fail: AtomicBool,
}
#[async_trait::async_trait]
impl ExternalKeyAuditStore for Audit {
    async fn record_reload(&self, e: ExternalKeyReloadAudit) -> Result<(), IdentityPortError> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(IdentityPortError::Unavailable);
        }
        self.events.lock().unwrap().push(e);
        Ok(())
    }
}
fn claims() -> TestClaims {
    let seconds = time_now().as_offset_date_time().unix_timestamp();
    TestClaims {
        iss: "https://idp.test".into(),
        sub: "subject".into(),
        aud: "engine".into(),
        exp: seconds + 3600,
        nbf: seconds - 60,
        iat: seconds - 60,
        email: None,
        role: None,
    }
}
fn time_now() -> Timestamp {
    orbisync_domain::Clock::now(&orbisync_domain::SystemClock)
}
fn request() -> RequestId {
    RequestId::new(format!("req_{}", uuid::Uuid::now_v7())).unwrap()
}
fn load(path: &std::path::Path) -> JwtIdentityProvider {
    JwtIdentityProvider::from_jwks_file(
        path,
        "https://idp.test".into(),
        "engine".into(),
        "EdDSA",
        0,
    )
    .unwrap()
}
#[tokio::test]
async fn overlap_then_removal_changes_new_authentication_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    let old = LocalSigningIssuer::new("old", [1; 32]);
    let new = LocalSigningIssuer::new("new", [2; 32]);
    old.write_jwks(&path);
    let provider = load(&path);
    let audit = Audit::default();
    let mut set: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    new.write_jwks(&path);
    let new_set: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    set["keys"]
        .as_array_mut()
        .unwrap()
        .extend(new_set["keys"].as_array().unwrap().iter().cloned());
    std::fs::write(&path, serde_json::to_vec(&set).unwrap()).unwrap();
    let summary = provider
        .reload_keys(&audit, UserId::generate(), request(), time_now())
        .await
        .unwrap();
    assert_eq!(summary.generation, 2);
    assert_eq!(summary.key_ids, vec!["new", "old"]);
    provider
        .verify(&old.sign(&claims()), time_now())
        .await
        .unwrap();
    provider
        .verify(&new.sign(&claims()), time_now())
        .await
        .unwrap();
    new.write_jwks(&path);
    provider
        .reload_keys(&audit, UserId::generate(), request(), time_now())
        .await
        .unwrap();
    assert_eq!(
        provider
            .verify(&old.sign(&claims()), time_now())
            .await
            .unwrap_err(),
        orbisync_application::ExternalAuthError::UnknownKey
    );
    provider
        .verify(&new.sign(&claims()), time_now())
        .await
        .unwrap();
    let mut wrong = claims();
    wrong.iss = "https://other.test".into();
    assert!(
        provider
            .verify(&new.sign(&wrong), time_now())
            .await
            .is_err()
    );
    let events = audit.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.succeeded));
}
#[tokio::test]
async fn bad_keyset_and_unavailable_audit_retain_the_working_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    let old = LocalSigningIssuer::new("old", [3; 32]);
    let new = LocalSigningIssuer::new("new", [4; 32]);
    old.write_jwks(&path);
    let provider = load(&path);
    let audit = Audit::default();
    for bad in [
        r#"{"keys":[]}"#,
        r#"{"keys":[{"kid":"bad","alg":"HS256","kty":"oct","x":"bad"}]}"#,
        "not JSON",
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            provider
                .reload_keys(&audit, UserId::generate(), request(), time_now())
                .await
                .is_err()
        );
        provider
            .verify(&old.sign(&claims()), time_now())
            .await
            .unwrap();
    }
    new.write_jwks(&path);
    audit.fail.store(true, Ordering::Relaxed);
    assert!(
        provider
            .reload_keys(&audit, UserId::generate(), request(), time_now())
            .await
            .is_err()
    );
    provider
        .verify(&old.sign(&claims()), time_now())
        .await
        .unwrap();
    assert!(
        provider
            .verify(&new.sign(&claims()), time_now())
            .await
            .is_err()
    );
    audit.fail.store(false, Ordering::Relaxed);
    let summary = provider
        .reload_keys(&audit, UserId::generate(), request(), time_now())
        .await
        .unwrap();
    assert_eq!(summary.generation, 2);
    assert_eq!(
        audit
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| !e.succeeded)
            .count(),
        3
    );
}
#[tokio::test]
async fn duplicate_ids_oversized_files_and_algorithm_changes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    let old = LocalSigningIssuer::new("same", [5; 32]);
    old.write_jwks(&path);
    let provider = load(&path);
    let audit = Audit::default();
    let mut set: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let duplicate = set["keys"][0].clone();
    set["keys"].as_array_mut().unwrap().push(duplicate);
    std::fs::write(&path, serde_json::to_vec(&set).unwrap()).unwrap();
    assert!(
        provider
            .reload_keys(&audit, UserId::generate(), request(), time_now())
            .await
            .is_err()
    );
    std::fs::write(&path, vec![b' '; 262_145]).unwrap();
    assert!(
        provider
            .reload_keys(&audit, UserId::generate(), request(), time_now())
            .await
            .is_err()
    );
    provider
        .verify(&old.sign(&claims()), time_now())
        .await
        .unwrap();
}

enum AwsSigningKey {
    Ec(aws_lc_rs::signature::EcdsaKeyPair),
    Rsa(aws_lc_rs::rsa::KeyPair),
}

impl AwsSigningKey {
    fn algorithm(&self) -> &'static str {
        match self {
            Self::Ec(_) => "ES256",
            Self::Rsa(_) => "RS256",
        }
    }

    fn jwk(&self) -> serde_json::Value {
        use aws_lc_rs::signature::KeyPair as _;
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        match self {
            Self::Ec(key) => {
                let point = key.public_key().as_ref();
                assert_eq!(point.len(), 65);
                assert_eq!(point[0], 4);
                serde_json::json!({"kid":"aws-valid","alg":"ES256","kty":"EC","crv":"P-256","x":encode(&point[1..33]),"y":encode(&point[33..65])})
            }
            Self::Rsa(key) => {
                let components: aws_lc_rs::rsa::PublicKeyComponents<Vec<u8>> =
                    key.public_key().into();
                serde_json::json!({"kid":"aws-valid","alg":"RS256","kty":"RSA","n":encode(&components.n),"e":encode(&components.e)})
            }
        }
    }

    fn sign(&self, claims: &TestClaims) -> String {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = serde_json::json!({"alg":self.algorithm(),"kid":"aws-valid","typ":"JWT"});
        let signing_input = format!(
            "{}.{}",
            encode(header.to_string().as_bytes()),
            encode(&serde_json::to_vec(claims).unwrap())
        );
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let signature = match self {
            Self::Ec(key) => key
                .sign(&rng, signing_input.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            Self::Rsa(key) => {
                let mut signature = vec![0; key.public_modulus_len()];
                key.sign(
                    &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                    &rng,
                    signing_input.as_bytes(),
                    &mut signature,
                )
                .unwrap();
                signature
            }
        };
        format!("{signing_input}.{}", encode(&signature))
    }
}

#[tokio::test]
async fn aws_generated_p256_and_rsa_keys_verify_and_reject_invalid_replacement_material() {
    use base64::Engine as _;
    let keys = [
        AwsSigningKey::Ec(
            aws_lc_rs::signature::EcdsaKeyPair::generate(
                &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            )
            .unwrap(),
        ),
        AwsSigningKey::Rsa(
            aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap(),
        ),
    ];
    for key in keys {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        let valid = key.jwk();
        let write_key = |jwk: &serde_json::Value| {
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"keys":[jwk]})).unwrap(),
            )
            .unwrap()
        };
        write_key(&valid);
        let provider = JwtIdentityProvider::from_jwks_file(
            &path,
            "https://idp.test".into(),
            "engine".into(),
            key.algorithm(),
            0,
        )
        .unwrap();
        let jwt = key.sign(&claims());
        provider
            .verify(&jwt, time_now())
            .await
            .expect("AWS-LC generated signatures must verify through production backend");
        let audit = Audit::default();
        assert_eq!(
            provider
                .reload_keys(&audit, UserId::generate(), request(), time_now())
                .await
                .unwrap()
                .generation,
            2
        );
        provider.verify(&jwt, time_now()).await.unwrap();
        let mut wrong = claims();
        wrong.aud = "other-audience".into();
        assert!(
            provider
                .verify(&key.sign(&wrong), time_now())
                .await
                .is_err()
        );
        let invalid = if key.algorithm() == "ES256" {
            let mut off_curve = valid.clone();
            off_curve["x"] = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode([0; 32])
                .into();
            off_curve["y"] = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode([0; 32])
                .into();
            let mut short = valid.clone();
            short["x"] = "AA".into();
            let mut wrong_curve = valid.clone();
            wrong_curve["crv"] = "P-384".into();
            vec![off_curve, short, wrong_curve]
        } else {
            let mut even_modulus = valid.clone();
            let mut n = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(valid["n"].as_str().unwrap())
                .unwrap();
            *n.last_mut().unwrap() &= 0xfe;
            even_modulus["n"] = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(n)
                .into();
            let mut unit_exponent = valid.clone();
            unit_exponent["e"] = "AQ".into();
            let mut undersized = valid.clone();
            undersized["n"] = "Aw".into();
            vec![even_modulus, unit_exponent, undersized]
        };
        for candidate in invalid {
            write_key(&candidate);
            assert!(
                JwtIdentityProvider::from_jwks_file(
                    &path,
                    "https://idp.test".into(),
                    "engine".into(),
                    key.algorithm(),
                    0
                )
                .is_err(),
                "invalid {} key must fail at startup",
                key.algorithm()
            );
            assert!(
                provider
                    .reload_keys(&audit, UserId::generate(), request(), time_now())
                    .await
                    .is_err(),
                "invalid {} material must not be published",
                key.algorithm()
            );
            provider
                .verify(&jwt, time_now())
                .await
                .expect("failed reload must retain the working generation");
        }
        write_key(&valid);
        assert_eq!(
            provider
                .reload_keys(&audit, UserId::generate(), request(), time_now())
                .await
                .unwrap()
                .generation,
            3
        );
        let events = audit.events.lock().unwrap();
        assert_eq!(events.iter().filter(|e| !e.succeeded).count(), 3);
    }
}
