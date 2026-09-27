//! Allow-only RBAC evaluation.

use std::collections::BTreeSet;

use orbisync_domain::{Permission, Role};

/// Result of an RBAC evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// At least one assigned role grants the requested permission.
    Allow,
    /// No assigned role grants the requested permission.
    Deny,
}

/// Stateless allow-only role authorizer.
#[derive(Debug, Default, Clone, Copy)]
pub struct RbacAuthorizer;

impl RbacAuthorizer {
    /// Returns the union of permissions granted by all server-loaded roles.
    #[must_use]
    pub fn effective_permissions(roles: &[Role]) -> BTreeSet<Permission> {
        roles
            .iter()
            .flat_map(|role| role.permissions().iter().cloned())
            .collect()
    }

    /// Checks the requested permission against server-loaded roles only.
    #[must_use]
    pub fn authorize(roles: &[Role], requested: &Permission) -> AuthorizationDecision {
        if roles
            .iter()
            .any(|role| role.permissions().contains(requested))
        {
            AuthorizationDecision::Allow
        } else {
            AuthorizationDecision::Deny
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthorizationDecision, RbacAuthorizer};
    use orbisync_domain::{Permission, Role, RoleId};

    #[test]
    fn permissions_are_the_union_and_deny_is_implicit() {
        let read = Permission::new("admin.users.read").expect("valid");
        let create = Permission::new("admin.users.create").expect("valid");
        let roles = vec![
            Role::new(RoleId::generate(), "Reader", None, [read.clone()]).expect("valid"),
            Role::new(RoleId::generate(), "Creator", None, [create.clone()]).expect("valid"),
        ];
        let effective = RbacAuthorizer::effective_permissions(&roles);
        assert!(effective.contains(&read));
        assert!(effective.contains(&create));
        assert_eq!(
            RbacAuthorizer::authorize(&roles, &Permission::new("admin.audit.read").expect("valid")),
            AuthorizationDecision::Deny
        );
    }

    #[test]
    fn client_claimed_permissions_are_not_an_input() {
        let requested = Permission::new("admin.roles.delete").expect("valid");
        assert_eq!(
            RbacAuthorizer::authorize(&[], &requested),
            AuthorizationDecision::Deny
        );
    }
}
