//! Ports for the authentication methods added by ADR-026.
//!
//! The design documents describe an `IdentityProvider` trait, but no such
//! trait existed in the code: it appeared only inside fenced code blocks in
//! `auth-authorization.md` and the specification. This module defines it for
//! the first time, together with the port that bounds which worlds a
//! temporary subject may join.

use orbisync_domain::{Timestamp, UserId};

use crate::ApplicationError;

/// An external subject, as proven by a verified token.
///
/// Only the fields the issuer authenticated are carried. Self-asserted claims
/// such as `email`, `name`, `groups` or `role` are deliberately absent so they
/// cannot reach identity or authorization decisions: an issuer must not be
/// able to take over a local account by asserting its address, nor grant
/// itself permissions by asserting a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    /// Verified `iss` claim.
    pub issuer: String,
    /// Verified `sub` claim, stable for the lifetime of the upstream account.
    pub subject: String,
}

/// Why an external token was refused.
///
/// The transport collapses every variant into one opaque failure, so a caller
/// cannot use the distinction to probe the configuration. The variants exist
/// for operator-facing logs and for tests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalAuthError {
    /// The token is not a well-formed JWT.
    #[error("token is malformed")]
    Malformed,
    /// No configured key matches the token's `kid`.
    #[error("token key is unknown")]
    UnknownKey,
    /// The signature does not verify under the selected key.
    #[error("token signature is invalid")]
    InvalidSignature,
    /// The `alg` header is not the algorithm expected for the selected key.
    #[error("token algorithm is not accepted")]
    UnacceptedAlgorithm,
    /// `iss` does not match the configured issuer.
    #[error("token issuer is not accepted")]
    UnacceptedIssuer,
    /// `aud` does not contain the configured audience.
    #[error("token audience is not accepted")]
    UnacceptedAudience,
    /// The token is expired or not yet valid.
    #[error("token is outside its validity window")]
    OutsideValidityWindow,
    /// A required claim is missing or empty.
    #[error("token is missing a required claim")]
    MissingClaim,
    /// The configured keys could not be read.
    #[error("identity provider keys are unavailable")]
    KeysUnavailable,
}

/// Verifies tokens issued by a configured external identity provider.
///
/// Injected by the composition root, so the verification rules live behind one
/// boundary and a deployment without external authentication configured simply
/// has no implementation wired.
#[async_trait::async_trait]
pub trait IdentityProvider: Send + Sync + 'static {
    /// Verifies `token` and returns the issuer and subject it proves.
    ///
    /// # Errors
    ///
    /// Returns an [`ExternalAuthError`] for every token that fails any check.
    /// Implementations must fail closed: a key file that cannot be read, or a
    /// `kid` with no configured key, is a refusal rather than a pass.
    async fn verify(
        &self,
        token: &str,
        now: Timestamp,
    ) -> Result<ExternalIdentity, ExternalAuthError>;
}

#[async_trait::async_trait]
impl<T> IdentityProvider for std::sync::Arc<T>
where
    T: IdentityProvider + ?Sized,
{
    async fn verify(
        &self,
        token: &str,
        now: Timestamp,
    ) -> Result<ExternalIdentity, ExternalAuthError> {
        (**self).verify(token, now).await
    }
}

/// Maps a verified `(issuer, subject)` pair to an internal user.
///
/// Both halves are part of the key. Resolving on the subject alone would let
/// one issuer claim another issuer's users, since a subject string is only
/// unique within the issuer that minted it.
#[async_trait::async_trait]
pub trait ExternalIdentityStore: Send + Sync + 'static {
    /// Returns the user already bound to this pair, if any.
    ///
    /// # Errors
    ///
    /// Returns a port failure when the lookup cannot be performed. The caller
    /// refuses the login rather than creating a second user for a pair that
    /// may already exist.
    async fn find_user(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<UserId>, ApplicationError>;
}

/// What the participation check concluded for a subject and a world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeDecision {
    /// The subject holds no temporary-subject row, so this boundary does not
    /// apply and the existing authorization rules decide alone.
    NotEphemeral,
    /// The subject may participate in the world.
    Allowed,
    /// The subject may not participate in the world.
    Denied,
}

impl ScopeDecision {
    /// Returns whether participation may proceed.
    #[must_use]
    pub const fn permits(self) -> bool {
        matches!(self, Self::NotEphemeral | Self::Allowed)
    }
}

/// Bounds which worlds a temporary subject may participate in (ADR-026 §6).
///
/// This is a new authorization point, not a reuse of an existing one: nothing
/// in the join path authorized a participation target before ADR-026, so
/// converging the new methods onto the existing route would not have bounded
/// them. The check narrows what RBAC already allows and can never widen it.
#[async_trait::async_trait]
pub trait EphemeralSubjectScope: Send + Sync + 'static {
    /// Decides whether `user` may participate in `world`.
    ///
    /// Also enforces the subject's absolute deadline: a temporary subject past
    /// `expires_at` is denied here regardless of whether the revocation job
    /// has run, so a delayed job never extends access.
    ///
    /// # Errors
    ///
    /// Returns an error when the decision cannot be made. Callers treat that
    /// as a denial: "cannot confirm the subject may join" and "confirmed it
    /// may not" are the same answer at this boundary.
    async fn decide(
        &self,
        user: UserId,
        world: uuid::Uuid,
        now: Timestamp,
    ) -> Result<ScopeDecision, ApplicationError>;
}

#[async_trait::async_trait]
impl<T> EphemeralSubjectScope for std::sync::Arc<T>
where
    T: EphemeralSubjectScope + ?Sized,
{
    async fn decide(
        &self,
        user: UserId,
        world: uuid::Uuid,
        now: Timestamp,
    ) -> Result<ScopeDecision, ApplicationError> {
        (**self).decide(user, world, now).await
    }
}

#[cfg(test)]
mod tests {
    use super::ScopeDecision;

    #[test]
    fn only_a_denial_stops_participation() {
        assert!(ScopeDecision::Allowed.permits());
        // A permanent subject is unaffected by this boundary, so the check
        // must not turn into a blanket denial for local accounts.
        assert!(ScopeDecision::NotEphemeral.permits());
        assert!(!ScopeDecision::Denied.permits());
    }
}
