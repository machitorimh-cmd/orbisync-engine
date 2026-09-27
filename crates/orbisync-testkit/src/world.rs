//! In-memory world directory port for tests without PostgreSQL.

use std::collections::HashMap;
use std::sync::Mutex;

use orbisync_application::{
    ApplicationError, ApplicationErrorKind, ExtensionEvent, Page, PageRequest, WorldAuditEvent,
    WorldAuthorizer, WorldDirectoryStore, WorldView,
};
use orbisync_domain::{InstanceId, World, WorldId, WorldInstance};

/// Authorizer that allows every request – for tests that don't exercise auth.
///
/// Production code must use a real authorizer such as `PgWorldAuthorizer`.
/// This is the testkit replacement for the former `application::AllowAllAuthorizer`
/// which is now `#[cfg(test)]` only.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllAuthorizer;

#[async_trait::async_trait]
impl WorldAuthorizer for AllowAllAuthorizer {
    async fn require(
        &self,
        _actor: orbisync_domain::UserId,
        _permission: &str,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }
}

/// Authorizer for realtime tests that exercise owner-only entity updates.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowEntityOwnerAuthorizer;

#[async_trait::async_trait]
impl WorldAuthorizer for AllowEntityOwnerAuthorizer {
    async fn require(
        &self,
        _actor: orbisync_domain::UserId,
        permission: &str,
    ) -> Result<(), ApplicationError> {
        match permission {
            // The fixture's administrator setup also exercises the identity
            // and world-directory permissions. Keep the owner-only negative
            // cases meaningful by continuing to reject entity.update.any and
            // all permissions outside this explicit test surface.
            "admin.users.read"
            | "admin.users.create"
            | "admin.users.update"
            | "admin.users.status"
            | "admin.users.import"
            | "admin.users.credentials.reset"
            | "admin.roles.read"
            | "admin.roles.create"
            | "admin.roles.update"
            | "admin.roles.delete"
            | "admin.roles.assign"
            | "admin.audit.read"
            | "admin.worlds.create"
            | "admin.worlds.read"
            | "admin.worlds.update"
            | "admin.worlds.archive"
            | "world.instance.create"
            | "entity.spawn"
            | "entity.update.own" => Ok(()),
            _ => Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "permission denied",
            )),
        }
    }
}

/// Authorizer that denies every request – mutation test helper.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllAuthorizer;

#[async_trait::async_trait]
impl WorldAuthorizer for DenyAllAuthorizer {
    async fn require(
        &self,
        _actor: orbisync_domain::UserId,
        _permission: &str,
    ) -> Result<(), ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorKind::NotAuthorized,
            "permission denied",
        ))
    }
}

/// In-memory implementation of [`WorldDirectoryStore`].
///
/// `create_world_with_audit` and `create_instance_with_audit` reject an id
/// that is already stored, because `world_definitions.id` and
/// `world_instances.id` are `PRIMARY KEY`
/// (`migrations/0004_world_directory.sql`) and the PostgreSQL adapter inserts
/// without `ON CONFLICT`, so a duplicate surfaces as
/// [`ApplicationErrorKind::PortFailure`]. Each `*_with_audit` is atomic:
/// if the audit is configured to fail, neither state nor audit is persisted.
#[derive(Debug, Default)]
pub struct FakeWorldDirectoryStore {
    worlds: Mutex<HashMap<WorldId, World>>,
    instances: Mutex<HashMap<InstanceId, WorldInstance>>,
    get_instance_failure: Mutex<Option<ApplicationError>>,
    audits: Mutex<Vec<WorldAuditEvent>>,
    fail_next_audit: Mutex<Option<ApplicationError>>,
    extension_events: Mutex<Vec<ExtensionEvent>>,
}

impl FakeWorldDirectoryStore {
    /// Creates an empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a world, bypassing the duplicate-id rule so a test
    /// can seed state directly.
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    pub fn insert_world(&self, world: World) {
        self.worlds
            .lock()
            .unwrap_or_else(|error| panic!("fake world lock poisoned: {error}"))
            .insert(world.id(), world);
    }

    /// Inserts or replaces an instance, bypassing the duplicate-id rule so a
    /// test can seed state directly.
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    pub fn insert_instance(&self, instance: WorldInstance) {
        self.instances
            .lock()
            .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"))
            .insert(instance.id(), instance);
    }

    /// Makes every following [`WorldDirectoryStore::get_instance`] call return
    /// the given error instead of a lookup result.
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    pub fn fail_get_instance_with(&self, error: ApplicationError) {
        let mut guard = self
            .get_instance_failure
            .lock()
            .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"));
        *guard = Some(error);
    }

    /// Makes the next audit insertion fail with the given error.
    ///
    /// When `create_world_with_audit` / `create_instance_with_audit` /
    /// `record_world_audit` is called next, it returns the error without
    /// persisting the state or the audit, modelling a rolled-back
    /// transaction. Subsequent calls succeed unless this is called again.
    pub fn fail_next_audit_with(&self, error: ApplicationError) {
        *self
            .fail_next_audit
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}")) = Some(error);
    }

    /// Returns a snapshot of all audit events recorded via the audited methods.
    pub fn audits(&self) -> Vec<WorldAuditEvent> {
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .clone()
    }

    /// Clears recorded audits (test helper).
    pub fn clear_audits(&self) {
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .clear();
    }

    /// Returns extension events emitted by lifecycle transitions.
    pub fn extension_events(&self) -> Vec<ExtensionEvent> {
        self.extension_events
            .lock()
            .unwrap_or_else(|error| panic!("fake extension event lock poisoned: {error}"))
            .clone()
    }
}

#[async_trait::async_trait]
impl WorldDirectoryStore for FakeWorldDirectoryStore {
    async fn create_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        // Atomic: if audit is configured to fail, neither world nor audit is persisted.
        if let Some(error) = self
            .fail_next_audit
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .take()
        {
            return Err(error);
        }
        {
            let mut worlds = self
                .worlds
                .lock()
                .unwrap_or_else(|error| panic!("fake world lock poisoned: {error}"));
            if worlds.contains_key(&world.id()) {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::PortFailure,
                    "world id already exists",
                ));
            }
            worlds.insert(world.id(), world);
        }
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .push(audit);
        Ok(())
    }

    async fn get_world(&self, id: WorldId) -> Result<Option<World>, ApplicationError> {
        Ok(self
            .worlds
            .lock()
            .unwrap_or_else(|error| panic!("fake world lock poisoned: {error}"))
            .get(&id)
            .cloned())
    }

    async fn list_worlds(&self, page: PageRequest) -> Result<Page<WorldView>, ApplicationError> {
        let mut views: Vec<WorldView> = self
            .worlds
            .lock()
            .unwrap_or_else(|error| panic!("fake world lock poisoned: {error}"))
            .values()
            .map(|world| WorldView {
                id: world.id(),
                name: world.name().to_owned(),
                status: world.status().as_str().to_owned(),
                revision: world.revision(),
            })
            .collect();
        views.sort_by_key(|view| view.id.to_string());
        let start = if let Some(after) = &page.after {
            views
                .iter()
                .position(|view| view.id.to_string() == after.0)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    ApplicationError::new(ApplicationErrorKind::DomainRule, "invalid cursor")
                })?
        } else {
            0
        };
        let limit = usize::try_from(page.limit.clamp(1, 200)).unwrap_or(0);
        let end = (start + limit).min(views.len());
        let items = views[start..end].to_vec();
        let next = if end < views.len() {
            items
                .last()
                .map(|item| orbisync_application::QueryCursor(item.id.to_string()))
        } else {
            None
        };
        Ok(Page { items, next })
    }

    async fn update_world_with_audit(
        &self,
        world: World,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        if let Some(error) = self
            .fail_next_audit
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .take()
        {
            return Err(error);
        }
        {
            let mut worlds = self
                .worlds
                .lock()
                .unwrap_or_else(|error| panic!("fake world lock poisoned: {error}"));
            if !worlds.contains_key(&world.id()) {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::NotFound,
                    "world not found",
                ));
            }
            worlds.insert(world.id(), world);
        }
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .push(audit);
        Ok(())
    }

    async fn create_instance_with_audit(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
    ) -> Result<(), ApplicationError> {
        if let Some(error) = self
            .fail_next_audit
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .take()
        {
            return Err(error);
        }
        {
            let mut instances = self
                .instances
                .lock()
                .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"));
            if instances.contains_key(&instance.id()) {
                return Err(ApplicationError::new(
                    ApplicationErrorKind::PortFailure,
                    "instance id already exists",
                ));
            }
            instances.insert(instance.id(), instance);
        }
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .push(audit);
        Ok(())
    }

    async fn record_world_audit(&self, audit: WorldAuditEvent) -> Result<(), ApplicationError> {
        if let Some(error) = self
            .fail_next_audit
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .take()
        {
            return Err(error);
        }
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .push(audit);
        Ok(())
    }

    async fn update_instance_with_event(
        &self,
        instance: WorldInstance,
        audit: WorldAuditEvent,
        event: ExtensionEvent,
    ) -> Result<(), ApplicationError> {
        if let Some(error) = self
            .fail_next_audit
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .take()
        {
            return Err(error);
        }
        self.instances
            .lock()
            .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"))
            .insert(instance.id(), instance);
        self.audits
            .lock()
            .unwrap_or_else(|error| panic!("fake audit lock poisoned: {error}"))
            .push(audit);
        self.extension_events
            .lock()
            .unwrap_or_else(|error| panic!("fake extension event lock poisoned: {error}"))
            .push(event);
        Ok(())
    }

    async fn get_instance(
        &self,
        id: InstanceId,
    ) -> Result<Option<WorldInstance>, ApplicationError> {
        if let Some(error) = self
            .get_instance_failure
            .lock()
            .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"))
            .clone()
        {
            return Err(error);
        }
        Ok(self
            .instances
            .lock()
            .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"))
            .get(&id)
            .cloned())
    }

    async fn list_instances(
        &self,
        page: PageRequest,
    ) -> Result<Page<orbisync_application::InstanceView>, ApplicationError> {
        let mut instances: Vec<WorldInstance> = self
            .instances
            .lock()
            .unwrap_or_else(|error| panic!("fake instance lock poisoned: {error}"))
            .values()
            .cloned()
            .collect();
        instances.sort_by_key(WorldInstance::id);
        let after = page.after.map(|c| c.0);
        let start = match after {
            Some(cursor) => instances
                .iter()
                .position(|i| i.id().to_string() > cursor)
                .unwrap_or(instances.len()),
            None => 0,
        };
        let limit = page.limit as usize;
        let remaining = &instances[start..];
        let items: Vec<orbisync_application::InstanceView> = remaining
            .iter()
            .take(limit)
            .map(|instance| orbisync_application::InstanceView {
                id: instance.id(),
                world_id: instance.world_id(),
                status: instance.lifecycle().as_str().to_owned(),
                revision: instance.revision(),
            })
            .collect();
        let next = if remaining.len() > limit {
            items
                .last()
                .map(|item| orbisync_application::QueryCursor(item.id.to_string()))
        } else {
            None
        };
        Ok(Page { items, next })
    }
}

/// In-memory implementation of [`orbisync_application::InstanceMembershipStore`].
///
/// Models the live runtime registry for tests that do not spin up
/// `orbisync-world-runtime`: a plain `instance_id -> Vec<UserId>` map.
#[derive(Debug, Default)]
pub struct FakeInstanceMembershipStore {
    members: Mutex<HashMap<InstanceId, Vec<orbisync_domain::UserId>>>,
}

impl FakeInstanceMembershipStore {
    /// Creates an empty membership store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds `instance_id` with the given live members (test helper).
    ///
    /// # Panics
    ///
    /// Panics if the test mutex is poisoned.
    pub fn seed(&self, instance_id: InstanceId, members: Vec<orbisync_domain::UserId>) {
        self.members
            .lock()
            .unwrap_or_else(|error| panic!("fake membership lock poisoned: {error}"))
            .insert(instance_id, members);
    }
}

#[async_trait::async_trait]
impl orbisync_application::InstanceMembershipStore for FakeInstanceMembershipStore {
    async fn list_members(
        &self,
        instance_id: InstanceId,
    ) -> Result<Vec<orbisync_domain::UserId>, ApplicationError> {
        Ok(self
            .members
            .lock()
            .unwrap_or_else(|error| panic!("fake membership lock poisoned: {error}"))
            .get(&instance_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn kick_member(
        &self,
        instance_id: InstanceId,
        user_id: orbisync_domain::UserId,
    ) -> Result<bool, ApplicationError> {
        let mut guard = self
            .members
            .lock()
            .unwrap_or_else(|error| panic!("fake membership lock poisoned: {error}"));
        let Some(members) = guard.get_mut(&instance_id) else {
            return Ok(false);
        };
        let before = members.len();
        members.retain(|id| *id != user_id);
        Ok(members.len() != before)
    }
}

#[cfg(test)]
mod tests {
    use orbisync_application::{
        ApplicationError, ApplicationErrorKind, RequestId, StartInstanceCommand,
        StopInstanceCommand, WorldAuditEvent, WorldDirectoryStore, WorldDirectoryUseCase,
    };
    use orbisync_domain::{
        InstanceId, Timestamp, UserId, World, WorldId, WorldInstance, transform::Transform,
    };

    use super::FakeWorldDirectoryStore;

    fn ts() -> Timestamp {
        Timestamp::from_unix_millis(1_785_481_200_000).expect("constant timestamp must be valid")
    }

    fn world() -> World {
        World::new(
            WorldId::generate(),
            "lobby".to_owned(),
            None,
            Transform::identity(),
            64,
            ts(),
        )
        .expect("valid world")
    }

    fn instance(world_id: WorldId, capacity: u32) -> WorldInstance {
        WorldInstance::new(InstanceId::generate(), world_id, capacity, ts())
            .expect("valid instance")
    }

    fn audit(action: &'static str) -> WorldAuditEvent {
        WorldAuditEvent {
            occurred_at: ts(),
            actor_id: UserId::generate(),
            action,
            resource_id: None,
            request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("req"),
            succeeded: true,
        }
    }

    const fn assert_implements_store<S: WorldDirectoryStore>() {}

    #[test]
    fn test_fake_world_directory_store_implements_the_port() {
        assert_implements_store::<FakeWorldDirectoryStore>();
    }

    #[test]
    fn test_inserted_instance_is_returned_with_its_fields() {
        let store = FakeWorldDirectoryStore::new();
        let world = world();
        let instance = instance(world.id(), 7);
        let id = instance.id();
        store.insert_world(world.clone());
        store.insert_instance(instance.clone());

        let (found_world, found) = pollster::block_on(async {
            (
                store.get_world(world.id()).await.expect("lookup succeeds"),
                store.get_instance(id).await.expect("lookup succeeds"),
            )
        });

        let found = found.expect("inserted instance is present");
        assert_eq!(found, instance);
        assert_eq!(found.capacity(), 7);
        assert_eq!(found.world_id(), world.id());
        assert_eq!(found_world.expect("inserted world is present"), world);
    }

    #[test]
    fn test_unknown_instance_id_is_not_found() {
        let store = FakeWorldDirectoryStore::new();
        store.insert_instance(instance(WorldId::generate(), 4));

        let found = pollster::block_on(store.get_instance(InstanceId::generate()))
            .expect("lookup succeeds");

        assert!(found.is_none());
    }

    #[test]
    fn test_create_rejects_an_id_that_already_exists() {
        let store = FakeWorldDirectoryStore::new();
        let w = world();
        let inst = instance(w.id(), 4);

        let (world_error, instance_error) = pollster::block_on(async {
            store
                .create_world_with_audit(
                    w.clone(),
                    audit(orbisync_application::AUDIT_ACTION_WORLD_CREATED),
                )
                .await
                .expect("first world insert succeeds");
            store
                .create_instance_with_audit(
                    inst.clone(),
                    audit(orbisync_application::AUDIT_ACTION_INSTANCE_CREATED),
                )
                .await
                .expect("first instance insert succeeds");
            (
                store
                    .create_world_with_audit(
                        w,
                        audit(orbisync_application::AUDIT_ACTION_WORLD_CREATED),
                    )
                    .await
                    .expect_err("duplicate world id must fail"),
                store
                    .create_instance_with_audit(
                        inst,
                        audit(orbisync_application::AUDIT_ACTION_INSTANCE_CREATED),
                    )
                    .await
                    .expect_err("duplicate instance id must fail"),
            )
        });

        assert_eq!(world_error.kind(), ApplicationErrorKind::PortFailure);
        assert_eq!(instance_error.kind(), ApplicationErrorKind::PortFailure);
    }

    #[test]
    fn test_get_instance_returns_the_injected_failure() {
        let store = FakeWorldDirectoryStore::new();
        let instance = instance(WorldId::generate(), 4);
        let id = instance.id();
        store.insert_instance(instance);
        store.fail_get_instance_with(ApplicationError::new(
            ApplicationErrorKind::PortFailure,
            "injected",
        ));

        let error = pollster::block_on(store.get_instance(id)).expect_err("injected failure");

        assert_eq!(error.kind(), ApplicationErrorKind::PortFailure);
    }

    #[test]
    fn test_atomic_rollback_on_audit_failure() {
        // Verifies that when audit fails, world/instance is not persisted (atomic).
        let store = FakeWorldDirectoryStore::new();
        let w = world();
        let wid = w.id();
        store.fail_next_audit_with(ApplicationError::new(
            ApplicationErrorKind::PortFailure,
            "audit injected failure",
        ));
        let err =
            pollster::block_on(store.create_world_with_audit(
                w,
                audit(orbisync_application::AUDIT_ACTION_WORLD_CREATED),
            ))
            .expect_err("audit failure");
        assert_eq!(err.kind(), ApplicationErrorKind::PortFailure);
        let found = pollster::block_on(store.get_world(wid)).expect("lookup");
        assert!(
            found.is_none(),
            "world must not be persisted when audit fails"
        );
        assert!(
            store.audits().is_empty(),
            "audit must not be persisted when it fails"
        );
    }

    #[test]
    fn lifecycle_use_case_emits_started_and_stopped_events() {
        let store = std::sync::Arc::new(FakeWorldDirectoryStore::new());
        let w = world();
        let instance = instance(w.id(), 4);
        let instance_id = instance.id();
        store.insert_instance(instance);
        let use_case = WorldDirectoryUseCase::new(store.clone(), super::AllowAllAuthorizer);
        let actor_id = UserId::generate();
        let started = pollster::block_on(use_case.start_instance(StartInstanceCommand {
            actor_id,
            instance_id,
            now: ts(),
            request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        }))
        .expect("start succeeds");
        assert_eq!(started.status, "running");
        let stopped = pollster::block_on(use_case.stop_instance(StopInstanceCommand {
            actor_id,
            instance_id,
            now: ts(),
            request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        }))
        .expect("stop succeeds");
        assert_eq!(stopped.status, "stopping");
        assert_eq!(
            store
                .extension_events()
                .iter()
                .map(orbisync_application::ExtensionEvent::kind)
                .collect::<Vec<_>>(),
            vec!["instance.started", "instance.stopped"]
        );
    }

    #[test]
    fn start_and_stop_instance_reject_unauthorized_actor_with_audit() {
        let store = std::sync::Arc::new(FakeWorldDirectoryStore::new());
        let w = world();
        let instance = instance(w.id(), 4);
        let instance_id = instance.id();
        store.insert_instance(instance);
        let use_case = WorldDirectoryUseCase::new(store.clone(), super::DenyAllAuthorizer);
        let actor_id = UserId::generate();

        let start_err = pollster::block_on(use_case.start_instance(StartInstanceCommand {
            actor_id,
            instance_id,
            now: ts(),
            request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        }))
        .expect_err("deny-all authorizer must reject start");
        assert_eq!(start_err.kind(), ApplicationErrorKind::NotAuthorized);

        let stop_err = pollster::block_on(use_case.stop_instance(StopInstanceCommand {
            actor_id,
            instance_id,
            now: ts(),
            request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        }))
        .expect_err("deny-all authorizer must reject stop");
        assert_eq!(stop_err.kind(), ApplicationErrorKind::NotAuthorized);

        let audits = store.audits();
        assert_eq!(audits.len(), 2, "both failures must be audited");
        assert!(audits.iter().all(|a| !a.succeeded));
        assert_eq!(
            audits.iter().map(|a| a.action).collect::<Vec<_>>(),
            vec![
                orbisync_application::AUDIT_ACTION_INSTANCE_STARTED,
                orbisync_application::AUDIT_ACTION_INSTANCE_STOPPED
            ]
        );
    }

    #[test]
    fn get_instance_and_list_instances_paginate_and_authorize() {
        let store = std::sync::Arc::new(FakeWorldDirectoryStore::new());
        let w = world();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let inst = instance(w.id(), 4);
            ids.push(inst.id());
            store.insert_instance(inst);
        }
        ids.sort();
        let use_case = WorldDirectoryUseCase::new(store.clone(), super::AllowAllAuthorizer);
        let actor_id = UserId::generate();

        let found = pollster::block_on(use_case.get_instance(actor_id, ids[0]))
            .expect("get_instance succeeds");
        assert_eq!(found.id, ids[0]);

        let missing = pollster::block_on(use_case.get_instance(actor_id, InstanceId::generate()))
            .expect_err("unknown instance must 404");
        assert_eq!(missing.kind(), ApplicationErrorKind::NotFound);

        let page1 = pollster::block_on(use_case.list_instances(
            actor_id,
            orbisync_application::PageRequest {
                limit: 2,
                after: None,
            },
        ))
        .expect("list page 1");
        assert_eq!(page1.items.len(), 2);
        assert!(page1.next.is_some(), "third instance remains");
        assert_eq!(page1.items[0].id, ids[0]);
        assert_eq!(page1.items[1].id, ids[1]);

        let page2 = pollster::block_on(use_case.list_instances(
            actor_id,
            orbisync_application::PageRequest {
                limit: 2,
                after: page1.next,
            },
        ))
        .expect("list page 2");
        assert_eq!(page2.items.len(), 1);
        assert_eq!(page2.items[0].id, ids[2]);
        assert!(page2.next.is_none(), "last page has no cursor");

        let deny_use_case = WorldDirectoryUseCase::new(store, super::DenyAllAuthorizer);
        let denied = pollster::block_on(deny_use_case.get_instance(actor_id, ids[0]))
            .expect_err("deny-all authorizer must reject read");
        assert_eq!(denied.kind(), ApplicationErrorKind::NotAuthorized);
    }

    #[test]
    fn instance_membership_lists_kicks_and_authorizes() {
        use orbisync_application::{InstanceMembershipStore as _, InstanceMembershipUseCase};

        let store = std::sync::Arc::new(FakeWorldDirectoryStore::new());
        let w = world();
        let instance = instance(w.id(), 4);
        let instance_id = instance.id();
        store.insert_instance(instance);

        let membership = std::sync::Arc::new(super::FakeInstanceMembershipStore::new());
        let mut member_ids = vec![UserId::generate(), UserId::generate(), UserId::generate()];
        member_ids.sort_by_key(UserId::to_string);
        membership.seed(instance_id, member_ids.clone());

        let use_case = InstanceMembershipUseCase::new(
            store.clone(),
            membership.clone(),
            super::AllowAllAuthorizer,
        );
        let actor_id = UserId::generate();

        // Pagination over live members.
        let page1 = pollster::block_on(use_case.list_members(
            actor_id,
            instance_id,
            orbisync_application::PageRequest {
                limit: 2,
                after: None,
            },
        ))
        .expect("list page 1");
        assert_eq!(page1.items.len(), 2);
        assert!(page1.next.is_some());
        let page2 = pollster::block_on(use_case.list_members(
            actor_id,
            instance_id,
            orbisync_application::PageRequest {
                limit: 2,
                after: page1.next,
            },
        ))
        .expect("list page 2");
        assert_eq!(page2.items.len(), 1);
        assert!(page2.next.is_none());

        // Unknown instance is 404, even though it has no live members.
        let missing_instance = pollster::block_on(use_case.list_members(
            actor_id,
            InstanceId::generate(),
            orbisync_application::PageRequest {
                limit: 10,
                after: None,
            },
        ))
        .expect_err("unknown instance must 404");
        assert_eq!(missing_instance.kind(), ApplicationErrorKind::NotFound);

        // Kicking a live member succeeds and is reflected in the membership port.
        let target = member_ids[0];
        pollster::block_on(use_case.kick_member(
            actor_id,
            instance_id,
            target,
            ts(),
            RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        ))
        .expect("kick succeeds");
        let remaining = pollster::block_on(membership.list_members(instance_id)).expect("list");
        assert!(!remaining.contains(&target));

        // Kicking again (already gone) is 404.
        let already_gone = pollster::block_on(use_case.kick_member(
            actor_id,
            instance_id,
            target,
            ts(),
            RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        ))
        .expect_err("kicking a non-member must 404");
        assert_eq!(already_gone.kind(), ApplicationErrorKind::NotFound);

        // Unauthorized actor is rejected with an audit, before touching membership.
        let deny_use_case =
            InstanceMembershipUseCase::new(store, membership, super::DenyAllAuthorizer);
        let unauthorized = pollster::block_on(deny_use_case.kick_member(
            actor_id,
            instance_id,
            member_ids[1],
            ts(),
            RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
        ))
        .expect_err("deny-all authorizer must reject kick");
        assert_eq!(unauthorized.kind(), ApplicationErrorKind::NotAuthorized);
    }
}
