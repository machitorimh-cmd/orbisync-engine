/// In-memory empty world directory used as a default when no store is injected.
///
/// Production always injects `PgWorldDirectoryStore` via the builder; tests that
/// do not care about existence verification get this empty implementation which
/// always reports "not found".
struct EmptyWorldDirectoryStore;

struct DenyAllWorldAuthorizer;

#[async_trait::async_trait]
impl WorldAuthorizer for DenyAllWorldAuthorizer {
    async fn require(
        &self,
        _actor: UserId,
        _permission: &str,
    ) -> Result<(), orbisync_application::ApplicationError> {
        Err(orbisync_application::ApplicationError::new(
            ApplicationErrorKind::NotAuthorized,
            "permission denied",
        ))
    }
}

async fn resolve_world_permissions(
    authorizer: &dyn WorldAuthorizer,
    user: UserId,
) -> WorldPermissions {
    WorldPermissions {
        entity_spawn: authorizer
            .require(user, PERMISSION_ENTITY_SPAWN)
            .await
            .is_ok(),
        entity_update_own: authorizer
            .require(user, PERMISSION_ENTITY_UPDATE_OWN)
            .await
            .is_ok(),
        entity_update_any: authorizer
            .require(user, PERMISSION_ENTITY_UPDATE_ANY)
            .await
            .is_ok(),
    }
}

/// Decides whether a temporary subject may act in `world` right now.
///
/// Deliberately not folded into the pre-commit hook's permission re-resolution.
/// That re-resolution runs only when an active extension holds the matching
/// capability, so a deployment with no hook registered -- the default, and the
/// one the whiteboard is exercised under -- would never reach the check. It
/// also never sees transform updates, which take a different branch. Keeping
/// this a standalone call lets join, resume and every command consult it on
/// the same terms.
///
/// Returns `false` when the decision cannot be made: at this boundary, "cannot
/// confirm the subject may act" and "confirmed it may not" are the same answer.
async fn ephemeral_participation_allowed(
    scope: Option<&dyn orbisync_application::EphemeralSubjectScope>,
    user: UserId,
    world: uuid::Uuid,
    now: orbisync_domain::Timestamp,
) -> bool {
    // Isolated test states may omit the boundary. Production always wires it:
    // disabling issuance says nothing about subjects already in the ledger.
    let Some(scope) = scope else {
        return true;
    };
    match scope.decide(user, world, now).await {
        Ok(decision) => decision.permits(),
        Err(error) => {
            tracing::warn!(
                event = "realtime.participation_scope_unavailable",
                error = %error,
                "participation boundary could not be evaluated; refusing"
            );
            false
        }
    }
}

/// Re-checks whether `session_id` is still active (ADR-025 §9), for
/// callers that want a fresher answer than the handshake-time ticket
/// verification.
///
/// Returns `true` (treated as "no additional restriction") when either
/// input is `None` — a `RealtimeTicketVerifier` that does not track
/// sessions (`StubTicketVerifier`, `DenyAllTicketVerifier`, test doubles)
/// or a deployment with no `identity_repository` wired keeps behaving
/// exactly as before this check existed; this is opt-in the same way the
/// pre-commit hook itself is. Returns `false` — fail closed — on a
/// missing session, a `Revoked` status, an expired session, or a lookup
/// error: "cannot confirm the session is still active" and "confirmed
/// inactive" are treated the same way here, per ADR-025 §9 (do not fail
/// open on a lookup error).
async fn is_session_still_active(
    identity_repository: Option<&dyn IdentityRepository>,
    session_id: Option<orbisync_domain::AuthSessionId>,
    now: orbisync_domain::Timestamp,
) -> bool {
    let (Some(repository), Some(session_id)) = (identity_repository, session_id) else {
        return true;
    };
    match repository.find_session(session_id).await {
        Ok(Some(session)) => session.is_active_at(now),
        Ok(None) | Err(_) => false,
    }
}

#[async_trait::async_trait]
impl WorldDirectoryStore for EmptyWorldDirectoryStore {
    async fn get_world(
        &self,
        _: orbisync_domain::WorldId,
    ) -> Result<Option<orbisync_domain::World>, orbisync_application::ApplicationError> {
        Ok(None)
    }

    async fn list_worlds(
        &self,
        _: orbisync_application::PageRequest,
    ) -> Result<
        orbisync_application::Page<orbisync_application::WorldView>,
        orbisync_application::ApplicationError,
    > {
        Ok(orbisync_application::Page {
            items: Vec::new(),
            next: None,
        })
    }

    async fn update_world_with_audit(
        &self,
        _: orbisync_domain::World,
        _: orbisync_application::WorldAuditEvent,
    ) -> Result<(), orbisync_application::ApplicationError> {
        Err(orbisync_application::ApplicationError::new(
            ApplicationErrorKind::NotFound,
            "world not found",
        ))
    }

    async fn get_instance(
        &self,
        _: InstanceId,
    ) -> Result<Option<orbisync_domain::WorldInstance>, orbisync_application::ApplicationError>
    {
        Ok(None)
    }

    async fn create_world_with_audit(
        &self,
        _: orbisync_domain::World,
        _: orbisync_application::WorldAuditEvent,
    ) -> Result<(), orbisync_application::ApplicationError> {
        Ok(())
    }

    async fn create_instance_with_audit(
        &self,
        _: orbisync_domain::WorldInstance,
        _: orbisync_application::WorldAuditEvent,
    ) -> Result<(), orbisync_application::ApplicationError> {
        Ok(())
    }

    async fn record_world_audit(
        &self,
        _: orbisync_application::WorldAuditEvent,
    ) -> Result<(), orbisync_application::ApplicationError> {
        Ok(())
    }

    async fn list_instances(
        &self,
        _: orbisync_application::PageRequest,
    ) -> Result<
        orbisync_application::Page<orbisync_application::InstanceView>,
        orbisync_application::ApplicationError,
    > {
        Ok(orbisync_application::Page {
            items: Vec::new(),
            next: None,
        })
    }
}

/// Shared realtime state owned by the server.
#[derive(Clone)]
pub struct RealtimeState {
    /// Realtime config for limits and heartbeat.
    pub config: RealtimeConfig,
    /// Staged generation policy; does not enlarge the legacy adapter.
    pub checkpoint_limits: orbisync_application::CheckpointLimits,
    /// Explicit generation opt-in; absent preserves legacy operation.
    pub generation: Option<Arc<crate::checkpoint_generation::GenerationServices>>,
    /// Runtime registry for instance actors.
    pub registry: Arc<RuntimeRegistry>,
    /// Delivery registry for StateDelta fan-out.
    pub delivery: Arc<DeliveryRegistry>,
    /// Instance-scoped bounded command results used for realtime idempotency.
    pub command_dedup: Arc<crate::command_dedup::CommandDedupStore>,
    /// Clock for timestamp generation (SystemClock in prod, FixedClock in tests).
    pub clock: Arc<dyn Clock>,
    /// Default instance capacity plumbed from `WorldInstance::capacity` / `world.default_capacity`.
    pub default_capacity: u32,
    /// Interest grid built from `config.interest` (H-6c). Stored here so
    /// `handle_socket` uses the configured `cell_size` / radii instead of
    /// `UniformGrid::default()` and snapshot/delta share the same filter.
    pub interest_grid: orbisync_interest::UniformGrid,
    /// Store for instance existence / capacity resolution (N-2).
    pub world_store: Arc<dyn WorldDirectoryStore>,
    /// Durable checkpoint store used before publishing a runtime actor.
    pub checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    /// Durable persistent-entity row store consulted after checkpoint
    /// restore to recover entity mutations newer than the latest checkpoint
    /// (crash gap between periodic checkpoints, `state-and-runtime.md` §1.2/
    /// §1.3). `None` in tests that do not exercise activation restore.
    pub persistent_entity_store: Option<Arc<dyn PersistentEntityStore>>,
    /// Resolves a realtime ticket into the authenticated user (N-1, RV-A C1 async).
    pub tickets: Arc<dyn RealtimeTicketVerifier>,
    /// Resolves world permissions for runtime commands at the application boundary.
    pub world_authorizer: Arc<dyn WorldAuthorizer>,
    /// Loads viewer roles once after realtime authentication.
    pub identity_repository: Option<Arc<dyn IdentityRepository>>,
    /// Bounds which worlds a temporary subject may participate in (ADR-026).
    ///
    /// Always configured in production, even with temporary issuance disabled.
    /// Test states may omit it, treating every subject as ungoverned.
    pub ephemeral_scope: Option<Arc<dyn orbisync_application::EphemeralSubjectScope>>,
    /// Server-side resume token store (W-18, D-23): process memory only (D-24).
    /// MRIB-02 remains open; this choice is documented and reversible.
    pub resume_store: Arc<ResumeSessionStore>,
    /// Configured resume grace period, used to schedule expiry pruning.
    pub resume_grace_seconds: u64,
    /// Configured interval for expiry pruning, in seconds.
    pub resume_prune_interval_seconds: u64,
    /// Semaphore limiting concurrent WebSocket connections (C3).
    pub connection_semaphore: Arc<Semaphore>,
    /// Number of currently active WebSocket connections (C3 metric).
    pub active_connections: Arc<AtomicUsize>,
    /// Total rejected upgrades due to semaphore full (C3 metric).
    pub rejected_upgrade_total: Arc<AtomicU64>,
    /// Total handshake timeouts before ClientHello (C3 metric).
    pub handshake_timeout_total: Arc<AtomicU64>,
    /// Normal message rate configured at the composition root.
    pub rate_limit_normal_per_sec: u32,
    /// Custom message rate configured at the composition root.
    pub rate_limit_custom_per_sec: u32,
    /// Persistent denial threshold configured at the composition root.
    pub rate_limit_persistent_threshold: u32,
    /// Bounded world revision history capacity.
    pub history_capacity: usize,
    /// Configured movement speed limit.
    pub max_speed: f64,
    /// Configured movement acceleration limit.
    pub max_acceleration: f64,
    /// `world.speed_acceleration_check_enabled` (ADR-024). Gates only the
    /// speed/acceleration comparison; ownership, revision, distance, and
    /// finiteness checks are unaffected regardless of this setting.
    pub speed_acceleration_check_enabled: bool,
    /// Per-instance custom component update rate.
    pub component_updates_per_sec: u32,
    /// Per-instance bounded mailbox capacities.
    pub mailbox_config: MailboxConfig,
    /// Metrics recorder used by each instance mailbox.
    pub metrics_recorder: Arc<dyn MetricsRecorder>,
    /// Process-wide bound on concurrent instance activations. Each activation
    /// may load and decode a checkpoint up to
    /// `MAX_CHECKPOINT_PAYLOAD_BYTES`, so the process peak is
    /// "per-activation peak x concurrent activations + periodic saves"; a
    /// byte limit alone does not bound it. Periodic saves are bounded by
    /// `MAX_PENDING_CHECKPOINT_SAVES` in the composition root. See the limit
    /// derivation on `orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES`.
    pub activation_permits: Arc<Semaphore>,
    /// How long an activation may wait for an activation permit before
    /// failing closed (round 4: the wait used to be unbounded, so a stuck
    /// restore could stall joins indefinitely).
    pub activation_permit_timeout: std::time::Duration,
    /// Shared process lifecycle state used to reject new upgrades and drain
    /// accepted connections.
    pub shutdown: Arc<ShutdownState>,
    /// Instance-local bounded checkpoint recovery workers. A failed instance
    /// must not hold up maintenance or shutdown for unrelated instances.
    recovery_tasks: Arc<Mutex<RecoveryTasks>>,
    /// Trusted input rules, registered by server composition.
    pub input_rules: crate::input::InputRules,
    /// Synchronous pre-commit validation hook (ADR-025). `None` when the
    /// feature is not wired (e.g. no extension backend configured); the
    /// entity mutation dispatch path then behaves exactly as it did before
    /// ADR-025, since no capability lookup or HTTP call is ever attempted.
    pub pre_commit_gate: Option<Arc<dyn orbisync_extensions::PreCommitGate>>,
    /// Extension registration lookup used to find the (at most one) Active
    /// extension subscribed to a given pre-commit-hook capability
    /// (ADR-025). `None` has the same opt-out effect as `pre_commit_gate`
    /// being `None`.
    pub extension_registrations: Option<Arc<dyn orbisync_extensions::ExtensionRegistrationStore>>,
    /// Cached "does an Active `hooks:entity:spawn` registration currently
    /// exist" flag (ADR-025 "既知の迂回経路"). Read (not queried) on the
    /// `UpdateTransform` hot path to decide whether the M2 auto-create
    /// path may create an unapproved entity; refreshed periodically by a
    /// background task (`refresh_spawn_hook_active_cache`), not on every
    /// message and not fixed once at process startup, so a registration
    /// added or suspended after boot is reflected within one refresh
    /// interval rather than never. Defaults to `false` (matches
    /// pre-ADR-025 behavior) until the first refresh completes.
    pub spawn_hook_active: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct RecoveryTasks {
    closed: bool,
    handles: Vec<JoinHandle<()>>,
}

fn log_recovery_join_result(result: Result<(), tokio::task::JoinError>, phase: &'static str) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::error!(
            event = "realtime.recovery_worker_failed",
            phase,
            error = %error,
            "recovery worker exited unexpectedly"
        );
    }
}

/// Maximum number of instances that may be restored from a durable
/// checkpoint at the same time in this process. Two activations of a
/// limit-sized checkpoint stay inside the documented 1 GiB process memory
/// budget alongside the bounded periodic saves (see the derivation on
/// `orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES`).
pub const MAX_CONCURRENT_INSTANCE_ACTIVATIONS: usize = 2;

/// Default ceiling on how long a join waits for a free activation permit.
/// Restoring a limit-sized checkpoint takes well under a second (measured),
/// so 10 seconds covers a full queue of `MAX_CONCURRENT_INSTANCE_ACTIVATIONS`
/// restores with a wide margin while still failing closed instead of
/// stalling forever.
pub const ACTIVATION_PERMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl RealtimeState {
    /// Creates the state.
    #[must_use]
    pub fn new(
        config: RealtimeConfig,
        registry: Arc<RuntimeRegistry>,
        delivery: Arc<DeliveryRegistry>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::with_capacity(config, registry, delivery, clock, 100)
    }

    /// Creates the state with an explicit capacity (plumbed from `WorldInstance`).
    #[must_use]
    pub fn with_capacity(
        config: RealtimeConfig,
        registry: Arc<RuntimeRegistry>,
        delivery: Arc<DeliveryRegistry>,
        clock: Arc<dyn Clock>,
        default_capacity: u32,
    ) -> Self {
        let resume_store = Arc::new(ResumeSessionStore::new(Arc::clone(&clock), 60));
        let resume_prune_interval_seconds = config.resume_prune_interval_seconds;
        let permits = config.max_connections.max(1) as usize;
        let history_capacity = config.outbound_queue_capacity as usize;
        Self {
            config,
            registry,
            delivery,
            command_dedup: Arc::new(crate::command_dedup::CommandDedupStore::default()),
            clock,
            default_capacity,
            interest_grid: orbisync_interest::UniformGrid::default(),
            world_store: Arc::new(EmptyWorldDirectoryStore),
            checkpoint_store: None,
            checkpoint_limits: orbisync_application::CheckpointLimits::default(),
            generation: None,
            persistent_entity_store: None,
            tickets: Arc::new(DenyAllTicketVerifier::new()),
            world_authorizer: Arc::new(DenyAllWorldAuthorizer),
            identity_repository: None,
            ephemeral_scope: None,
            resume_store,
            resume_grace_seconds: 60,
            resume_prune_interval_seconds,
            connection_semaphore: Arc::new(Semaphore::new(permits)),
            active_connections: Arc::new(AtomicUsize::new(0)),
            rejected_upgrade_total: Arc::new(AtomicU64::new(0)),
            handshake_timeout_total: Arc::new(AtomicU64::new(0)),
            rate_limit_normal_per_sec: RateLimiter::DEFAULT_NORMAL_PER_SEC,
            rate_limit_custom_per_sec: RateLimiter::DEFAULT_CUSTOM_PER_SEC,
            rate_limit_persistent_threshold: RateLimiter::DEFAULT_PERSISTENT_THRESHOLD,
            history_capacity,
            max_speed: orbisync_world_runtime::validation::DEFAULT_MAX_SPEED,
            max_acceleration: orbisync_world_runtime::validation::DEFAULT_MAX_ACCELERATION,
            speed_acceleration_check_enabled: true,
            component_updates_per_sec: InstanceActor::DEFAULT_COMPONENT_UPDATES_PER_SEC,
            mailbox_config: MailboxConfig::default(),
            metrics_recorder: Arc::new(NoopMetrics),
            activation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INSTANCE_ACTIVATIONS)),
            activation_permit_timeout: ACTIVATION_PERMIT_TIMEOUT,
            shutdown: Arc::new(ShutdownState::new()),
            recovery_tasks: Arc::new(Mutex::new(RecoveryTasks::default())),
            input_rules: Default::default(),
            pre_commit_gate: None,
            extension_registrations: None,
            spawn_hook_active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates the state with an explicit interest grid (H-6c: config-driven).
    #[must_use]
    pub fn with_interest_grid(
        config: RealtimeConfig,
        registry: Arc<RuntimeRegistry>,
        delivery: Arc<DeliveryRegistry>,
        clock: Arc<dyn Clock>,
        default_capacity: u32,
        interest_grid: orbisync_interest::UniformGrid,
    ) -> Self {
        let resume_store = Arc::new(ResumeSessionStore::new(Arc::clone(&clock), 60));
        let resume_prune_interval_seconds = config.resume_prune_interval_seconds;
        let permits = config.max_connections.max(1) as usize;
        let history_capacity = config.outbound_queue_capacity as usize;
        Self {
            config,
            registry,
            delivery,
            command_dedup: Arc::new(crate::command_dedup::CommandDedupStore::default()),
            clock,
            default_capacity,
            interest_grid,
            world_store: Arc::new(EmptyWorldDirectoryStore),
            checkpoint_store: None,
            checkpoint_limits: orbisync_application::CheckpointLimits::default(),
            generation: None,
            persistent_entity_store: None,
            tickets: Arc::new(DenyAllTicketVerifier::new()),
            world_authorizer: Arc::new(DenyAllWorldAuthorizer),
            identity_repository: None,
            ephemeral_scope: None,
            resume_store,
            resume_grace_seconds: 60,
            resume_prune_interval_seconds,
            connection_semaphore: Arc::new(Semaphore::new(permits)),
            active_connections: Arc::new(AtomicUsize::new(0)),
            rejected_upgrade_total: Arc::new(AtomicU64::new(0)),
            handshake_timeout_total: Arc::new(AtomicU64::new(0)),
            rate_limit_normal_per_sec: RateLimiter::DEFAULT_NORMAL_PER_SEC,
            rate_limit_custom_per_sec: RateLimiter::DEFAULT_CUSTOM_PER_SEC,
            rate_limit_persistent_threshold: RateLimiter::DEFAULT_PERSISTENT_THRESHOLD,
            history_capacity,
            max_speed: orbisync_world_runtime::validation::DEFAULT_MAX_SPEED,
            max_acceleration: orbisync_world_runtime::validation::DEFAULT_MAX_ACCELERATION,
            speed_acceleration_check_enabled: true,
            component_updates_per_sec: InstanceActor::DEFAULT_COMPONENT_UPDATES_PER_SEC,
            mailbox_config: MailboxConfig::default(),
            metrics_recorder: Arc::new(NoopMetrics),
            activation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INSTANCE_ACTIVATIONS)),
            activation_permit_timeout: ACTIVATION_PERMIT_TIMEOUT,
            shutdown: Arc::new(ShutdownState::new()),
            recovery_tasks: Arc::new(Mutex::new(RecoveryTasks::default())),
            input_rules: Default::default(),
            pre_commit_gate: None,
            extension_registrations: None,
            spawn_hook_active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Builder entry point (N-2: resolves instance existence via `world_store`).
    #[must_use]
    pub fn builder(
        config: RealtimeConfig,
        registry: Arc<RuntimeRegistry>,
        delivery: Arc<DeliveryRegistry>,
        clock: Arc<dyn Clock>,
    ) -> RealtimeStateBuilder {
        let history_capacity = config.outbound_queue_capacity as usize;
        RealtimeStateBuilder {
            config,
            registry,
            delivery,
            clock,
            default_capacity: None,
            interest_grid: None,
            world_store: None,
            checkpoint_store: None,
            checkpoint_limits: orbisync_application::CheckpointLimits::default(),
            generation: None,
            persistent_entity_store: None,
            command_dedup: None,
            tickets: None,
            world_authorizer: None,
            identity_repository: None,
            ephemeral_scope: None,
            resume_store: None,
            resume_grace_seconds: None,
            rate_limit_normal_per_sec: None,
            rate_limit_custom_per_sec: None,
            rate_limit_persistent_threshold: None,
            history_capacity: Some(history_capacity),
            max_speed: None,
            max_acceleration: None,
            speed_acceleration_check_enabled: None,
            component_updates_per_sec: None,
            mailbox_config: None,
            metrics_recorder: None,
            shutdown: None,
            input_rules: Default::default(),
            pre_commit_gate: None,
            extension_registrations: None,
            spawn_hook_active: None,
        }
    }

    /// Removes expired resume bindings and refreshes the member gauge.
    ///
    /// D-26 keeps disconnected presences alive during the grace period. This
    /// method is called by the server's periodic maintenance task after that
    /// period, so the membership and its aggregate metric are both reclaimed.
    pub fn prune_expired_resume_sessions(&self) -> usize {
        let expired = self.resume_store.prune_expired();
        if expired.is_empty() {
            return 0;
        }
        for binding in expired.iter().filter(|binding| binding.disconnected) {
            if let Err(error) = self.registry.submit_sync(
                binding.instance_id,
                InstanceCommand::Leave {
                    presence_id: binding.presence_id,
                },
            ) {
                tracing::warn!(
                    event = "realtime.expired_resume_leave_failed",
                    instance_id = %binding.instance_id,
                    error = ?error,
                );
            }
        }
        record_total_members(&self.registry, self.metrics_recorder.as_ref());
        expired.len()
    }

    /// Removes expired resume bindings through their task-owned actors.
    pub async fn prune_expired_resume_sessions_async(&self) -> usize {
        let expired = self.resume_store.prune_expired();
        if expired.is_empty() {
            return 0;
        }
        for binding in expired.iter().filter(|binding| binding.disconnected) {
            if let Err(error) = self
                .registry
                .submit(
                    binding.instance_id,
                    InstanceCommand::Leave {
                        presence_id: binding.presence_id,
                    },
                )
                .await
            {
                tracing::warn!(
                    event = "realtime.expired_resume_leave_failed",
                    instance_id = %binding.instance_id,
                    error = ?error,
                );
            }
        }
        record_total_members(&self.registry, self.metrics_recorder.as_ref());
        expired.len()
    }

    /// Returns the configured maintenance cadence for resume expiry.
    #[must_use]
    pub fn resume_prune_interval_seconds(&self) -> u64 {
        self.resume_prune_interval_seconds
    }

    /// Merges durable `persistent_entities` rows into `checkpoint`, recovering
    /// entity mutations newer than the latest saved checkpoint.
    ///
    /// Rows are written on every persistence-relevant mutation while
    /// checkpoints are only taken periodically
    /// (`world.checkpoint_interval_secs`, default 300s), so a crash between
    /// checkpoints can leave durable rows ahead of the last saved checkpoint
    /// by up to that interval. Graceful shutdown is unaffected: it flushes a
    /// final checkpoint that already reflects every row.
    ///
    /// The exact restore procedure (precedence when both sources disagree,
    /// handling of a row deleted after its last checkpoint, dedup/instance
    /// revision reconciliation) is an open design question deferred to
    /// ARC-08 (`state-and-runtime.md:60`). This merge only takes the action
    /// that cannot regress the existing checkpoint-only restore:
    ///
    /// - An entity present only in rows (not yet checkpointed) is added.
    /// - An entity present in both, with a strictly newer row revision, is
    ///   replaced by the row (rows are never older than their checkpoint by
    ///   construction, since checkpoints are built from the same state).
    /// - An entity present only in the checkpoint (deleted from rows, or the
    ///   row source unavailable) is left untouched.
    /// - The checkpoint's own revision is raised to the maximum entity
    ///   revision now present, which is required to satisfy
    ///   `InstanceState::from_persisted`'s existing invariant that no entity
    ///   revision may exceed the instance revision; this is a mechanical
    ///   consequence of merging, not a new policy choice.
    /// - Dedup entries and any other checkpoint-only state are left exactly
    ///   as the checkpoint stored them; rows carry no dedup data.
    async fn merge_persistent_rows(
        &self,
        instance_id: InstanceId,
        mut checkpoint: orbisync_world_runtime::Checkpoint,
    ) -> Result<orbisync_world_runtime::Checkpoint, InstanceActivationError> {
        let Some(store) = self.persistent_entity_store.as_ref() else {
            return Ok(checkpoint);
        };
        let rows = store
            .list_by_instance(instance_id)
            .await
            .map_err(|error| InstanceActivationError::new(error.to_string()))?;
        if rows.is_empty() {
            return Ok(checkpoint);
        }

        let mut by_id: std::collections::HashMap<EntityId, orbisync_domain::Entity> = checkpoint
            .entities
            .into_iter()
            .map(|entity| (entity.id(), entity))
            .collect();
        let mut merged_revision = checkpoint.revision;
        let mut added = 0_usize;
        let mut upgraded = 0_usize;
        for row in rows {
            if row.revision() > merged_revision {
                merged_revision = row.revision();
            }
            match by_id.get(&row.id()) {
                Some(existing) if existing.revision() >= row.revision() => {}
                Some(_) => {
                    upgraded += 1;
                    by_id.insert(row.id(), row);
                }
                None => {
                    added += 1;
                    by_id.insert(row.id(), row);
                }
            }
        }
        if added > 0 || upgraded > 0 {
            tracing::info!(
                event = "persistent_entity.restore_merged",
                instance_id = %instance_id,
                added,
                upgraded,
                "merged persistent entity rows newer than the last checkpoint into the restored instance"
            );
        }
        checkpoint.entities = by_id.into_values().collect();
        checkpoint.revision = merged_revision;
        Ok(checkpoint)
    }

    /// Ensures an instance actor exists, restoring its latest durable
    /// checkpoint before the actor becomes visible to any join request.
    /// Both guards must be retained through Join submission so shutdown waits
    /// for this accepted operation before enumerating actors for final saves.
    pub async fn ensure_instance_activated(
        &self,
        instance_id: InstanceId,
    ) -> Result<
        (OwnedMutexGuard<()>, tokio::sync::OwnedRwLockReadGuard<()>),
        InstanceActivationError,
    > {
        let admission = self
            .shutdown
            .admit()
            .ok_or_else(|| InstanceActivationError::new("server is shutting down"))?;
        let activation = self
            .ensure_instance_activated_admitted(instance_id, &admission)
            .await?;
        Ok((activation, admission))
    }

    async fn ensure_instance_activated_admitted(
        &self,
        instance_id: InstanceId,
        _admission: &tokio::sync::OwnedRwLockReadGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, InstanceActivationError> {
        // The returned guard remains held until the caller has submitted Join.
        // This closes the activation-to-membership window in which the periodic
        // idle reaper could otherwise stop a newly restored actor.
        let activation = self.registry.lifecycle_guard(instance_id).await;
        if let Some(service) = &self.generation {
            if !service.ready() {
                return Err(InstanceActivationError::new(
                    "generation writer unavailable",
                ));
            }
            if self.registry.contains(instance_id) {
                return Ok(activation);
            }
            let state = self.clone();
            let generation = service.clone();
            let actor = service
                .restore_with(instance_id, move |selected, mut checkpoint, cutoff| {
                    let receipts = std::mem::take(&mut checkpoint.dedup);
                    let mut actor = InstanceActor::from_checkpoint_with_mailbox_config(
                        checkpoint,
                        state.interest_grid.near_radius() as f32,
                        state.history_capacity,
                        state.max_speed,
                        state.max_acceleration,
                        state.mailbox_config,
                        Arc::clone(&state.metrics_recorder),
                    )?;
                    actor.enable_generation_admission_with_limits(
                        Some(orbisync_world_runtime::actor::GenerationBinding {
                            store: generation.store.clone(),
                            writer: generation.writer.clone(),
                            head: selected.expected_head.checked_add(1).ok_or_else(|| {
                                orbisync_application::ApplicationError::port_failure(
                                    "invalid generation head",
                                )
                            })?,
                        }),
                        generation.limits,
                        receipts,
                    )?;
                    actor.set_generation_cutoff(cutoff)?;
                    actor = actor
                        .with_component_updates_per_sec(state.component_updates_per_sec)
                        .with_speed_acceleration_check(state.speed_acceleration_check_enabled)
                        .with_spawn_hook_active(Arc::clone(&state.spawn_hook_active));
                    actor.start();

                    Ok(actor)
                })
                .await
                .map_err(|e| InstanceActivationError::new(e.to_string()))?;
            if !service.ready() {
                return Err(InstanceActivationError::new("generation writer lost"));
            }
            self.registry.ensure_instance(actor);
            return Ok(activation);
        }
        if self.registry.contains(instance_id) {
            return Ok(activation);
        }

        let store = self
            .checkpoint_store
            .as_ref()
            .ok_or_else(|| InstanceActivationError::new("checkpoint store is not configured"))?;
        // Bound concurrent checkpoint restorations process-wide. Rejoins to an
        // already-active instance return above without taking a permit; only
        // the expensive load/decode path is serialized. The permit is held for
        // the whole restore and released when this future completes, before
        // the caller submits Join. The wait is bounded: an activation that
        // cannot get a permit within `activation_permit_timeout` fails closed
        // instead of stalling the join forever (round 4).
        let _activation_permit = tokio::time::timeout(
            self.activation_permit_timeout,
            self.activation_permits.acquire(),
        )
        .await
        .map_err(|_| {
            InstanceActivationError::new(format!(
                "timed out after {:?} waiting for an instance activation permit",
                self.activation_permit_timeout
            ))
        })?
        .map_err(|error| InstanceActivationError::new(error.to_string()))?;
        let durable = store.load_latest(instance_id).await.map_err(|error| {
            if error.kind() == ApplicationErrorKind::CheckpointTooLarge {
                self.metrics_recorder
                    .incr(Counter::CheckpointRestoreRejectedTotal);
                tracing::error!(
                    event = "checkpoint.restore_rejected_too_large",
                    instance_id = %instance_id,
                    limit_bytes = MAX_CHECKPOINT_PAYLOAD_BYTES,
                    detail = %error.detail(),
                    "checkpoint exceeds the payload limit; the instance cannot be restored from its durable row"
                );
            }
            InstanceActivationError::new(error.to_string())
        })?;

        let mut actor = if let Some(durable) = durable {
            if durable.instance_id != instance_id {
                return Err(InstanceActivationError::new(
                    "checkpoint store returned a different instance id",
                ));
            }
            let checkpoint = orbisync_world_runtime::Checkpoint::from_legacy_json_with_limits(
                &durable.payload,
                self.checkpoint_limits,
            )
            .map_err(|error| InstanceActivationError::new(error.to_string()))?;
            if checkpoint.instance_id != durable.instance_id {
                return Err(InstanceActivationError::new(
                    "checkpoint payload instance id does not match its storage row",
                ));
            }
            if checkpoint.revision != durable.revision {
                return Err(InstanceActivationError::new(
                    "checkpoint payload revision does not match its storage row",
                ));
            }
            let checkpoint = self.merge_persistent_rows(instance_id, checkpoint).await?;
            let restored_dedup = self
                .command_dedup
                .restore(
                    instance_id,
                    &checkpoint.dedup,
                    self.clock.now().to_unix_millis().unwrap_or(0),
                )
                .map_err(InstanceActivationError::new)?;
            let restored_revision = checkpoint.revision;
            let restored_entities = checkpoint.entity_count();
            let actor = InstanceActor::from_checkpoint_with_mailbox_config(
                checkpoint,
                self.interest_grid.near_radius() as f32,
                self.history_capacity,
                self.max_speed,
                self.max_acceleration,
                self.mailbox_config,
                Arc::clone(&self.metrics_recorder),
            )
            .map_err(|error| InstanceActivationError::new(error.to_string()))?;
            tracing::info!(
                event = "checkpoint.restored",
                instance_id = %instance_id,
                revision = restored_revision.as_u64(),
                entity_count = restored_entities,
                dedup_count = restored_dedup,
                "restored instance actor before join"
            );
            actor
        } else {
            // No checkpoint has ever been saved for this instance (it may
            // still have never existed, or it crashed before its first
            // periodic checkpoint). Row-only recovery covers that gap too:
            // an empty synthetic checkpoint gains any durable rows via the
            // same merge used for the checkpoint-present path.
            let empty = orbisync_world_runtime::Checkpoint::new(
                instance_id,
                orbisync_domain::Revision::INITIAL,
                Vec::new(),
                self.clock.now(),
            );
            let merged = self.merge_persistent_rows(instance_id, empty).await?;
            if merged.entities.is_empty() {
                InstanceActor::with_mailbox_config(
                    InstanceRuntimeDescriptor {
                        instance_id,
                        state: RuntimeState::Starting,
                        revision: orbisync_domain::Revision::INITIAL,
                    },
                    self.interest_grid.near_radius() as f32,
                    self.history_capacity,
                    self.max_speed,
                    self.max_acceleration,
                    self.mailbox_config,
                    Arc::clone(&self.metrics_recorder),
                )
            } else {
                let restored_revision = merged.revision;
                let restored_entities = merged.entity_count();
                let actor = InstanceActor::from_checkpoint_with_mailbox_config(
                    merged,
                    self.interest_grid.near_radius() as f32,
                    self.history_capacity,
                    self.max_speed,
                    self.max_acceleration,
                    self.mailbox_config,
                    Arc::clone(&self.metrics_recorder),
                )
                .map_err(|error| InstanceActivationError::new(error.to_string()))?;
                tracing::info!(
                    event = "persistent_entity.restored_without_checkpoint",
                    instance_id = %instance_id,
                    revision = restored_revision.as_u64(),
                    entity_count = restored_entities,
                    "restored instance actor from persistent entity rows with no prior checkpoint"
                );
                actor
            }
        };
        actor = actor.with_component_updates_per_sec(self.component_updates_per_sec);
        actor = actor.with_speed_acceleration_check(self.speed_acceleration_check_enabled);
        actor = actor.with_spawn_hook_active(Arc::clone(&self.spawn_hook_active));
        actor.start();
        self.registry.ensure_instance(actor);
        Ok(activation)
    }

    /// Starts a detached, instance-local recovery worker and retains its join
    /// handle so shutdown can await it with a deadline.
    pub fn spawn_recovery<F>(&self, recovery: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut tasks = self
            .recovery_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Join handles retain task allocation even after the task finishes.
        // Reap completed workers on every admission so storage is bounded by
        // currently-running recovery, not lifetime command volume.
        tasks.handles.retain(|task| !task.is_finished());
        if tasks.closed {
            return;
        }
        tasks.handles.push(tokio::spawn(recovery));
    }

    /// Waits for recovery workers without making shutdown depend on a broken
    /// persistence port. Workers that overrun the deadline are aborted.
    pub async fn drain_recovery(&self, timeout: std::time::Duration) {
        let mut tasks = {
            let mut all = self
                .recovery_tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            all.closed = true;
            std::mem::take(&mut all.handles)
        };
        let deadline = tokio::time::Instant::now() + timeout;
        while let Some(mut task) = tasks.pop() {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(result) => log_recovery_join_result(result, "drain"),
                Err(_) => {
                    task.abort();
                    for remaining in &tasks {
                        remaining.abort();
                    }
                    log_recovery_join_result(task.await, "drain-abort");
                    for remaining in tasks {
                        log_recovery_join_result(remaining.await, "drain-abort");
                    }
                    return;
                }
            }
        }
    }

    /// Refreshes [`Self::spawn_hook_active`] from `extension_registrations`,
    /// when configured (ADR-025 "既知の迂回経路"). A no-op when no
    /// registration store is wired (opt-out, matches pre-ADR-025 behavior).
    ///
    /// A lookup failure (e.g. a duplicate-registration `Conflict` or a
    /// transient database error) sets the flag to `true` (fail closed)
    /// rather than leaving the previous value in place: independent
    /// verification found that leaving the previous value meant a failed
    /// *first* refresh at process boot left the flag at its `false`
    /// default indefinitely, silently permitting `UpdateTransform`
    /// auto-create for as long as the lookup kept failing — asymmetric
    /// with `SpawnEntity`, whose live (uncached) lookup on the same
    /// failure already returns `PRE_COMMIT_UNAVAILABLE`. This cache is
    /// still only a secondary/defense-in-depth signal for the actor's
    /// auto-create branch: the primary, zero-window enforcement is the
    /// live per-candidate lookup in `realtime_ws_connection_runtime.rs`'s
    /// `UpdateTransform` handling, which this cache cannot substitute for
    /// on its own (see ADR-025).
    pub async fn refresh_spawn_hook_active_cache(&self) {
        let Some(registrations) = self.extension_registrations.as_ref() else {
            return;
        };
        let active = match registrations
            .find_active_registration_by_capability(PreCommitOperation::Spawn.capability())
            .await
        {
            Ok(found) => found.is_some(),
            Err(_) => true,
        };
        self.spawn_hook_active.store(active, Ordering::Release);
    }
}

/// Error returned when an instance cannot be activated safely.
#[derive(Debug)]
pub struct InstanceActivationError {
    detail: String,
}

impl InstanceActivationError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl core::fmt::Display for InstanceActivationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Builder for [`RealtimeState`] (N-2: existence verification requires `world_store`).
pub struct RealtimeStateBuilder {
    checkpoint_limits: orbisync_application::CheckpointLimits,
    generation: Option<Arc<crate::checkpoint_generation::GenerationServices>>,
    config: RealtimeConfig,
    registry: Arc<RuntimeRegistry>,
    delivery: Arc<DeliveryRegistry>,
    clock: Arc<dyn Clock>,
    default_capacity: Option<u32>,
    interest_grid: Option<orbisync_interest::UniformGrid>,
    world_store: Option<Arc<dyn WorldDirectoryStore>>,
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    persistent_entity_store: Option<Arc<dyn PersistentEntityStore>>,
    command_dedup: Option<Arc<crate::command_dedup::CommandDedupStore>>,
    tickets: Option<Arc<dyn RealtimeTicketVerifier>>,
    world_authorizer: Option<Arc<dyn WorldAuthorizer>>,
    identity_repository: Option<Arc<dyn IdentityRepository>>,
    ephemeral_scope: Option<Arc<dyn orbisync_application::EphemeralSubjectScope>>,
    resume_store: Option<Arc<ResumeSessionStore>>,
    resume_grace_seconds: Option<u64>,
    rate_limit_normal_per_sec: Option<u32>,
    rate_limit_custom_per_sec: Option<u32>,
    rate_limit_persistent_threshold: Option<u32>,
    history_capacity: Option<usize>,
    max_speed: Option<f64>,
    max_acceleration: Option<f64>,
    speed_acceleration_check_enabled: Option<bool>,
    component_updates_per_sec: Option<u32>,
    mailbox_config: Option<MailboxConfig>,
    metrics_recorder: Option<Arc<dyn MetricsRecorder>>,
    shutdown: Option<Arc<ShutdownState>>,
    input_rules: crate::input::InputRules,
    pre_commit_gate: Option<Arc<dyn orbisync_extensions::PreCommitGate>>,
    extension_registrations: Option<Arc<dyn orbisync_extensions::ExtensionRegistrationStore>>,
    spawn_hook_active: Option<Arc<AtomicBool>>,
}

impl RealtimeStateBuilder {
    /// Registers trusted rule code for a world. Registration is composition-time only.
    #[must_use]
    pub fn with_input_rule(
        mut self,
        world: orbisync_domain::WorldId,
        name: impl Into<String>,
        rule: Arc<dyn crate::input::InputRule>,
    ) -> Self {
        self.input_rules.insert((world, name.into()), crate::input::RegisteredInputRule::Local(rule));
        self
    }

    /// Installs validated operator configuration, shared with stock production startup.
    #[must_use]
    pub fn with_input_rules(mut self, rules: crate::input::InputRules) -> Self {
        self.input_rules.extend(rules);
        self
    }

    /// Production composition preserves legacy mode when opt-in is absent.
    #[must_use]
    pub fn with_optional_generation_services(
        mut self,
        service: Option<Arc<crate::checkpoint_generation::GenerationServices>>,
    ) -> Self {
        self.generation = service;
        self
    }
    /// Explicit capability opt-in. The service's validated policy takes precedence
    /// over legacy builder defaults, regardless of builder call order.
    #[must_use]
    pub fn with_generation_services(
        mut self,
        service: Arc<crate::checkpoint_generation::GenerationServices>,
    ) -> Self {
        self.generation = Some(service);
        self
    }
    /// Retains the exact validated generation policy for future activation/store wiring.
    #[must_use]
    pub fn with_checkpoint_limits(
        mut self,
        limits: orbisync_application::CheckpointLimits,
    ) -> Self {
        self.checkpoint_limits = limits;
        self
    }

    /// Overrides the default capacity (otherwise `100`).
    #[must_use]
    pub fn with_capacity(mut self, capacity: u32) -> Self {
        self.default_capacity = Some(capacity);
        self
    }

    /// Sets the interest grid (otherwise `UniformGrid::default()`).
    #[must_use]
    pub fn with_interest_grid(mut self, grid: orbisync_interest::UniformGrid) -> Self {
        self.interest_grid = Some(grid);
        self
    }

    /// Sets the world directory store for instance existence / capacity (N-2).
    #[must_use]
    pub fn with_world_store(mut self, store: Arc<dyn WorldDirectoryStore>) -> Self {
        self.world_store = Some(store);
        self
    }

    /// Sets the durable checkpoint store used for restore-before-join.
    #[must_use]
    pub fn with_checkpoint_store(mut self, store: Arc<dyn CheckpointStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    /// Sets the durable persistent-entity row store consulted after
    /// checkpoint restore to recover rows newer than the latest checkpoint.
    #[must_use]
    pub fn with_persistent_entity_store(mut self, store: Arc<dyn PersistentEntityStore>) -> Self {
        self.persistent_entity_store = Some(store);
        self
    }

    /// Sets the process-wide dedup store shared with checkpoint persistence.
    #[must_use]
    pub fn with_command_dedup(
        mut self,
        store: Arc<crate::command_dedup::CommandDedupStore>,
    ) -> Self {
        self.command_dedup = Some(store);
        self
    }

    /// Sets the ticket verifier for authentication (N-1).
    #[must_use]
    pub fn with_tickets(mut self, tickets: Arc<dyn RealtimeTicketVerifier>) -> Self {
        self.tickets = Some(tickets);
        self
    }

    /// Sets the application authorizer used to resolve world command permissions.
    #[must_use]
    pub fn with_world_authorizer(mut self, authorizer: Arc<dyn WorldAuthorizer>) -> Self {
        self.world_authorizer = Some(authorizer);
        self
    }

    /// Sets the identity repository used to resolve viewer roles at join time.
    #[must_use]
    pub fn with_identity_repository(mut self, repository: Arc<dyn IdentityRepository>) -> Self {
        self.identity_repository = Some(repository);
        self
    }

    /// Sets the participation boundary for temporary subjects (ADR-026).
    #[must_use]
    pub fn with_ephemeral_scope(
        mut self,
        scope: Arc<dyn orbisync_application::EphemeralSubjectScope>,
    ) -> Self {
        self.ephemeral_scope = Some(scope);
        self
    }

    /// Sets the participation boundary when one is configured.
    ///
    /// `None` leaves the realtime state without a boundary, which reports
    /// every subject as ungoverned. Production must use `with_ephemeral_scope`
    /// even when issuance of temporary subjects is disabled.
    #[must_use]
    pub fn with_optional_ephemeral_scope(
        self,
        scope: Option<Arc<dyn orbisync_application::EphemeralSubjectScope>>,
    ) -> Self {
        match scope {
            Some(scope) => self.with_ephemeral_scope(scope),
            None => self,
        }
    }

    /// Sets the resume session store (W-18). Overrides the default process-local store.
    #[must_use]
    pub fn with_resume_store(mut self, store: Arc<ResumeSessionStore>) -> Self {
        self.resume_store = Some(store);
        self
    }

    /// Sets the resume grace in seconds (W-18, D-26). When set, a fresh store is created with this TTL.
    #[must_use]
    pub fn with_resume_grace_seconds(mut self, secs: u64) -> Self {
        self.resume_grace_seconds = Some(secs);
        self
    }

    /// Sets configured realtime rate limits.
    #[must_use]
    pub fn with_rate_limits(mut self, normal: u32, custom: u32, threshold: u32) -> Self {
        self.rate_limit_normal_per_sec = Some(normal);
        self.rate_limit_custom_per_sec = Some(custom);
        self.rate_limit_persistent_threshold = Some(threshold);
        self
    }

    /// Sets the bounded world revision history capacity.
    #[must_use]
    pub fn with_history_capacity(mut self, capacity: usize) -> Self {
        self.history_capacity = Some(capacity.max(1));
        self
    }

    /// Sets configured world movement limits.
    #[must_use]
    pub fn with_movement_limits(mut self, max_speed: f64, max_acceleration: f64) -> Self {
        self.max_speed = Some(max_speed);
        self.max_acceleration = Some(max_acceleration);
        self
    }

    /// Sets `world.speed_acceleration_check_enabled` (otherwise `true`, ADR-024).
    #[must_use]
    pub fn with_speed_acceleration_check(mut self, enabled: bool) -> Self {
        self.speed_acceleration_check_enabled = Some(enabled);
        self
    }

    /// Sets the per-instance custom component update rate.
    #[must_use]
    pub fn with_component_updates_per_sec(mut self, per_sec: u32) -> Self {
        self.component_updates_per_sec = Some(per_sec.max(1));
        self
    }

    /// Sets the per-instance mailbox capacities.
    #[must_use]
    pub fn with_mailbox_config(mut self, config: MailboxConfig) -> Self {
        self.mailbox_config = Some(config);
        self
    }

    /// Sets the process metrics recorder used for mailbox saturation gauges.
    #[must_use]
    pub fn with_metrics_recorder(mut self, metrics: Arc<dyn MetricsRecorder>) -> Self {
        self.metrics_recorder = Some(metrics);
        self
    }

    /// Shares process-wide shutdown admission and drain notifications.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: Arc<ShutdownState>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Sets the synchronous pre-commit validation hook (ADR-025). Leaving
    /// this unset (the default) means the entity mutation dispatch path
    /// never attempts a capability lookup or an HTTP call, matching
    /// pre-ADR-025 behavior exactly.
    #[must_use]
    pub fn with_pre_commit_gate(
        mut self,
        gate: Arc<dyn orbisync_extensions::PreCommitGate>,
    ) -> Self {
        self.pre_commit_gate = Some(gate);
        self
    }

    /// Sets the extension registration store used to look up the (at most
    /// one) Active extension subscribed to a pre-commit-hook capability
    /// (ADR-025). Leaving this unset has the same opt-out effect as leaving
    /// `pre_commit_gate` unset.
    #[must_use]
    pub fn with_extension_registrations(
        mut self,
        store: Arc<dyn orbisync_extensions::ExtensionRegistrationStore>,
    ) -> Self {
        self.extension_registrations = Some(store);
        self
    }

    /// Injects an externally owned "spawn hook active" flag (ADR-025 "既知の
    /// 迂回経路"), e.g. so a test can flip it directly without waiting for a
    /// periodic refresh, or so the composition root can share the same
    /// `Arc<AtomicBool>` a background refresh task writes into. Leaving this
    /// unset defaults to a fresh flag starting at `false`.
    #[must_use]
    pub fn with_spawn_hook_active(mut self, flag: Arc<AtomicBool>) -> Self {
        self.spawn_hook_active = Some(flag);
        self
    }

    /// Builds the state.
    #[must_use]
    pub fn build(self) -> RealtimeState {
        let grace = self.resume_grace_seconds.unwrap_or(60);
        let resume_prune_interval_seconds = self.config.resume_prune_interval_seconds;
        let resume_store = self
            .resume_store
            .unwrap_or_else(|| Arc::new(ResumeSessionStore::new(Arc::clone(&self.clock), grace)));
        let permits = self.config.max_connections.max(1) as usize;
        RealtimeState {
            config: self.config,
            registry: self.registry,
            delivery: self.delivery,
            command_dedup: self
                .command_dedup
                .unwrap_or_else(|| Arc::new(crate::command_dedup::CommandDedupStore::default())),
            clock: self.clock,
            default_capacity: self.default_capacity.unwrap_or(100),
            interest_grid: self.interest_grid.unwrap_or_default(),
            world_store: self
                .world_store
                .unwrap_or_else(|| Arc::new(EmptyWorldDirectoryStore)),
            checkpoint_store: self.checkpoint_store,
            checkpoint_limits: self
                .generation
                .as_ref()
                .map_or(self.checkpoint_limits, |service| service.limits),
            generation: self.generation,
            persistent_entity_store: self.persistent_entity_store,
            tickets: self
                .tickets
                .unwrap_or_else(|| Arc::new(DenyAllTicketVerifier::new())),
            world_authorizer: self
                .world_authorizer
                .unwrap_or_else(|| Arc::new(DenyAllWorldAuthorizer)),
            identity_repository: self.identity_repository,
            ephemeral_scope: self.ephemeral_scope,
            resume_store,
            resume_grace_seconds: grace,
            resume_prune_interval_seconds,
            connection_semaphore: Arc::new(Semaphore::new(permits)),
            active_connections: Arc::new(AtomicUsize::new(0)),
            rejected_upgrade_total: Arc::new(AtomicU64::new(0)),
            handshake_timeout_total: Arc::new(AtomicU64::new(0)),
            rate_limit_normal_per_sec: self
                .rate_limit_normal_per_sec
                .unwrap_or(RateLimiter::DEFAULT_NORMAL_PER_SEC),
            rate_limit_custom_per_sec: self
                .rate_limit_custom_per_sec
                .unwrap_or(RateLimiter::DEFAULT_CUSTOM_PER_SEC),
            rate_limit_persistent_threshold: self
                .rate_limit_persistent_threshold
                .unwrap_or(RateLimiter::DEFAULT_PERSISTENT_THRESHOLD),
            history_capacity: self
                .history_capacity
                .unwrap_or(InstanceActor::DEFAULT_HISTORY_CAPACITY),
            max_speed: self
                .max_speed
                .unwrap_or(orbisync_world_runtime::validation::DEFAULT_MAX_SPEED),
            max_acceleration: self
                .max_acceleration
                .unwrap_or(orbisync_world_runtime::validation::DEFAULT_MAX_ACCELERATION),
            speed_acceleration_check_enabled: self.speed_acceleration_check_enabled.unwrap_or(true),
            component_updates_per_sec: self
                .component_updates_per_sec
                .unwrap_or(InstanceActor::DEFAULT_COMPONENT_UPDATES_PER_SEC),
            mailbox_config: self.mailbox_config.unwrap_or_default(),
            metrics_recorder: self
                .metrics_recorder
                .unwrap_or_else(|| Arc::new(NoopMetrics)),
            activation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INSTANCE_ACTIVATIONS)),
            activation_permit_timeout: ACTIVATION_PERMIT_TIMEOUT,
            shutdown: self
                .shutdown
                .unwrap_or_else(|| Arc::new(ShutdownState::new())),
            recovery_tasks: Arc::new(Mutex::new(RecoveryTasks::default())),
            input_rules: self.input_rules,
            pre_commit_gate: self.pre_commit_gate,
            extension_registrations: self.extension_registrations,
            spawn_hook_active: self
                .spawn_hook_active
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
        }
    }
}

impl core::fmt::Debug for RealtimeState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RealtimeState")
            .field("config", &self.config)
            .field("registry", &self.registry)
            .field("delivery", &self.delivery)
            .field("clock", &"dyn Clock")
            .field("default_capacity", &self.default_capacity)
            .field("interest_grid", &self.interest_grid)
            .field("world_store", &"dyn WorldDirectoryStore")
            .field(
                "checkpoint_store",
                &self
                    .checkpoint_store
                    .as_ref()
                    .map(|_| "dyn CheckpointStore"),
            )
            .field(
                "persistent_entity_store",
                &self
                    .persistent_entity_store
                    .as_ref()
                    .map(|_| "dyn PersistentEntityStore"),
            )
            .field("tickets", &"dyn RealtimeTicketVerifier")
            .field("resume_store", &self.resume_store)
            .field("resume_grace_seconds", &self.resume_grace_seconds)
            .field(
                "resume_prune_interval_seconds",
                &self.resume_prune_interval_seconds,
            )
            .field(
                "connection_semaphore",
                &self.connection_semaphore.available_permits(),
            )
            .field(
                "active_connections",
                &self.active_connections.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// RAII guard that reclaims delivery senders on drop and retains
/// `PresenceId` for the resume grace window (W-18, D-26).
///
/// D-26: grace期内は `PresenceId` を解放しない。切断直後に `Leave` を送ると
/// 復帰先の membership が消え条件2が成立しない。代わりに delivery のみを
/// 掃除し、membership は resume token の有効期間だけ保持する。期限切れ後は
/// `ResumeSessionStore` の prune が `Leave` を実行するまで capacity に数え続ける
/// (H-1 の容量テストに影響する場合は報告すること — テストを書き換えない)。
///
/// Early returns before `Join` do not create the guard, so no spurious `Leave` is sent.
struct PresenceGuard {
    delivery: Arc<DeliveryRegistry>,
    instance_id: InstanceId,
    presence: PresenceId,
    resume_store: Arc<ResumeSessionStore>,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        // D-26: do NOT issue Leave here. The presence stays in `InstanceState`
        // for the grace window so resume can rebind the same PresenceId and
        // capacity remains accounted for. The token store holds the binding;
        // when it expires the next join/resume path or the tick reaper will
        // clean the membership.
        self.delivery.cleanup_closed(self.instance_id);
        // Mark the binding so the join path may reclaim this slot if the
        // instance fills up before the grace window ends. Until then the
        // presence stays, so a resume rebinds the same PresenceId.
        self.resume_store.mark_disconnected(self.presence);
        // Intentionally no `actor.handle(Leave)` — see D-26 comment above.
        // For process lifetime tests where the guard is dropped and immediately
        // joined by another connection, capacity will appear full for 60 s;
        // that is the expected trade-off per D-26 and must be reported if
        //existing H-1 tests turn red, not hidden by reverting this.
    }
}

/// Guard that tracks active WebSocket connections for C3 metrics.
struct ActiveConnectionGuard {
    active: Arc<AtomicUsize>,
    metrics: Arc<dyn MetricsRecorder>,
}

fn record_total_members(registry: &RuntimeRegistry, metrics: &dyn MetricsRecorder) {
    let total = registry.member_count();
    metrics.set(
        Gauge::InstanceMembersCurrent,
        i64::try_from(total).unwrap_or(i64::MAX),
    );
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        let current = self
            .active
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        self.metrics.set(
            Gauge::WebsocketConnectionsCurrent,
            i64::try_from(current).unwrap_or(i64::MAX),
        );
        self.metrics.incr(Counter::WebsocketDisconnects {
            reason: WebsocketDisconnectReason::Transport,
        });
        // Do not touch the registry here. This guard can be dropped while a
        // registry lock is still held by the termination path; re-locking it
        // from Drop would self-deadlock. The periodic resume-prune path
        // refreshes the aggregate gauge after releasing its registry lock.
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::RealtimeState;
    use crate::delivery::DeliveryRegistry;
    use orbisync_domain::{Clock, SystemClock};
    use orbisync_world_runtime::RuntimeRegistry;
    use std::future::pending;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    fn state() -> RealtimeState {
        RealtimeState::new(
            orbisync_config::Config::default().realtime,
            Arc::new(RuntimeRegistry::new()),
            Arc::new(DeliveryRegistry::new()),
            Arc::new(SystemClock::new()) as Arc<dyn Clock>,
        )
    }

    #[tokio::test]
    async fn recovery_admission_reclaims_finished_join_handles() {
        let state = state();
        for _ in 0..128 {
            state.spawn_recovery(async {});
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let all_finished = state
                    .recovery_tasks
                    .lock()
                    .expect("recovery task lock")
                    .handles
                    .iter()
                    .all(tokio::task::JoinHandle::is_finished);
                if all_finished {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workers must finish before the next admission");
        assert_eq!(
            state
                .recovery_tasks
                .lock()
                .expect("recovery task lock")
                .handles
                .len(),
            128
        );

        // This admission is the observation point: all 128 completed handles
        // must be collected before the new worker is retained.
        state.spawn_recovery(async {});
        assert_eq!(
            state
                .recovery_tasks
                .lock()
                .expect("recovery task lock")
                .handles
                .len(),
            1
        );
        state.drain_recovery(Duration::from_secs(1)).await;
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn recovery_drain_aborts_overdue_workers_and_closes_admission() {
        let state = state();
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let probe = Arc::clone(&dropped);
        state.spawn_recovery(async move {
            let _probe = DropProbe(probe);
            started_tx.send(()).expect("started receiver");
            pending::<()>().await;
        });
        started_rx.await.expect("worker started");

        state.drain_recovery(Duration::from_millis(20)).await;
        assert!(
            dropped.load(Ordering::Acquire),
            "aborted recovery must run its Drop guard"
        );

        let admitted = Arc::new(AtomicBool::new(false));
        let admitted_worker = Arc::clone(&admitted);
        state.spawn_recovery(async move {
            admitted_worker.store(true, Ordering::Release);
        });
        tokio::task::yield_now().await;
        assert!(
            !admitted.load(Ordering::Acquire),
            "draining must close recovery admission"
        );
    }
}
