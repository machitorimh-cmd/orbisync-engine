//! Property tests for allow-only RBAC combinations.

use orbisync_domain::{Permission, Role, RoleId};
use orbisync_identity::rbac::{AuthorizationDecision, RbacAuthorizer};
use proptest::prelude::*;

const PERMISSION_NAMES: [&str; 4] = [
    "admin.users.read",
    "admin.users.create",
    "admin.roles.read",
    "admin.audit.read",
];

proptest! {
    #[test]
    fn permission_authorization_matches_assigned_role_combinations(
        grants in prop::collection::vec(any::<bool>(), 4..=4),
        requested_index in 0usize..4,
    ) {
        let permissions: Vec<Permission> = PERMISSION_NAMES
            .iter()
            .map(|name| Permission::new(*name).expect("static permission is valid"))
            .collect();
        let mut roles = Vec::new();
        for (index, (permission, granted)) in permissions.iter().zip(&grants).enumerate() {
            if *granted {
                roles.push(
                    Role::new(
                        RoleId::generate(),
                        format!("property-role-{index}"),
                        None,
                        [permission.clone()],
                    )
                    .expect("static role is valid"),
                );
            }
        }

        let effective = RbacAuthorizer::effective_permissions(&roles);
        for (permission, granted) in permissions.iter().zip(&grants) {
            prop_assert_eq!(effective.contains(permission), *granted);
        }
        let expected = if grants[requested_index] {
            AuthorizationDecision::Allow
        } else {
            AuthorizationDecision::Deny
        };
        prop_assert_eq!(
            RbacAuthorizer::authorize(&roles, &permissions[requested_index]),
            expected,
        );
    }
}
