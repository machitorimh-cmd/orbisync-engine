//! Guest and name-only participation (ADR-026 §3).
//!
//! Both methods issue a subject that owns a `users` row but no credential row.
//! The row has to exist: `auth_sessions.user_id` and
//! `persistent_entities.owner_id` are foreign keys into `users`, so without it
//! the subject could neither hold a session nor own anything it creates.
//! "No permanent account" means no `user_credentials` row, which is what keeps
//! the subject off the password login path.

use std::sync::Arc;

use orbisync_application::{
    ApplicationError, ApplicationErrorKind, EphemeralSubjectRecord, LoginResult, NewSubjectUser,
    RequestId,
};
use orbisync_domain::{AuthMethod, LoginId, Timestamp, UserId, UserKind};
use uuid::Uuid;

use crate::session_issuer::{IssuableSubject, SessionIssuer};

/// Resolved, validated settings for one temporary method.
///
/// Built once at startup from configuration. Holding role ids rather than role
/// names means an unresolvable role stops the server instead of silently
/// producing subjects with no permissions.
#[derive(Debug, Clone)]
pub struct EphemeralMethodPolicy {
    /// Which method this policy governs.
    pub method: AuthMethod,
    /// Roles granted to a new subject. Never empty.
    pub grant_roles: Vec<orbisync_domain::RoleId>,
    /// Absolute subject lifetime in seconds.
    pub session_ttl_seconds: u64,
    /// Worlds a subject may join. Never empty.
    pub allowed_worlds: Vec<Uuid>,
    /// Prefix of the generated anonymous display name.
    pub display_name_prefix: String,
}

/// Issues guest and name-only subjects.
#[derive(Debug)]
pub struct EphemeralSubjectService {
    issuer: Arc<SessionIssuer>,
    guest: Option<EphemeralMethodPolicy>,
    name_only: Option<EphemeralMethodPolicy>,
}

impl EphemeralSubjectService {
    /// Creates the service. A `None` policy means the method is disabled.
    #[must_use]
    pub const fn new(
        issuer: Arc<SessionIssuer>,
        guest: Option<EphemeralMethodPolicy>,
        name_only: Option<EphemeralMethodPolicy>,
    ) -> Self {
        Self {
            issuer,
            guest,
            name_only,
        }
    }

    /// Issues an anonymous guest subject.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when guest participation is disabled, and a
    /// port failure when the subject cannot be committed.
    pub async fn issue_guest(
        &self,
        request_id: RequestId,
        source_ip: Option<String>,
    ) -> Result<LoginResult, ApplicationError> {
        let policy = self.guest.as_ref().ok_or_else(method_disabled)?;
        let now = self.issuer.now();
        let user_id = UserId::generate();
        // The visitor supplies nothing at all here, so the name is derived from
        // the server-issued id and cannot be used to impersonate anyone.
        let display_name = format!("{}-{}", policy.display_name_prefix, short_id(user_id));
        self.issue(policy, user_id, display_name, now, request_id, source_ip)
            .await
    }

    /// Issues a subject carrying a visitor-supplied display name.
    ///
    /// The name is a label only. Two visitors may choose the same one and stay
    /// distinct subjects, because identity comes from the generated [`UserId`]
    /// and the login identifier is derived from it rather than from the name.
    ///
    /// # Errors
    ///
    /// Returns `NotAuthorized` when the method is disabled and `DomainRule`
    /// when the display name is blank, overlong or holds control characters.
    pub async fn issue_name_only(
        &self,
        display_name: &str,
        request_id: RequestId,
        source_ip: Option<String>,
    ) -> Result<LoginResult, ApplicationError> {
        let policy = self.name_only.as_ref().ok_or_else(method_disabled)?;
        let display_name =
            orbisync_domain::validate_supplied_display_name(display_name).map_err(|error| {
                ApplicationError::new(ApplicationErrorKind::DomainRule, error.to_string())
            })?;
        let now = self.issuer.now();
        self.issue(
            policy,
            UserId::generate(),
            display_name,
            now,
            request_id,
            source_ip,
        )
        .await
    }

    async fn issue(
        &self,
        policy: &EphemeralMethodPolicy,
        user_id: UserId,
        display_name: String,
        now: Timestamp,
        request_id: RequestId,
        source_ip: Option<String>,
    ) -> Result<LoginResult, ApplicationError> {
        let login_id = LoginId::for_generated_subject(policy.method, user_id)
            .map_err(|_| ApplicationError::port_failure("login identifier generation failed"))?;
        let ttl_millis = i64::try_from(
            policy
                .session_ttl_seconds
                .checked_mul(1_000)
                .ok_or_else(|| ApplicationError::port_failure("session TTL overflow"))?,
        )
        .map_err(|_| ApplicationError::port_failure("session TTL overflow"))?;
        let expires_at = now
            .checked_add_millis(ttl_millis)
            .map_err(|_| ApplicationError::port_failure("session expiry overflow"))?;

        self.issuer
            .issue(
                IssuableSubject {
                    user_id,
                    new_user: Some(NewSubjectUser {
                        login_id,
                        display_name,
                        kind: UserKind::for_method(policy.method),
                        created_at: now,
                    }),
                    // Roles come from startup configuration. No request field
                    // reaches this list, so a client cannot choose its own.
                    grant_roles: policy.grant_roles.clone(),
                    ephemeral: Some(EphemeralSubjectRecord {
                        method: policy.method,
                        created_at: now,
                        expires_at,
                        // Snapshot of the configured boundary. Recorded per
                        // subject so the join path decides from stored state
                        // rather than re-reading configuration mid-session.
                        allowed_worlds: policy.allowed_worlds.clone(),
                    }),
                    external: None,
                },
                request_id,
                source_ip,
                audit_action(policy.method),
            )
            .await
    }

    /// Returns whether a method is enabled.
    #[must_use]
    pub const fn supports(&self, method: AuthMethod) -> bool {
        match method {
            AuthMethod::Guest => self.guest.is_some(),
            AuthMethod::NameOnly => self.name_only.is_some(),
            AuthMethod::Local | AuthMethod::External => false,
        }
    }
}

/// Returns the audit action recorded for a method's session issue.
const fn audit_action(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::Guest => "auth.guest",
        AuthMethod::NameOnly => "auth.name_only",
        AuthMethod::External => "auth.external",
        AuthMethod::Local => "auth.login",
    }
}

fn method_disabled() -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorKind::NotAuthorized,
        "authentication method is not enabled",
    )
}

/// Returns a short, readable suffix of a user id for a generated name.
///
/// Taken from the *end* of the identifier. [`UserId`] is a UUIDv7, whose
/// leading digits encode the creation millisecond, so a prefix would be
/// identical for two guests that arrive in the same millisecond and they would
/// be shown the same name. The trailing digits are the random block.
fn short_id(user_id: UserId) -> String {
    let text = user_id.to_string();
    let tail: String = text
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::{audit_action, short_id};
    use orbisync_domain::{AuthMethod, LoginId, UserId};

    #[test]
    fn generated_names_differ_for_subjects_created_in_the_same_millisecond() {
        // UserId is a UUIDv7: its leading digits are the creation timestamp, so
        // a prefix-based name would repeat for two guests arriving together.
        // Generate a batch back to back and require every name to be distinct.
        let names: std::collections::HashSet<String> =
            (0..64).map(|_| short_id(UserId::generate())).collect();
        assert_eq!(names.len(), 64, "generated display names must not collide");
        assert!(names.iter().all(|name| name.chars().count() == 8));
    }

    #[test]
    fn each_method_records_its_own_audit_action() {
        assert_eq!(audit_action(AuthMethod::Guest), "auth.guest");
        assert_eq!(audit_action(AuthMethod::NameOnly), "auth.name_only");
        assert_eq!(audit_action(AuthMethod::External), "auth.external");
        // The credential path keeps its existing action name so that existing
        // audit queries and dashboards continue to match.
        assert_eq!(audit_action(AuthMethod::Local), "auth.login");
    }

    #[test]
    fn generated_login_ids_are_unique_per_subject() {
        // Two name-only subjects that pick the same display name must not
        // collide on users.login_id.
        let a = LoginId::for_generated_subject(AuthMethod::NameOnly, UserId::generate())
            .expect("first");
        let b = LoginId::for_generated_subject(AuthMethod::NameOnly, UserId::generate())
            .expect("second");
        assert_ne!(a, b);
    }
}
