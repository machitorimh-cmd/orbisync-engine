//! Argon2id hashing, verification and password policy from ADR-015.

use std::collections::HashSet;
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{
    Algorithm, Argon2, Params, PasswordHash as ParsedHash, PasswordVerifier as _, Version,
};
use orbisync_application::SecretString;
use orbisync_domain::PasswordHash;
use tokio::sync::Semaphore;

/// Default Argon2id memory cost in KiB.
pub const DEFAULT_MEMORY_KIB: u32 = 65_536;
/// Default Argon2id time cost.
pub const DEFAULT_TIME_COST: u32 = 3;
/// Default Argon2id parallelism.
pub const DEFAULT_PARALLELISM: u32 = 1;
const OUTPUT_LENGTH: usize = 32;
/// Default number of concurrent password-hash workers.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Password hashing or policy failure without secret material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// Candidate violates the public password policy.
    #[error("password policy rejected the candidate")]
    PolicyViolation,
    /// Hashing infrastructure failed or was closed.
    #[error("password hashing is unavailable")]
    Unavailable,
    /// All bounded password hashing workers are busy.
    #[error("password hashing capacity is exhausted")]
    RateLimited,
    /// Stored PHC data is invalid.
    #[error("stored password hash is invalid")]
    InvalidHash,
}

/// Validates length, context words and a versioned external denylist.
#[derive(Debug, Clone)]
pub struct PasswordPolicy {
    denylist: HashSet<String>,
    min_length: usize,
}

impl PasswordPolicy {
    /// Builds a policy from the repository-managed weak-password list.
    ///
    /// Entries are compared case-insensitively after Unicode lowercase mapping.
    /// The minimum length defaults to 12 (`Config::auth.password_min_length`
    /// default); callers that wire configuration should use
    /// `with_min_length` to override it.
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        Self {
            denylist: entries
                .into_iter()
                .map(|entry| entry.to_lowercase())
                .collect(),
            min_length: 12,
        }
    }

    /// Overrides the minimum accepted password length.
    ///
    /// Wired from `auth.password_min_length` by the composition root.
    #[must_use]
    pub fn with_min_length(mut self, min_length: u32) -> Self {
        self.min_length = min_length as usize;
        self
    }

    /// Returns the configured minimum length.
    #[must_use]
    pub const fn min_length(&self) -> usize {
        self.min_length
    }

    /// Builds production policy from exactly 10,000 approved entries.
    ///
    /// # Errors
    ///
    /// Returns unavailable unless the corpus contains exactly 10,000 distinct
    /// case-insensitive entries. Production startup therefore fails closed.
    pub fn production(entries: impl IntoIterator<Item = String>) -> Result<Self, PasswordError> {
        let policy = Self::new(entries);
        if policy.denylist.len() != 10_000 {
            return Err(PasswordError::Unavailable);
        }
        Ok(policy)
    }

    /// Validates a candidate without normalizing or trimming the accepted value.
    ///
    /// # Errors
    ///
    /// Returns [`PasswordError::PolicyViolation`] unless the password contains
    /// `min_length` through 128 Unicode scalar values and is absent from the denylist and
    /// contextual/simple-pattern checks.
    pub fn validate(&self, candidate: &SecretString) -> Result<(), PasswordError> {
        let value = candidate.expose_secret();
        let length = value.chars().count();
        let lower = value.to_lowercase();
        if !(self.min_length..=128).contains(&length)
            || self.denylist.contains(&lower)
            || lower.contains("orbisync")
            || lower.contains("password")
            || is_single_repeated_character(value)
            || is_ascii_sequence(value)
        {
            return Err(PasswordError::PolicyViolation);
        }
        Ok(())
    }
}

fn is_single_repeated_character(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    characters.all(|character| character == first)
}

fn is_ascii_sequence(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 12
        && (bytes
            .windows(2)
            .all(|pair| pair[1] == pair[0].wrapping_add(1))
            || bytes
                .windows(2)
                .all(|pair| pair[0] == pair[1].wrapping_add(1)))
}

/// Bounded Argon2id service with a process-local dummy hash.
#[derive(Clone)]
pub struct PasswordService {
    policy: PasswordPolicy,
    permits: Arc<Semaphore>,
    dummy_hash: PasswordHash,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl core::fmt::Debug for PasswordService {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PasswordService")
            .field("policy", &self.policy)
            .field("permits", &"bounded")
            .field("dummy_hash", &"[REDACTED]")
            .finish()
    }
}

impl PasswordService {
    /// Creates the bounded service and its process-local dummy PHC hash.
    ///
    /// This performs one expensive hash and should run during startup.
    ///
    /// # Errors
    ///
    /// Returns an unavailable error if Argon2 parameters or CSPRNG hashing fail.
    pub fn new(policy: PasswordPolicy) -> Result<Self, PasswordError> {
        Self::new_with_argon2(
            policy,
            DEFAULT_MEMORY_KIB,
            DEFAULT_TIME_COST,
            DEFAULT_PARALLELISM,
        )
    }

    /// Creates a service with configured Argon2id parameters.
    pub fn new_with_argon2(
        policy: PasswordPolicy,
        memory_kib: u32,
        iterations: u32,
        parallelism: u32,
    ) -> Result<Self, PasswordError> {
        Self::new_with_argon2_and_concurrency(
            policy,
            memory_kib,
            iterations,
            parallelism,
            DEFAULT_CONCURRENCY,
        )
    }

    /// Creates a service with configured Argon2id parameters and hash-worker
    /// concurrency.
    pub fn new_with_argon2_and_concurrency(
        policy: PasswordPolicy,
        memory_kib: u32,
        iterations: u32,
        parallelism: u32,
        concurrency: usize,
    ) -> Result<Self, PasswordError> {
        if concurrency == 0 {
            return Err(PasswordError::Unavailable);
        }
        let _ = configured_argon2(memory_kib, iterations, parallelism)?;
        let dummy = SecretString::new(random_dummy_password());
        let dummy_hash = hash_synchronously(&dummy, memory_kib, iterations, parallelism)?;
        Ok(Self {
            policy,
            permits: Arc::new(Semaphore::new(concurrency)),
            dummy_hash,
            memory_kib,
            iterations,
            parallelism,
        })
    }

    /// Returns the dummy hash used for nonexistent account verification.
    #[must_use]
    pub const fn dummy_hash(&self) -> &PasswordHash {
        &self.dummy_hash
    }

    /// Validates policy and hashes on the bounded blocking executor.
    ///
    /// # Errors
    ///
    /// Returns policy, availability, or hashing errors without secret details.
    pub async fn hash(&self, password: SecretString) -> Result<PasswordHash, PasswordError> {
        self.policy.validate(&password)?;
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| PasswordError::RateLimited)?;
        let memory_kib = self.memory_kib;
        let iterations = self.iterations;
        let parallelism = self.parallelism;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hash_synchronously(&password, memory_kib, iterations, parallelism)
        })
        .await
        .map_err(|_| PasswordError::Unavailable)?
    }

    /// Performs exactly one Argon2 verification on the bounded blocking executor.
    ///
    /// # Errors
    ///
    /// Returns an invalid-hash or availability error. A password mismatch is `Ok(false)`.
    pub async fn verify(
        &self,
        password: SecretString,
        password_hash: PasswordHash,
    ) -> Result<bool, PasswordError> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| PasswordError::RateLimited)?;
        let memory_kib = self.memory_kib;
        let iterations = self.iterations;
        let parallelism = self.parallelism;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            verify_synchronously(
                &password,
                &password_hash,
                memory_kib,
                iterations,
                parallelism,
            )
        })
        .await
        .map_err(|_| PasswordError::Unavailable)?
    }

    /// Returns whether a stored Argon2id hash uses older parameters.
    #[must_use]
    pub fn needs_rehash(password_hash: &PasswordHash) -> bool {
        !password_hash
            .expose_phc()
            .starts_with("$argon2id$v=19$m=65536,t=3,p=1$")
    }

    /// Rehashes an already authenticated password with current parameters.
    ///
    /// Existing credentials bypass the new-password denylist: a policy update
    /// must not prevent transparent parameter upgrades after successful login.
    ///
    /// # Errors
    ///
    /// Returns an availability error when the bounded worker is busy or hashing fails.
    pub async fn rehash_existing(
        &self,
        password: SecretString,
    ) -> Result<PasswordHash, PasswordError> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| PasswordError::RateLimited)?;
        let memory_kib = self.memory_kib;
        let iterations = self.iterations;
        let parallelism = self.parallelism;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hash_synchronously(&password, memory_kib, iterations, parallelism)
        })
        .await
        .map_err(|_| PasswordError::Unavailable)?
    }
}

fn configured_argon2(
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<Argon2<'static>, PasswordError> {
    let params = Params::new(memory_kib, iterations, parallelism, Some(OUTPUT_LENGTH))
        .map_err(|_| PasswordError::Unavailable)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

fn hash_synchronously(
    password: &SecretString,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<PasswordHash, PasswordError> {
    let mut salt_bytes = [0_u8; 16];
    {
        use rand::TryRng as _;
        rand::rngs::SysRng
            .try_fill_bytes(&mut salt_bytes)
            .map_err(|_| PasswordError::Unavailable)?;
    }
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| PasswordError::Unavailable)?;
    let rendered = configured_argon2(memory_kib, iterations, parallelism)?
        .hash_password(password.expose_secret().as_bytes(), &salt)
        .map_err(|_| PasswordError::Unavailable)?
        .to_string();
    PasswordHash::new(rendered).map_err(|_| PasswordError::Unavailable)
}

fn verify_synchronously(
    password: &SecretString,
    password_hash: &PasswordHash,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<bool, PasswordError> {
    let parsed =
        ParsedHash::new(password_hash.expose_phc()).map_err(|_| PasswordError::InvalidHash)?;
    Ok(configured_argon2(memory_kib, iterations, parallelism)?
        .verify_password(password.expose_secret().as_bytes(), &parsed)
        .is_ok())
}

fn random_dummy_password() -> String {
    use base64::Engine as _;
    use rand::Rng as _;

    let mut bytes = [0_u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::{PasswordPolicy, PasswordService};
    use orbisync_application::SecretString;

    #[test]
    fn policy_enforces_length_denylist_and_context() {
        let policy = PasswordPolicy::new([String::from("correct horse battery staple")]);
        assert!(policy.validate(&SecretString::new("short")).is_err());
        assert!(
            policy
                .validate(&SecretString::new("correct horse battery staple"))
                .is_err()
        );
        assert!(
            policy
                .validate(&SecretString::new("Use-OrbiSync-123!"))
                .is_err()
        );
        assert!(
            policy
                .validate(&SecretString::new("Cedar!Lake7-Comet"))
                .is_ok()
        );
    }

    #[test]
    fn production_policy_fails_closed_for_missing_or_truncated_corpus() {
        assert!(PasswordPolicy::production(Vec::new()).is_err());
        assert!(PasswordPolicy::production(vec![String::from("weak"); 10_000]).is_err());
    }

    #[test]
    fn outdated_parameters_are_detected_and_rehashed() {
        let old = orbisync_domain::PasswordHash::new("$argon2id$v=19$m=8192,t=1,p=1$c2FsdA$aGFzaA")
            .expect("valid PHC shape");
        assert!(PasswordService::needs_rehash(&old));
        let service = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("startup");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let current = runtime
            .block_on(service.rehash_existing(SecretString::new("Cedar!Lake7-Comet")))
            .expect("rehash");
        assert!(!PasswordService::needs_rehash(&current));
    }

    #[test]
    fn hashing_round_trip_and_dummy_verification_work() {
        let service = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("startup hash");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let password = SecretString::new("Cedar!Lake7-Comet");
            let hash = service.hash(password.clone()).await.expect("hash");
            assert!(service.verify(password, hash).await.expect("verify"));
            assert!(
                !service
                    .verify(
                        SecretString::new("Wrong!Lake7-Comet"),
                        service.dummy_hash().clone(),
                    )
                    .await
                    .expect("dummy verify")
            );
        });
    }

    #[test]
    fn configured_argon2_parameters_are_used_in_the_phc_hash() {
        let service =
            PasswordService::new_with_argon2(PasswordPolicy::new(Vec::new()), 8_192, 1, 1)
                .expect("configured startup hash");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let hash = runtime
            .block_on(service.hash(SecretString::new("Cedar!Lake7-Comet")))
            .expect("hash");
        assert!(
            hash.expose_phc()
                .starts_with("$argon2id$v=19$m=8192,t=1,p=1$")
        );
    }

    #[test]
    fn configured_hash_concurrency_controls_capacity() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let password = SecretString::new("Cedar!Lake7-Comet");
            let one = PasswordService::new_with_argon2_and_concurrency(
                PasswordPolicy::new(Vec::new()),
                8_192,
                1,
                1,
                1,
            )
            .expect("service");
            let held = one.permits.clone().try_acquire_owned().expect("permit");
            assert_eq!(
                one.hash(password.clone()).await,
                Err(super::PasswordError::RateLimited)
            );
            drop(held);

            let two = PasswordService::new_with_argon2_and_concurrency(
                PasswordPolicy::new(Vec::new()),
                8_192,
                1,
                1,
                2,
            )
            .expect("service");
            let held = two.permits.clone().try_acquire_owned().expect("permit");
            assert!(two.hash(password).await.is_ok());
            drop(held);
        });
    }
}
