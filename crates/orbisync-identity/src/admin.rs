//! User and role administration use cases over the atomic ADR-017 port.

use std::sync::Arc;

use base64::Engine as _;
use orbisync_application::{
    ApplicationError, ApplicationErrorKind, CreateRoleCommand, CreateUserCommand,
    EncryptedResponse, ExtensionEvent, IdempotencyCompletion, IdentityAdministrationStore,
    IdentityAuditEvent, IdentityMutation, SecretString, UpdateRoleCommand,
};
use orbisync_domain::{
    AuthSession, Clock, Credential, DomainErrorKind, Permission, Revision, Role, RoleId, User,
    UserId,
};
use rand::Rng as _;

use crate::{AuthorizationDecision, PasswordService, RbacAuthorizer};

/// User and role use cases with mandatory same-transaction audit insertion.
#[derive(Debug)]
pub struct IdentityAdministrationService<S, C> {
    store: Arc<S>,
    clock: Arc<C>,
    passwords: PasswordService,
}

/// Request metadata shared by idempotent password mutations.
#[derive(Debug)]
pub struct PasswordIdempotencyContext {
    /// Request correlation identifier.
    pub request_id: orbisync_application::RequestId,
    /// Idempotency key supplied by the client.
    pub idempotency_key: String,
    /// Owner token used to complete the idempotency record.
    pub idempotency_owner: String,
    /// Trusted-proxy-filtered source IP, when available.
    pub source_ip: Option<String>,
}

impl PasswordIdempotencyContext {
    /// Creates password mutation metadata.
    #[must_use]
    pub fn new(
        request_id: orbisync_application::RequestId,
        idempotency_key: String,
        idempotency_owner: String,
        source_ip: Option<String>,
    ) -> Self {
        Self {
            request_id,
            idempotency_key,
            idempotency_owner,
            source_ip,
        }
    }
}

impl<S, C> IdentityAdministrationService<S, C>
where
    S: IdentityAdministrationStore,
    C: Clock,
{
    /// Creates the first administrator and its full-access role atomically.
    ///
    /// # Errors
    ///
    /// Returns a validation, hashing, or atomic persistence failure.
    pub async fn bootstrap_administrator(
        &self,
        login_id: orbisync_domain::LoginId,
        display_name: String,
        request_id: orbisync_application::RequestId,
    ) -> Result<(User, SecretString), ApplicationError> {
        let now = self.clock.now();
        let temporary_password = generate_temporary_password();
        let password_hash = self
            .passwords
            .hash(temporary_password.clone())
            .await
            .map_err(|_| ApplicationError::port_failure("password hashing unavailable"))?;
        let mut user = User::new(UserId::generate(), login_id, display_name, now)?;
        let admin_permissions = [
            "admin.users.read",
            "admin.users.create",
            "admin.users.update",
            "admin.users.status",
            "admin.users.import",
            "admin.users.credentials.reset",
            "admin.roles.read",
            "admin.roles.create",
            "admin.roles.update",
            "admin.roles.delete",
            "admin.roles.assign",
            "admin.audit.read",
            "admin.diagnostics.read",
            "admin.worlds.create",
            "admin.worlds.read",
            "admin.worlds.update",
            "admin.worlds.archive",
        ]
        .into_iter()
        .map(Permission::new)
        .collect::<Result<Vec<_>, _>>()?;
        let world_permissions = [
            "world.instance.create",
            "world.instance.read",
            "world.instance.start",
            "world.instance.stop",
            "moderation.kick",
            "entity.spawn",
            "entity.update.own",
            "entity.update.any",
        ]
        .into_iter()
        .map(Permission::new)
        .collect::<Result<Vec<_>, _>>()?;
        let administrator_role = Role::new(
            RoleId::generate(),
            "Administrator",
            Some(String::from("Initial administration role")),
            admin_permissions,
        )?;
        let world_administrator_role = Role::new(
            RoleId::generate(),
            "World Administrator",
            Some(String::from("Initial world administration role")),
            world_permissions,
        )?;
        user.replace_roles(
            [administrator_role.id(), world_administrator_role.id()],
            now,
        )?;
        let credential = Credential::new(user.id(), password_hash, now);
        self.store
            .apply(
                IdentityMutation::BootstrapAdministrator {
                    user: user.clone(),
                    credential,
                    roles: vec![administrator_role, world_administrator_role],
                },
                IdentityAuditEvent {
                    occurred_at: now,
                    actor_id: None,
                    action: "administrator.bootstrapped",
                    resource_id: Some(user.id().to_string()),
                    request_id,
                    succeeded: true,
                },
            )
            .await
            .map_err(|err| {
                // IdentityPortError deliberately exposes only a secret-free, stable
                // classification. Keep that classification in the diagnostic while
                // retaining the redacted application error contract.
                tracing::error!(error = %err, "administrator bootstrap failed");
                ApplicationError::port_failure(format!(
                    "administrator bootstrap unavailable: {err}"
                ))
            })?;
        Ok((user, temporary_password))
    }

    /// Creates the administration service from application ports.
    #[must_use]
    pub fn new(store: Arc<S>, clock: Arc<C>, passwords: PasswordService) -> Self {
        Self {
            store,
            clock,
            passwords,
        }
    }

    /// Creates a user and returns the one-time temporary password.
    ///
    /// # Errors
    ///
    /// Returns not-authorized unless server-loaded roles grant
    /// `admin.users.create`, or returns a redacted domain/port error.
    pub async fn create_user(
        &self,
        command: CreateUserCommand,
        actor_roles: &[Role],
    ) -> Result<(User, SecretString), ApplicationError> {
        self.create_user_with_source_ip(command, actor_roles, None)
            .await
    }

    /// Creates a user and records the trusted-proxy-filtered source IP.
    pub async fn create_user_with_source_ip(
        &self,
        command: CreateUserCommand,
        actor_roles: &[Role],
        source_ip: Option<String>,
    ) -> Result<(User, SecretString), ApplicationError> {
        require(actor_roles, "admin.users.create")?;
        let now = self.clock.now();
        let temporary_password = generate_temporary_password();
        let password_hash = self
            .passwords
            .hash(temporary_password.clone())
            .await
            .map_err(|_| ApplicationError::port_failure("password hashing unavailable"))?;
        let user = User::new(
            UserId::generate(),
            command.login_id,
            command.display_name,
            now,
        )?;
        let credential = Credential::new(user.id(), password_hash, now);
        let audit = IdentityAuditEvent {
            occurred_at: now,
            actor_id: Some(command.actor_id),
            action: "user.created",
            resource_id: Some(user.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .apply_with_event_and_source_ip(
                IdentityMutation::CreateUser {
                    user: user.clone(),
                    credential,
                },
                audit,
                ExtensionEvent::UserCreated { user_id: user.id() },
                source_ip,
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        Ok((user, temporary_password))
    }

    /// Creates a role with the exact server-validated permission set.
    ///
    /// # Errors
    ///
    /// Returns not-authorized unless server-loaded roles grant
    /// `admin.roles.create`, or returns a redacted domain/port error.
    pub async fn create_role(
        &self,
        command: CreateRoleCommand,
        actor_roles: &[Role],
    ) -> Result<Role, ApplicationError> {
        self.create_role_with_source_ip(command, actor_roles, None)
            .await
    }

    /// Creates a role and records the trusted-proxy-filtered source IP.
    pub async fn create_role_with_source_ip(
        &self,
        command: CreateRoleCommand,
        actor_roles: &[Role],
        source_ip: Option<String>,
    ) -> Result<Role, ApplicationError> {
        require(actor_roles, "admin.roles.create")?;
        let role = Role::new(
            RoleId::generate(),
            command.name,
            command.description,
            command.permissions,
        )?;
        let audit = IdentityAuditEvent {
            occurred_at: self.clock.now(),
            actor_id: Some(command.actor_id),
            action: "role.created",
            resource_id: Some(role.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .apply_with_source_ip(
                IdentityMutation::StoreRole { role: role.clone() },
                audit,
                source_ip,
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        Ok(role)
    }

    /// Updates a role (merge-patch semantics) after an optimistic-concurrency
    /// check against `current`'s revision.
    ///
    /// `current` must be the caller's just-fetched, up-to-date role (loaded
    /// via `IdentityQueryPort::role` and reconstituted); this service does
    /// not itself have read access to the identity store. Unlike
    /// `create_role`'s blind `StoreRole` upsert, `IdentityMutation::UpdateRole`
    /// is rejected by the storage adapter when the row's current revision no
    /// longer matches, so the check is not lost to a race between the read
    /// and the write.
    ///
    /// # Errors
    ///
    /// Returns not-authorized unless server-loaded roles grant
    /// `admin.roles.update`, `Conflict` on revision mismatch (including a
    /// concurrent update that raced this one), `NotFound` when the role was
    /// deleted concurrently, or a redacted domain/port error.
    pub async fn update_role(
        &self,
        current: Role,
        command: UpdateRoleCommand,
        actor_roles: &[Role],
    ) -> Result<Role, ApplicationError> {
        self.update_role_with_source_ip(current, command, actor_roles, None)
            .await
    }

    /// Updates a role and records the trusted-proxy-filtered source IP.
    pub async fn update_role_with_source_ip(
        &self,
        mut current: Role,
        command: UpdateRoleCommand,
        actor_roles: &[Role],
        source_ip: Option<String>,
    ) -> Result<Role, ApplicationError> {
        require(actor_roles, "admin.roles.update")?;
        current
            .update(
                Revision::from_u64(command.expected_revision),
                command.name,
                command.description,
                command.permissions,
            )
            .map_err(|error| match error.kind() {
                DomainErrorKind::RevisionMismatch => {
                    ApplicationError::new(ApplicationErrorKind::Conflict, error.to_string())
                }
                _ => ApplicationError::new(ApplicationErrorKind::DomainRule, error.to_string()),
            })?;
        let audit = IdentityAuditEvent {
            occurred_at: self.clock.now(),
            actor_id: Some(command.actor_id),
            action: "role.updated",
            resource_id: Some(current.id().to_string()),
            request_id: command.request_id,
            succeeded: true,
        };
        self.store
            .apply_with_source_ip(
                IdentityMutation::UpdateRole {
                    role: current.clone(),
                },
                audit,
                source_ip,
            )
            .await
            .map_err(|err| match err {
                orbisync_application::IdentityPortError::NotFound => {
                    ApplicationError::new(ApplicationErrorKind::NotFound, "role not found")
                }
                orbisync_application::IdentityPortError::Conflict => {
                    ApplicationError::new(ApplicationErrorKind::Conflict, "revision mismatch")
                }
                orbisync_application::IdentityPortError::InvalidRequest => {
                    ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid request")
                }
                _ => ApplicationError::port_failure("identity administration unavailable"),
            })?;
        Ok(current)
    }

    /// Enables or disables a user and revokes active sessions on disable.
    ///
    /// # Errors
    ///
    /// Returns not-authorized, domain, hashing, or atomic persistence failure.
    pub async fn set_user_enabled(
        &self,
        user: &mut User,
        enabled: bool,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
    ) -> Result<(), ApplicationError> {
        require(actor_roles, "admin.users.status")?;
        let now = self.clock.now();
        let mut updated = user.clone();
        let changed = if enabled {
            updated.enable(now)?
        } else {
            updated.disable(now)?
        };
        if !changed {
            return Ok(());
        }
        let audit = IdentityAuditEvent {
            occurred_at: now,
            actor_id: Some(actor_id),
            action: if enabled {
                "user.enabled"
            } else {
                "user.disabled"
            },
            resource_id: Some(updated.id().to_string()),
            request_id,
            succeeded: true,
        };
        let event = (!enabled).then_some(ExtensionEvent::UserDisabled {
            user_id: updated.id(),
        });
        let result = if let Some(event) = event {
            self.store.apply_with_event(
                IdentityMutation::StoreUserStatus {
                    user: updated.clone(),
                },
                audit,
                event,
            )
        } else {
            self.store.apply(
                IdentityMutation::StoreUserStatus {
                    user: updated.clone(),
                },
                audit,
            )
        };
        result
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        *user = updated;
        Ok(())
    }

    /// Applies a `PATCH /v1/users/{user_id}` update under an `If-Match`
    /// optimistic lock.
    ///
    /// `display_name` requires `admin.users.update`; `enabled` requires
    /// `admin.users.status` (checked only when that field is present, so a
    /// display-name-only patch does not need the status permission). Both
    /// fields are applied through `User::apply_admin_patch` so the revision
    /// advances at most once even when both are supplied in the same call.
    ///
    /// # Errors
    ///
    /// Returns not-authorized for a missing permission, conflict for a
    /// revision mismatch (stale `If-Match` or a concurrent update), a
    /// domain error for an invalid `display_name`, or a port failure.
    pub async fn update_user(
        &self,
        user: &mut User,
        display_name: Option<String>,
        enabled: Option<bool>,
        expected_revision: u64,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
    ) -> Result<(), ApplicationError> {
        if display_name.is_some() {
            require(actor_roles, "admin.users.update")?;
        }
        if enabled.is_some() {
            require(actor_roles, "admin.users.status")?;
        }
        if user.revision().as_u64() != expected_revision {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "revision mismatch",
            ));
        }
        let now = self.clock.now();
        let mut updated = user.clone();
        let changed = updated.apply_admin_patch(display_name, enabled, now)?;
        if !changed {
            return Ok(());
        }
        let audit = IdentityAuditEvent {
            occurred_at: now,
            actor_id: Some(actor_id),
            action: "user.updated",
            resource_id: Some(updated.id().to_string()),
            request_id,
            succeeded: true,
        };
        let event = matches!(updated.status(), orbisync_domain::UserStatus::Disabled).then_some(
            ExtensionEvent::UserDisabled {
                user_id: updated.id(),
            },
        );
        let mutation = IdentityMutation::UpdateUserProfile {
            user: updated.clone(),
            expected_revision,
        };
        let result = if let Some(event) = event {
            self.store.apply_with_event(mutation, audit, event)
        } else {
            self.store.apply(mutation, audit)
        };
        result.await.map_err(|err| match err {
            orbisync_application::IdentityPortError::Conflict => {
                ApplicationError::new(ApplicationErrorKind::Conflict, "revision mismatch")
            }
            _ => ApplicationError::port_failure("identity administration unavailable"),
        })?;
        *user = updated;
        Ok(())
    }

    /// Resets a credential to a server-generated temporary password.
    ///
    /// # 失敗回数とロック状態の判断
    ///
    /// リセットで `failed_login_count` と `locked_until` をクリアするのが妥当と判断した。
    /// 理由: パスワードリセットは管理者による正規の回復経路であり、ブルートフォースで
    /// ロックされた状態でも運用者が復旧できる必要がある。維持すると一時パスワードでも
    /// ログインできず回復経路が機能しない。`Credential::replace_password` が内部で
    /// `record_success()` により失敗回数とロックをクリアするため、この設計と一致する。
    ///
    /// # 自分自身への実行の判断
    ///
    /// 自分自身に対する実行を**許す**と判断した。理由: 管理者自身がパスワードを忘れた場合や
    /// 漏洩が疑われる場合に、自身のアカウントをリセットできないと回復経路が失われる。
    /// 不正な自己リセットの懸念は監査 (`actor_id` と `resource_id` の両方を記録) と
    /// `admin.users.credentials.reset` 権限の付与管理でカバーできる。禁止すると運用上の
    /// デッドロックを生むため、許可が妥当である。
    ///
    /// # Errors
    ///
    /// Returns not-authorized, hashing, or atomic persistence failure.
    pub async fn reset_password(
        &self,
        user: &mut User,
        credential: &mut Credential,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
    ) -> Result<SecretString, ApplicationError> {
        // 権限は既存の管理者権限名に揃える: admin.users.credentials.reset
        require(actor_roles, "admin.users.credentials.reset")?;
        self.reset_password_record(
            user,
            credential,
            Some(actor_id),
            "password.reset",
            request_id,
        )
        .await
    }

    /// Recovers an existing active local administrator using installation/DB operator access.
    /// Only the local CLI may expose this use case; it does not authenticate a remote caller.
    /// Administrator eligibility means holding `admin.users.credentials.reset`.
    /// No account, role assignment, or account status is created or changed.
    pub async fn recover_local_administrator(
        &self,
        repository: &dyn orbisync_application::IdentityRepository,
        login_id: &orbisync_domain::LoginId,
        request_id: orbisync_application::RequestId,
    ) -> Result<SecretString, ApplicationError> {
        let mut account = repository
            .find_login(login_id)
            .await
            .map_err(|_| ApplicationError::port_failure("administrator lookup unavailable"))?
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorKind::NotFound,
                    "existing local administrator not found",
                )
            })?;
        let roles = repository
            .roles_for_user(account.user.id())
            .await
            .map_err(|_| ApplicationError::port_failure("administrator roles unavailable"))?;
        require(&roles, "admin.users.credentials.reset")?;
        if account.user.status() != orbisync_domain::UserStatus::Active {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "administrator is not active",
            ));
        }
        self.reset_password_record(
            &mut account.user,
            &mut account.credential,
            None,
            "administrator.password_recovered",
            request_id,
        )
        .await
    }

    async fn reset_password_record(
        &self,
        user: &mut User,
        credential: &mut Credential,
        actor_id: Option<UserId>,
        action: &'static str,
        request_id: orbisync_application::RequestId,
    ) -> Result<SecretString, ApplicationError> {
        if user.id() != credential.user_id() {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "credential does not belong to user",
            ));
        }
        let temporary = generate_temporary_password();
        let hash = self
            .passwords
            .hash(temporary.clone())
            .await
            .map_err(|_| ApplicationError::port_failure("password hashing unavailable"))?;
        let now = self.clock.now();
        let mut updated_user = user.clone();
        updated_user.require_password_change(now)?;
        let mut updated_credential = credential.clone();
        // Credential::replace_password で置き換え、`must_change_password` を true にする
        // と同時に失敗回数とロック状態をクリアする (record_success 内部呼び出し)。
        updated_credential.replace_password(hash, now);
        self.store
            .apply(
                IdentityMutation::ResetPassword {
                    user: updated_user.clone(),
                    credential: updated_credential.clone(),
                },
                IdentityAuditEvent {
                    occurred_at: now,
                    actor_id,
                    action,
                    resource_id: Some(credential.user_id().to_string()),
                    request_id,
                    succeeded: true,
                },
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        *user = updated_user;
        *credential = updated_credential;
        Ok(temporary)
    }

    /// Replaces a user's role assignments using server-validated role IDs.
    ///
    /// # Errors
    ///
    /// Returns not-authorized or atomic persistence failure.
    pub async fn assign_roles(
        &self,
        user_id: UserId,
        role_ids: impl IntoIterator<Item = RoleId>,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
    ) -> Result<(), ApplicationError> {
        self.assign_roles_with_source_ip(user_id, role_ids, actor_id, request_id, actor_roles, None)
            .await
    }

    /// Replaces role assignments and records the trusted-proxy-filtered source IP.
    pub async fn assign_roles_with_source_ip(
        &self,
        user_id: UserId,
        role_ids: impl IntoIterator<Item = RoleId>,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        require(actor_roles, "admin.roles.assign")?;
        self.store
            .apply_with_source_ip(
                IdentityMutation::ReplaceUserRoles {
                    user_id,
                    role_ids: role_ids.into_iter().collect(),
                },
                IdentityAuditEvent {
                    occurred_at: self.clock.now(),
                    actor_id: Some(actor_id),
                    action: "role.assigned",
                    resource_id: Some(user_id.to_string()),
                    request_id,
                    succeeded: true,
                },
                source_ip,
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))
    }

    /// Deletes a role and its assignments atomically with audit.
    ///
    /// # Assigned-role decision (W-G)
    ///
    /// When the role is still assigned to users, this method **cascades** the delete
    /// by removing `user_roles` rows in the same transaction (see
    /// `IdentityMutation::DeleteRole` documentation). Rationale is identical:
    /// `admin.roles.delete` is privileged, historic storage always cascaded, and
    /// rejecting would require an extra enumeration step for the caller.
    ///
    /// # Optimistic lock
    ///
    /// `expected_revision` must equal the stored `roles.revision`; a mismatch is
    /// mapped to `409 RESOURCE_CONFLICT` (not `412 REVISION_MISMATCH`).
    /// `openapi/errors.yaml` defines both `RESOURCE_CONFLICT` (409) and
    /// `REVISION_MISMATCH` (412); the task requires `409 CONFLICT` for this
    /// endpoint, so `RESOURCE_CONFLICT` is chosen. The reason is documented here
    /// per W-G requirement to justify the error-code choice.
    ///
    /// # Errors
    ///
    /// Returns not-authorized, not-found, conflict (revision mismatch), or port
    /// failure.
    pub async fn delete_role(
        &self,
        role_id: RoleId,
        expected_revision: u64,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
    ) -> Result<(), ApplicationError> {
        self.delete_role_with_source_ip(
            role_id,
            expected_revision,
            actor_id,
            request_id,
            actor_roles,
            None,
        )
        .await
    }

    /// Deletes a role and records the trusted-proxy-filtered source IP.
    pub async fn delete_role_with_source_ip(
        &self,
        role_id: RoleId,
        expected_revision: u64,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
        source_ip: Option<String>,
    ) -> Result<(), ApplicationError> {
        require(actor_roles, "admin.roles.delete")?;
        let audit = IdentityAuditEvent {
            occurred_at: self.clock.now(),
            actor_id: Some(actor_id),
            action: "role.deleted",
            resource_id: Some(role_id.to_string()),
            request_id,
            succeeded: true,
        };
        self.store
            .apply_with_source_ip(
                IdentityMutation::DeleteRole {
                    role_id,
                    expected_revision,
                },
                audit,
                source_ip,
            )
            .await
            .map_err(|err| match err {
                orbisync_application::IdentityPortError::NotFound => {
                    ApplicationError::new(ApplicationErrorKind::NotFound, "role not found")
                }
                orbisync_application::IdentityPortError::Conflict => {
                    ApplicationError::new(ApplicationErrorKind::Conflict, "revision mismatch")
                }
                orbisync_application::IdentityPortError::InvalidRequest => {
                    ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid request")
                }
                _ => ApplicationError::port_failure("identity administration unavailable"),
            })
    }

    /// Revokes a session for logout with the audit record in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns an atomic persistence failure.
    pub async fn logout(
        &self,
        session: &mut AuthSession,
        request_id: orbisync_application::RequestId,
    ) -> Result<(), ApplicationError> {
        let now = self.clock.now();
        let mut updated = session.clone();
        if !updated.revoke(now) {
            return Ok(());
        }
        self.store
            .apply(
                IdentityMutation::StoreSession {
                    session: updated.clone(),
                },
                IdentityAuditEvent {
                    occurred_at: now,
                    actor_id: Some(session.user_id()),
                    action: "logout",
                    resource_id: Some(session.id().to_string()),
                    request_id,
                    succeeded: true,
                },
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        *session = updated;
        Ok(())
    }

    /// Changes the authenticated user's password and clears forced-change state.
    ///
    /// Verifies `current_password` against the stored hash before hashing the
    /// new password (OpenAPI `ChangePasswordRequest` requires `current_password`).
    /// A mismatch returns `Unauthenticated` and records a `password.changed`
    /// failure audit (no secret material) so that token theft cannot silently
    /// replace the password. The new password is validated against the current
    /// `PasswordService` policy before hashing. On success the user
    /// `must_change_password` flag is cleared, `revision`/`updated_at` are
    /// advanced, credential fields are replaced and failure counters reset,
    /// and active sessions are revoked (except the current one in the HTTP
    /// layer) — all in a single `StorePasswordChange` transaction (ADR-017).
    ///
    /// # Errors
    ///
    /// Returns `Unauthenticated` for wrong current password, `DomainRule` for
    /// policy violation, `RateLimited` for capacity, or port failure. A
    /// mismatched user/credential pair yields `Conflict`.
    pub async fn change_password(
        &self,
        user: &mut User,
        credential: &mut Credential,
        current_password: SecretString,
        new_password: SecretString,
        request_id: orbisync_application::RequestId,
    ) -> Result<(), ApplicationError> {
        if user.id() != credential.user_id() {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "credential does not belong to user",
            ));
        }
        let now = self.clock.now();
        let verified = self
            .passwords
            .verify(current_password, credential.password_hash().clone())
            .await
            .map_err(|error| match error {
                crate::PasswordError::RateLimited => ApplicationError::new(
                    ApplicationErrorKind::RateLimited,
                    "password hashing capacity exhausted",
                ),
                crate::PasswordError::InvalidHash
                | crate::PasswordError::Unavailable
                | crate::PasswordError::PolicyViolation => {
                    ApplicationError::port_failure("password verification unavailable")
                }
            })?;
        if !verified {
            // Failure audit (result=failure) – no state change.
            // Failure is observable via `tracing::warn!` (A-3 fix for C4:
            // previously `let _ =` silently discarded the error with a false
            // claim that "storage logs"; now application logs and storage also
            // logs). Primary result is Unauthenticated and must not be masked.
            if let Err(err) = self
                .store
                .apply(
                    IdentityMutation::PasswordChangeRejected { user_id: user.id() },
                    IdentityAuditEvent {
                        occurred_at: now,
                        actor_id: Some(user.id()),
                        action: "password.changed",
                        resource_id: Some(user.id().to_string()),
                        request_id: request_id.clone(),
                        succeeded: false,
                    },
                )
                .await
            {
                tracing::warn!(
                    error = %err,
                    user_id = %user.id(),
                    "failed to persist password change rejected audit (best-effort, primary error is Unauthenticated)"
                );
            }
            return Err(ApplicationError::new(
                ApplicationErrorKind::Unauthenticated,
                "current password is incorrect",
            ));
        }
        let hash = self
            .passwords
            .hash(new_password)
            .await
            .map_err(|error| match error {
                crate::PasswordError::PolicyViolation => ApplicationError::new(
                    ApplicationErrorKind::DomainRule,
                    "password policy rejected the candidate",
                ),
                crate::PasswordError::RateLimited => ApplicationError::new(
                    ApplicationErrorKind::RateLimited,
                    "password hashing capacity exhausted",
                ),
                crate::PasswordError::Unavailable | crate::PasswordError::InvalidHash => {
                    ApplicationError::port_failure("password hashing unavailable")
                }
            })?;
        let mut updated_user = user.clone();
        updated_user.complete_password_change(now)?;
        let mut updated_credential = credential.clone();
        updated_credential.replace_password(hash, now);
        self.store
            .apply(
                IdentityMutation::StorePasswordChange {
                    user: updated_user.clone(),
                    credential: updated_credential.clone(),
                },
                IdentityAuditEvent {
                    occurred_at: now,
                    actor_id: Some(user.id()),
                    action: "password.changed",
                    resource_id: Some(user.id().to_string()),
                    request_id,
                    succeeded: true,
                },
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        *user = updated_user;
        *credential = updated_credential;
        Ok(())
    }

    /// Atomically resets a password and completes idempotency in one transaction (AUD-C2).
    ///
    /// This is the idempotent variant of `reset_password` that combines the
    /// `ResetPassword` mutation, its audit event, and the idempotency
    /// completion into a single PostgreSQL transaction. If the idempotency
    /// update affects 0 rows, the whole transaction is rolled back and a
    /// port failure is returned so the HTTP layer can return 5xx instead of
    /// 202 with a lost temporary password.
    pub async fn reset_password_with_idempotency(
        &self,
        user: &mut User,
        credential: &mut Credential,
        actor_id: UserId,
        request_id: orbisync_application::RequestId,
        actor_roles: &[Role],
        idempotency_key: String,
        idempotency_owner: String,
    ) -> Result<SecretString, ApplicationError> {
        self.reset_password_with_idempotency_and_source_ip(
            user,
            credential,
            actor_id,
            actor_roles,
            PasswordIdempotencyContext::new(request_id, idempotency_key, idempotency_owner, None),
        )
        .await
    }

    /// Atomically resets a password while recording the trusted-proxy-filtered source IP.
    pub async fn reset_password_with_idempotency_and_source_ip(
        &self,
        user: &mut User,
        credential: &mut Credential,
        actor_id: UserId,
        actor_roles: &[Role],
        context: PasswordIdempotencyContext,
    ) -> Result<SecretString, ApplicationError> {
        let PasswordIdempotencyContext {
            request_id,
            idempotency_key,
            idempotency_owner,
            source_ip,
        } = context;
        require(actor_roles, "admin.users.credentials.reset")?;
        if user.id() != credential.user_id() {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "credential does not belong to user",
            ));
        }
        let temporary = generate_temporary_password();
        let hash = self
            .passwords
            .hash(temporary.clone())
            .await
            .map_err(|_| ApplicationError::port_failure("password hashing unavailable"))?;
        let now = self.clock.now();
        let mut updated_user = user.clone();
        updated_user.require_password_change(now)?;
        let mut updated_credential = credential.clone();
        updated_credential.replace_password(hash, now);
        // AUD-C1: do NOT persist temporary_password. The idempotency
        // completion stores only non-secret data (must_change_password).
        // The temporary password is returned only in the first 202 response
        // and is never written to response_body BYTEA.
        let body = serde_json::json!({
            "must_change_password": true
        });
        let json_bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
        let completion = IdempotencyCompletion {
            status_code: 202,
            response_content_type: "application/json".to_owned(),
            response: EncryptedResponse::new(json_bytes),
        };
        let audit = IdentityAuditEvent {
            occurred_at: now,
            actor_id: Some(actor_id),
            action: "password.reset",
            resource_id: Some(credential.user_id().to_string()),
            request_id,
            succeeded: true,
        };
        self.store
            .apply_with_idempotency_and_source_ip(
                IdentityMutation::ResetPassword {
                    user: updated_user.clone(),
                    credential: updated_credential.clone(),
                },
                audit,
                idempotency_key,
                idempotency_owner,
                completion,
                source_ip,
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        *user = updated_user;
        *credential = updated_credential;
        Ok(temporary)
    }

    /// Atomically changes a password and completes idempotency in one transaction (AUD-C2).
    pub async fn change_password_with_idempotency(
        &self,
        user: &mut User,
        credential: &mut Credential,
        current_password: SecretString,
        new_password: SecretString,
        request_id: orbisync_application::RequestId,
        idempotency_key: String,
        idempotency_owner: String,
    ) -> Result<(), ApplicationError> {
        self.change_password_with_idempotency_and_source_ip(
            user,
            credential,
            current_password,
            new_password,
            PasswordIdempotencyContext::new(request_id, idempotency_key, idempotency_owner, None),
        )
        .await
    }

    /// Atomically changes a password while recording the trusted-proxy-filtered source IP.
    pub async fn change_password_with_idempotency_and_source_ip(
        &self,
        user: &mut User,
        credential: &mut Credential,
        current_password: SecretString,
        new_password: SecretString,
        context: PasswordIdempotencyContext,
    ) -> Result<(), ApplicationError> {
        let PasswordIdempotencyContext {
            request_id,
            idempotency_key,
            idempotency_owner,
            source_ip,
        } = context;
        if user.id() != credential.user_id() {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "credential does not belong to user",
            ));
        }
        let now = self.clock.now();
        let verified = self
            .passwords
            .verify(current_password, credential.password_hash().clone())
            .await
            .map_err(|error| match error {
                crate::PasswordError::RateLimited => ApplicationError::new(
                    ApplicationErrorKind::RateLimited,
                    "password hashing capacity exhausted",
                ),
                crate::PasswordError::InvalidHash
                | crate::PasswordError::Unavailable
                | crate::PasswordError::PolicyViolation => {
                    ApplicationError::port_failure("password verification unavailable")
                }
            })?;
        if !verified {
            // Same as above – failure audit is best-effort; observable via `tracing::warn!` (A-3 fix for C4).
            if let Err(err) = self
                .store
                .apply_with_source_ip(
                    IdentityMutation::PasswordChangeRejected { user_id: user.id() },
                    IdentityAuditEvent {
                        occurred_at: now,
                        actor_id: Some(user.id()),
                        action: "password.changed",
                        resource_id: Some(user.id().to_string()),
                        request_id,
                        succeeded: false,
                    },
                    source_ip.clone(),
                )
                .await
            {
                tracing::warn!(
                    error = %err,
                    user_id = %user.id(),
                    "failed to persist password change rejected audit (best-effort, primary error is Unauthenticated)"
                );
            }
            return Err(ApplicationError::new(
                ApplicationErrorKind::Unauthenticated,
                "current password is incorrect",
            ));
        }
        let hash = self
            .passwords
            .hash(new_password)
            .await
            .map_err(|error| match error {
                crate::PasswordError::PolicyViolation => ApplicationError::new(
                    ApplicationErrorKind::DomainRule,
                    "password policy rejected the candidate",
                ),
                crate::PasswordError::RateLimited => ApplicationError::new(
                    ApplicationErrorKind::RateLimited,
                    "password hashing capacity exhausted",
                ),
                crate::PasswordError::Unavailable | crate::PasswordError::InvalidHash => {
                    ApplicationError::port_failure("password hashing unavailable")
                }
            })?;
        let mut updated_user = user.clone();
        updated_user.complete_password_change(now)?;
        let mut updated_credential = credential.clone();
        updated_credential.replace_password(hash, now);
        let completion = IdempotencyCompletion {
            status_code: 204,
            response_content_type: "application/json".to_owned(),
            response: EncryptedResponse::new(Vec::new()),
        };
        let audit = IdentityAuditEvent {
            occurred_at: now,
            actor_id: Some(user.id()),
            action: "password.changed",
            resource_id: Some(user.id().to_string()),
            request_id,
            succeeded: true,
        };
        self.store
            .apply_with_idempotency_and_source_ip(
                IdentityMutation::StorePasswordChange {
                    user: updated_user.clone(),
                    credential: updated_credential.clone(),
                },
                audit,
                idempotency_key,
                idempotency_owner,
                completion,
                source_ip,
            )
            .await
            .map_err(|_| ApplicationError::port_failure("identity administration unavailable"))?;
        *user = updated_user;
        *credential = updated_credential;
        Ok(())
    }
}

fn require(roles: &[Role], permission: &str) -> Result<(), ApplicationError> {
    let permission = Permission::new(permission)?;
    if RbacAuthorizer::authorize(roles, &permission) == AuthorizationDecision::Allow {
        Ok(())
    } else {
        Err(ApplicationError::new(
            ApplicationErrorKind::NotAuthorized,
            "permission denied",
        ))
    }
}

fn generate_temporary_password() -> SecretString {
    let mut bytes = [0_u8; 20];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    SecretString::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Newtype delegating [`IdentityAdministrationStore`] via `Arc<dyn IdentityAdministrationStore>`.
///
/// `HttpState` holds `Arc<dyn IdentityAdministrationStore>` but
/// `IdentityAdministrationService<S, C>` is generic. This wrapper implements
/// the trait for the `Arc` so the service can be built as
/// `IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>` (D-8).
#[derive(Clone)]
pub struct DynIdentityAdministrationStore(pub Arc<dyn IdentityAdministrationStore>);

impl core::fmt::Debug for DynIdentityAdministrationStore {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("DynIdentityAdministrationStore")
    }
}

#[async_trait::async_trait]
impl IdentityAdministrationStore for DynIdentityAdministrationStore {
    async fn apply(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
    ) -> Result<(), orbisync_application::IdentityPortError> {
        self.0.apply(mutation, audit).await
    }

    async fn apply_with_event(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        event: ExtensionEvent,
    ) -> Result<(), orbisync_application::IdentityPortError> {
        self.0.apply_with_event(mutation, audit, event).await
    }

    async fn apply_with_idempotency(
        &self,
        mutation: IdentityMutation,
        audit: IdentityAuditEvent,
        idempotency_key: String,
        idempotency_owner: String,
        completion: IdempotencyCompletion,
    ) -> Result<(), orbisync_application::IdentityPortError> {
        self.0
            .apply_with_idempotency(
                mutation,
                audit,
                idempotency_key,
                idempotency_owner,
                completion,
            )
            .await
    }
}

/// Newtype delegating [`Clock`] via `Arc<dyn Clock>` for the same D-8 purpose.
#[derive(Clone)]
pub struct DynClock(pub Arc<dyn Clock>);

impl core::fmt::Debug for DynClock {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("DynClock")
    }
}

impl Clock for DynClock {
    fn now(&self) -> orbisync_domain::Timestamp {
        self.0.now()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use orbisync_application::{
        CreateUserCommand, IdentityAdministrationStore, IdentityAuditEvent, IdentityMutation,
        IdentityPortError, RequestId,
    };
    use orbisync_domain::{Clock, LoginId, Permission, Role, RoleId, Timestamp, UserId};

    use super::IdentityAdministrationService;
    use crate::{PasswordPolicy, PasswordService};

    #[derive(Debug)]
    struct TestClock(Timestamp);

    impl Clock for TestClock {
        fn now(&self) -> Timestamp {
            self.0
        }
    }

    #[derive(Debug, Default)]
    struct Store(Mutex<Vec<(IdentityMutation, IdentityAuditEvent)>>);

    #[async_trait]
    impl IdentityAdministrationStore for Store {
        async fn apply(
            &self,
            mutation: IdentityMutation,
            audit: IdentityAuditEvent,
        ) -> Result<(), IdentityPortError> {
            self.0.lock().expect("test lock").push((mutation, audit));
            Ok(())
        }
    }

    #[test]
    fn create_user_requires_server_loaded_permission_and_audits_atomically() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let now = Timestamp::from_unix_millis(1_000).expect("time");
            let store = Arc::new(Store::default());
            let service = IdentityAdministrationService::new(
                store.clone(),
                Arc::new(TestClock(now)),
                PasswordService::new(PasswordPolicy::new(Vec::new())).expect("password service"),
            );
            let actor = UserId::generate();
            let command = CreateUserCommand {
                login_id: LoginId::new("ada").expect("login"),
                display_name: String::from("Ada"),
                actor_id: actor,
                request_id: RequestId::new(format!("req_{}", UserId::generate()))
                    .expect("request id"),
            };
            assert!(service.create_user(command.clone(), &[]).await.is_err());
            let admin = Role::new(
                RoleId::generate(),
                "User creator",
                None,
                [Permission::new("admin.users.create").expect("permission")],
            )
            .expect("role");
            let (user, temporary) = service
                .create_user(command, &[admin])
                .await
                .expect("authorized creation");
            assert!(user.must_change_password());
            assert_eq!(temporary.expose_secret().chars().count(), 27);
            let captured = store.0.lock().expect("test lock");
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].1.action, "user.created");
            assert_eq!(captured[0].1.actor_id, Some(actor));
        });
    }

    #[test]
    fn bootstrap_creates_separate_administration_and_world_roles() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let now = Timestamp::from_unix_millis(1_000).expect("time");
            let store = Arc::new(Store::default());
            let service = IdentityAdministrationService::new(
                store.clone(),
                Arc::new(TestClock(now)),
                PasswordService::new(PasswordPolicy::new(Vec::new())).expect("password service"),
            );
            let login = LoginId::new("bootstrap").expect("login");
            let request_id =
                RequestId::new(format!("req_{}", UserId::generate())).expect("request");
            let (user, _) = service
                .bootstrap_administrator(login, "Bootstrap".to_owned(), request_id)
                .await
                .expect("bootstrap");
            let captured = store.0.lock().expect("test lock");
            let Some(entry) = captured.first() else {
                assert_eq!(
                    captured.len(),
                    1,
                    "bootstrap must persist the bootstrap mutation"
                );
                return;
            };
            let roles = if let IdentityMutation::BootstrapAdministrator { roles, .. } = &entry.0 {
                roles
            } else {
                assert!(
                    matches!(&entry.0, IdentityMutation::BootstrapAdministrator { .. }),
                    "bootstrap must persist the bootstrap mutation"
                );
                return;
            };
            assert_eq!(roles.len(), 2);
            assert_eq!(user.roles().len(), 2);
            assert_eq!(roles[0].name(), "Administrator");
            assert_eq!(roles[1].name(), "World Administrator");
            assert!(
                roles[0]
                    .permissions()
                    .iter()
                    .all(|permission| permission.as_str().starts_with("admin."))
            );
            assert!(
                roles[1]
                    .permissions()
                    .iter()
                    .all(|permission| !permission.as_str().starts_with("admin."))
            );
        });
    }
}
