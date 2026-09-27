//! Local login orchestration over application ports.

use std::sync::Arc;

use orbisync_application::{
    ApplicationError, ApplicationErrorKind, IdentityRepository, SecretString,
};
use orbisync_domain::{LoginId, User, UserId, UserStatus};

use crate::{PasswordError, PasswordService};

/// Authenticated subject returned after credential verification.
///
/// `must_change_password` is advisory: the server signals that the user was
/// created with a temporary password and should change it, but does not block
/// other operations. Clients should prompt for a password change when true
/// (`auth-authorization.md` §2, W-27 decision). Enforcement is a future ADR if needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticationSubject {
    /// Immutable server-loaded user identifier.
    pub user_id: UserId,
    /// Advisory flag indicating the temporary password should be changed.
    pub must_change_password: bool,
}

/// Local login service that always performs one lookup and one Argon2 verify.
#[derive(Debug)]
pub struct AuthenticationService<R> {
    repository: Arc<R>,
    passwords: PasswordService,
}

impl<R> AuthenticationService<R>
where
    R: IdentityRepository,
{
    /// Creates a login service over an injected repository port.
    #[must_use]
    pub fn new(repository: Arc<R>, passwords: PasswordService) -> Self {
        Self {
            repository,
            passwords,
        }
    }

    /// Authenticates without revealing account existence, password mismatch or lock state.
    ///
    /// The repository performs one logical credential lookup. A missing account
    /// uses the process-local dummy PHC, so every normal failure performs exactly
    /// one Argon2 verification.
    ///
    /// # Errors
    ///
    /// Returns the same unauthenticated error for missing, disabled, locked and
    /// mismatched credentials. Port and hashing infrastructure failures remain
    /// separate internal failures.
    pub async fn authenticate(
        &self,
        login_id: &LoginId,
        password: SecretString,
        now: orbisync_domain::Timestamp,
    ) -> Result<AuthenticationSubject, ApplicationError> {
        let account = self
            .repository
            .find_login(login_id)
            .await
            .map_err(|_| ApplicationError::port_failure("identity repository unavailable"))?;
        let hash = account.as_ref().map_or_else(
            || self.passwords.dummy_hash().clone(),
            |value| value.credential.password_hash().clone(),
        );
        let password_for_rehash = password.clone();
        let verified = self
            .passwords
            .verify(password, hash)
            .await
            .map_err(password_error)?;

        let Some(account) = account else {
            return Err(unauthenticated());
        };
        let locked = account.credential.is_locked_at(now);
        let enabled = account.user.status() == UserStatus::Active;
        if !verified || locked || !enabled {
            self.repository
                .record_login_failure(account.user.id(), now)
                .await
                .map_err(|_| ApplicationError::port_failure("identity repository unavailable"))?;
            return Err(unauthenticated());
        }
        let expected_failed = account.credential.failed_login_count();
        let expected_locked = account.credential.locked_until();
        if PasswordService::needs_rehash(account.credential.password_hash()) {
            let upgraded = self
                .passwords
                .rehash_existing(password_for_rehash)
                .await
                .map_err(password_error)?;
            // Optimistic reset with new hash; if concurrent modification,
            // the password upgrade is still applied atomically only when
            // predicate matches. On predicate miss we still succeed but
            // keep the concurrent failure count (RV-B success semantics).
            // Reason: The returned bool (whether row was updated) is intentionally not used;
            // login still succeeds and the concurrent failure count is preserved atomically (RV-B).
            #[allow(clippy::let_underscore_must_use)]
            let _ = self
                .repository
                .reset_login_success(
                    account.user.id(),
                    expected_failed,
                    expected_locked,
                    now,
                    Some(upgraded),
                )
                .await
                .map_err(|_| ApplicationError::port_failure("identity repository unavailable"))?;
        } else {
            // Reason: Same as above – bool indicates predicate match, not success.
            #[allow(clippy::let_underscore_must_use)]
            let _ = self
                .repository
                .reset_login_success(
                    account.user.id(),
                    expected_failed,
                    expected_locked,
                    now,
                    None,
                )
                .await
                .map_err(|_| ApplicationError::port_failure("identity repository unavailable"))?;
        }
        Ok(subject(&account.user))
    }
}

fn subject(user: &User) -> AuthenticationSubject {
    AuthenticationSubject {
        user_id: user.id(),
        must_change_password: user.must_change_password(),
    }
}

fn unauthenticated() -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorKind::Unauthenticated,
        "authentication failed",
    )
}

fn password_error(error: PasswordError) -> ApplicationError {
    match error {
        PasswordError::RateLimited => ApplicationError::new(
            ApplicationErrorKind::RateLimited,
            "password hashing capacity exhausted",
        ),
        PasswordError::PolicyViolation
        | PasswordError::Unavailable
        | PasswordError::InvalidHash => {
            ApplicationError::port_failure("password verification unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use orbisync_application::{IdentityPortError, IdentityRepository, LoginAccount, SecretString};
    use orbisync_domain::{
        AuthSession, AuthSessionId, Credential, LoginId, PasswordHash, Role, Timestamp, User,
        UserId,
    };

    use super::AuthenticationService;
    use crate::{PasswordPolicy, PasswordService};

    #[derive(Debug)]
    struct Repository {
        account: Mutex<Option<LoginAccount>>,
        lookups: Mutex<usize>,
    }

    #[async_trait]
    impl IdentityRepository for Repository {
        async fn find_login(
            &self,
            _login_id: &LoginId,
        ) -> Result<Option<LoginAccount>, IdentityPortError> {
            *self.lookups.lock().expect("lock") += 1;
            Ok(self.account.lock().expect("lock").clone())
        }

        async fn find_account(
            &self,
            _user_id: UserId,
        ) -> Result<Option<LoginAccount>, IdentityPortError> {
            Ok(self.account.lock().expect("lock").clone())
        }

        async fn save_credential(&self, credential: &Credential) -> Result<(), IdentityPortError> {
            if let Some(account) = self.account.lock().expect("lock").as_mut() {
                account.credential = credential.clone();
            }
            Ok(())
        }

        async fn record_login_failure(
            &self,
            _user_id: UserId,
            _now: Timestamp,
        ) -> Result<orbisync_application::LoginFailureOutcome, IdentityPortError> {
            let mut guard = self.account.lock().expect("lock");
            if let Some(account) = guard.as_mut() {
                let next = account.credential.failed_login_count().saturating_add(1);
                let locked = if next >= Credential::LOGIN_FAILURE_THRESHOLD {
                    Some(
                        _now.checked_add_millis(Credential::LOGIN_LOCKOUT_MILLIS)
                            .expect("lock deadline"),
                    )
                } else {
                    account.credential.locked_until()
                };
                account.credential = Credential::reconstitute(
                    account.user.id(),
                    account.credential.password_hash().clone(),
                    account.credential.password_changed_at(),
                    next,
                    locked,
                );
                Ok(orbisync_application::LoginFailureOutcome {
                    failed_login_count: next,
                    locked_until: locked,
                })
            } else {
                Err(IdentityPortError::NotFound)
            }
        }

        async fn reset_login_success(
            &self,
            _user_id: UserId,
            expected_failed_count: u32,
            expected_locked_until: Option<Timestamp>,
            _now: Timestamp,
            new_password_hash: Option<PasswordHash>,
        ) -> Result<bool, IdentityPortError> {
            let mut guard = self.account.lock().expect("lock");
            if let Some(account) = guard.as_mut() {
                if account.credential.failed_login_count() != expected_failed_count
                    || account.credential.locked_until() != expected_locked_until
                {
                    return Ok(false);
                }
                let new_hash =
                    new_password_hash.unwrap_or_else(|| account.credential.password_hash().clone());
                let new_changed =
                    if new_hash.expose_phc() != account.credential.password_hash().expose_phc() {
                        _now
                    } else {
                        account.credential.password_changed_at()
                    };
                account.credential =
                    Credential::reconstitute(account.user.id(), new_hash, new_changed, 0, None);
                Ok(true)
            } else {
                Err(IdentityPortError::NotFound)
            }
        }

        async fn roles_for_user(&self, _user_id: UserId) -> Result<Vec<Role>, IdentityPortError> {
            Ok(Vec::new())
        }

        async fn find_session(
            &self,
            _session_id: AuthSessionId,
        ) -> Result<Option<AuthSession>, IdentityPortError> {
            Ok(None)
        }

        async fn save_session(&self, _session: &AuthSession) -> Result<(), IdentityPortError> {
            Ok(())
        }
    }

    #[test]
    fn missing_and_wrong_password_are_indistinguishable() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("startup");
            let valid = SecretString::new("Cedar!Lake7-Comet");
            let hash = passwords.hash(valid).await.expect("hash");
            let now = Timestamp::from_unix_millis(1_000).expect("time");
            let user = User::new(
                UserId::generate(),
                LoginId::new("ada").expect("login"),
                "Ada",
                now,
            )
            .expect("user");
            let account = LoginAccount {
                credential: Credential::new(user.id(), hash, now),
                user,
            };
            let present_repo = Arc::new(Repository {
                account: Mutex::new(Some(account)),
                lookups: Mutex::new(0),
            });
            let present = AuthenticationService::new(present_repo.clone(), passwords.clone());
            let wrong = present
                .authenticate(
                    &LoginId::new("ada").expect("login"),
                    SecretString::new("Wrong!Lake7-Comet"),
                    now,
                )
                .await
                .expect_err("wrong password");

            let missing_repo = Arc::new(Repository {
                account: Mutex::new(None),
                lookups: Mutex::new(0),
            });
            let missing = AuthenticationService::new(missing_repo.clone(), passwords);
            let absent = missing
                .authenticate(
                    &LoginId::new("nobody").expect("login"),
                    SecretString::new("Wrong!Lake7-Comet"),
                    now,
                )
                .await
                .expect_err("missing account");
            assert_eq!(wrong.kind(), absent.kind());
            assert_eq!(wrong.detail(), absent.detail());
            assert_eq!(*present_repo.lookups.lock().expect("lock"), 1);
            assert_eq!(*missing_repo.lookups.lock().expect("lock"), 1);
        });
    }

    #[allow(dead_code)]
    fn _hash_type_is_domain_owned(_hash: PasswordHash) {}
}
