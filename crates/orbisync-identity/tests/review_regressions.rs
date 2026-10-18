//! Authentication regression cases found during the October review.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use orbisync_application::{
    ApplicationErrorKind, IdentityAdministrationStore, IdentityAuditEvent, IdentityMutation,
    IdentityPortError, IdentityRepository, LoginAccount, LoginAuditEvent, LoginCommit,
    LoginTransactionStore, RequestId, SecretString, SubjectCommit,
};
use orbisync_domain::{Credential, LoginId, Timestamp, User, UserId};
use orbisync_identity::{LoginService, PasswordPolicy, PasswordService, token::AccessTokenService};
use orbisync_testkit::{FakeIdentityStore, FixedClock};
use std::sync::Arc;

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";

fn tokens() -> Arc<AccessTokenService> {
    Arc::new(
        AccessTokenService::from_ed25519_private_pem(PRIVATE_PEM, "orbisync", "api", "test-key")
            .expect("tokens"),
    )
}

fn request_id() -> RequestId {
    RequestId::new(format!("req_{}", UserId::generate())).expect("request id")
}

/// Runs a completed administrative operation after the password was verified
/// and before the login commit. No timing or scheduler assumptions are needed.
struct InterleavedStore {
    inner: Arc<FakeIdentityStore>,
    mutation: Option<IdentityMutation>,
    failure: bool,
}

struct ExternalLookup(Arc<FakeIdentityStore>);

#[async_trait::async_trait]
impl orbisync_application::ExternalIdentityStore for ExternalLookup {
    async fn find_user(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<UserId>, orbisync_application::ApplicationError> {
        Ok(self
            .0
            .issued_subjects()
            .into_iter()
            .find(|entry| {
                entry
                    .issuer_subject
                    .as_ref()
                    .is_some_and(|(stored_issuer, stored_subject)| {
                        stored_issuer == issuer && stored_subject == subject
                    })
            })
            .map(|entry| entry.user.id()))
    }
}

#[async_trait::async_trait]
impl LoginTransactionStore for InterleavedStore {
    async fn commit_success(&self, commit: LoginCommit<'_>) -> Result<(), IdentityPortError> {
        if let Some(mutation) = &self.mutation {
            self.inner
                .apply(
                    mutation.clone(),
                    IdentityAuditEvent {
                        occurred_at: commit.audit.occurred_at,
                        actor_id: Some(commit.user_id),
                        action: "test.concurrent_administration",
                        resource_id: Some(commit.user_id.to_string()),
                        request_id: request_id(),
                        succeeded: true,
                    },
                )
                .await?;
        }
        if self.failure {
            self.inner
                .record_login_failure(commit.user_id, commit.audit.occurred_at)
                .await?;
        }
        self.inner.commit_success(commit).await
    }

    async fn record_failure(
        &self,
        user: Option<UserId>,
        audit: LoginAuditEvent,
    ) -> Result<(), IdentityPortError> {
        self.inner.record_failure(user, audit).await
    }

    async fn commit_subject_success(
        &self,
        commit: SubjectCommit<'_>,
    ) -> Result<(), IdentityPortError> {
        self.inner.commit_subject_success(commit).await
    }
}

#[tokio::test]
async fn completed_reset_or_disable_rejects_in_flight_login_without_restoring_old_password() {
    for reset in [true, false] {
        let now = Timestamp::from_unix_millis(1_700_000_000_000).expect("time");
        let store = Arc::new(FakeIdentityStore::new());
        let old = PasswordService::new_with_argon2(PasswordPolicy::new([]), 8_192, 1, 1)
            .expect("old policy");
        let current = PasswordService::new_with_argon2(PasswordPolicy::new([]), 8_192, 2, 1)
            .expect("new policy");
        let password = SecretString::new("Cedar!Lake7-Comet");
        let mut user = User::new(
            UserId::generate(),
            LoginId::new("alice").expect("login"),
            "Alice",
            now,
        )
        .expect("user");
        let old_hash = old.hash(password.clone()).await.expect("hash");
        store.insert_account(LoginAccount {
            user: user.clone(),
            credential: Credential::new(user.id(), old_hash, now),
        });
        let replacement = current
            .hash(SecretString::new("Maple!Lake8-Comet"))
            .await
            .expect("replacement");
        let mutation = if reset {
            user.require_password_change(now).expect("reset flag");
            IdentityMutation::ResetPassword {
                user: user.clone(),
                credential: Credential::new(user.id(), replacement.clone(), now),
            }
        } else {
            user.disable(now).expect("disable");
            IdentityMutation::StoreUserStatus { user: user.clone() }
        };
        let service = LoginService::new(
            store.clone(),
            Arc::new(InterleavedStore {
                inner: store.clone(),
                mutation: Some(mutation),
                failure: false,
            }),
            current,
            tokens(),
            Arc::new(FixedClock::new(now)),
            vec![7; 32],
            900,
            2_592_000,
        );
        let error = service
            .login(
                LoginId::new("alice").expect("login"),
                password,
                request_id(),
                None,
            )
            .await
            .expect_err("stale login rejected");
        assert_eq!(error.kind(), ApplicationErrorKind::Unauthenticated);
        assert!(store.sessions_snapshot().is_empty());
        assert!(store.login_refresh_tokens().is_empty());
        assert_eq!(store.login_audits().len(), 1);
        assert!(!store.login_audits()[0].succeeded);
        assert!(store.login_audits()[0].actor_id.is_none());
        if reset {
            assert_eq!(
                store
                    .find_account(user.id())
                    .await
                    .expect("lookup")
                    .expect("account")
                    .credential
                    .password_hash(),
                &replacement,
                "the stale rehash must not overwrite the new password"
            );
        }
    }
}

#[tokio::test]
async fn a_concurrent_failure_count_change_still_preserves_successful_login() {
    let now = Timestamp::from_unix_millis(1_700_000_000_000).expect("time");
    let store = Arc::new(FakeIdentityStore::new());
    let passwords =
        PasswordService::new_with_argon2(PasswordPolicy::new([]), 8_192, 1, 1).expect("policy");
    let password = SecretString::new("Cedar!Lake7-Comet");
    let user = User::new(
        UserId::generate(),
        LoginId::new("alice").expect("login"),
        "Alice",
        now,
    )
    .expect("user");
    let hash = passwords.hash(password.clone()).await.expect("hash");
    store.insert_account(LoginAccount {
        user: user.clone(),
        credential: Credential::new(user.id(), hash, now),
    });
    let service = LoginService::new(
        store.clone(),
        Arc::new(InterleavedStore {
            inner: store.clone(),
            mutation: None,
            failure: true,
        }),
        passwords,
        tokens(),
        Arc::new(FixedClock::new(now)),
        vec![7; 32],
        900,
        2_592_000,
    );
    service
        .login(
            LoginId::new("alice").expect("login"),
            password,
            request_id(),
            None,
        )
        .await
        .expect("login succeeds");
    assert_eq!(
        store
            .find_account(user.id())
            .await
            .expect("lookup")
            .expect("account")
            .credential
            .failed_login_count(),
        1
    );
    assert_eq!(store.sessions_snapshot().len(), 1);
}

#[tokio::test]
async fn a_disabled_external_user_cannot_obtain_another_session() {
    use orbisync_identity::{ExternalAuthService, JwtIdentityProvider, SessionIssuer};
    use orbisync_testkit::external_idp::{LocalSigningIssuer, TestClaims};
    let now = Timestamp::from_unix_millis(1_700_000_000_000).expect("time");
    let store = Arc::new(FakeIdentityStore::new());
    let signer = LocalSigningIssuer::new("idp-key", [3; 32]);
    let dir = tempfile::tempdir().expect("temp directory");
    let path = dir.path().join("jwks.json");
    signer.write_jwks(&path);
    let provider = JwtIdentityProvider::from_jwks_file(
        &path,
        "https://idp.test".into(),
        "orbisync".into(),
        "EdDSA",
        30,
    )
    .expect("provider");
    let token = signer.sign(&TestClaims {
        iss: "https://idp.test".into(),
        sub: "alice".into(),
        aud: "orbisync".into(),
        exp: 2_000_000_000,
        nbf: 1_600_000_000,
        iat: 1_600_000_000,
        email: None,
        role: None,
    });
    let issuer = Arc::new(SessionIssuer::new(
        store.clone(),
        tokens(),
        Arc::new(FixedClock::new(now)),
        vec![7; 32],
        900,
        2_592_000,
    ));
    let service = ExternalAuthService::new(
        Arc::new(provider),
        Arc::new(ExternalLookup(store.clone())),
        issuer,
        vec![],
    );
    let initial = service
        .authenticate(&token, request_id(), None)
        .await
        .expect("first login");
    let mut user = store
        .find_user(initial.user_id)
        .await
        .expect("lookup")
        .expect("user without a credential");
    assert!(
        store
            .find_account(user.id())
            .await
            .expect("account lookup")
            .is_none()
    );
    user.disable(now).expect("disable");
    store
        .apply(
            IdentityMutation::StoreUserStatus { user: user.clone() },
            IdentityAuditEvent {
                occurred_at: now,
                actor_id: Some(user.id()),
                action: "user.disabled",
                resource_id: Some(user.id().to_string()),
                request_id: request_id(),
                succeeded: true,
            },
        )
        .await
        .expect("disable commits");
    let error = service
        .authenticate(&token, request_id(), None)
        .await
        .expect_err("disabled external login rejected");
    assert_eq!(error.kind(), ApplicationErrorKind::Unauthenticated);
    assert_eq!(
        store.sessions_snapshot().len(),
        1,
        "no new session inserted"
    );
    assert!(
        store
            .sessions_snapshot()
            .values()
            .all(|session| !session.is_active_at(now))
    );
}
