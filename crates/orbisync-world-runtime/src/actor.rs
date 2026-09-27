//! Instance actor loop (`state-and-runtime.md` §3).

use crate::{
    InstanceRuntimeDescriptor, InterestSnapshot, RuntimeState,
    checkpoint::Checkpoint,
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
    extension::{ExtensionEvent, Outbox},
    persistence::{EntityPersistenceEvent, PersistenceOutbox},
    state::InstanceState,
    validation::{
        DEFAULT_MAX_ACCELERATION, DEFAULT_MAX_SPEED, validate_transform_update_with_limits,
    },
};
use orbisync_application::EntityOwnershipTransferAudit;
use orbisync_application::metrics::{
    Counter, Gauge, MailboxQueue, MetricsRecorder, NoopMetrics, RateLimitScope,
};
use orbisync_domain::{EntityId, PresenceId, Revision, Timestamp, UserId, Vec3, VisibilityPolicy};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

#[path = "admission.rs"]
mod admission;
pub(crate) use admission::GenerationInterestIndex;
pub use admission::{AdmissionResult, GenerationBinding};

/// Configurable capacities for one instance's bounded mailbox lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxConfig {
    /// Capacity for join, leave, administrative commands and shutdown.
    pub control_capacity: usize,
    /// Capacity for latest-wins transform inputs.
    pub transform_capacity: usize,
    /// Capacity for entity and component commands.
    pub entity_capacity: usize,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self {
            control_capacity: 64,
            transform_capacity: 256,
            entity_capacity: 256,
        }
    }
}

impl MailboxConfig {
    fn normalized(self) -> Self {
        Self {
            control_capacity: self.control_capacity.max(1),
            transform_capacity: self.transform_capacity.max(1),
            entity_capacity: self.entity_capacity.max(1),
        }
    }
}

/// Mailbox lane used in send errors and queue metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxLane {
    /// Control lane.
    Control,
    /// Transform lane.
    Transform,
    /// Entity lane.
    Entity,
}

impl MailboxLane {
    const fn metric_queue(self) -> MailboxQueue {
        match self {
            Self::Control => MailboxQueue::Control,
            Self::Transform => MailboxQueue::Transform,
            Self::Entity => MailboxQueue::Entity,
        }
    }
}

/// Error returned when a mailbox send cannot be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxSendError {
    /// The bounded lane is full.
    Full {
        /// Lane that is saturated.
        lane: MailboxLane,
    },
    /// The lane has no receiver (the actor has stopped).
    Closed {
        /// Lane whose receiver is closed.
        lane: MailboxLane,
    },
}

/// Result of enqueuing a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxEnqueue {
    /// A new item occupied a queue slot.
    Enqueued,
    /// An older transform for the same entity was replaced.
    Coalesced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TransformKey(EntityId);

struct Mailbox {
    control_tx: SyncSender<InstanceCommand>,
    control_rx: Receiver<InstanceCommand>,
    transform_tx: SyncSender<TransformKey>,
    transform_rx: Receiver<TransformKey>,
    transform_pending: HashMap<TransformKey, InstanceCommand>,
    entity_tx: SyncSender<InstanceCommand>,
    entity_rx: Receiver<InstanceCommand>,
    control_depth: usize,
    transform_depth: usize,
    entity_depth: usize,
    metrics: Arc<dyn MetricsRecorder>,
}

impl std::fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailbox")
            .field("control_depth", &self.control_depth)
            .field("transform_depth", &self.transform_depth)
            .field("entity_depth", &self.entity_depth)
            .finish_non_exhaustive()
    }
}

impl Mailbox {
    fn new(config: MailboxConfig, metrics: Arc<dyn MetricsRecorder>) -> Self {
        let config = config.normalized();
        let (control_tx, control_rx) = sync_channel(config.control_capacity);
        let (transform_tx, transform_rx) = sync_channel(config.transform_capacity);
        let (entity_tx, entity_rx) = sync_channel(config.entity_capacity);
        let mailbox = Self {
            control_tx,
            control_rx,
            transform_tx,
            transform_rx,
            transform_pending: HashMap::new(),
            entity_tx,
            entity_rx,
            control_depth: 0,
            transform_depth: 0,
            entity_depth: 0,
            metrics,
        };
        mailbox.record_depth();
        mailbox
    }

    fn record_depth(&self) {
        self.metrics.set(
            Gauge::InstanceCommandQueueDepth {
                queue: MailboxQueue::Control,
            },
            self.control_depth as i64,
        );
        self.metrics.set(
            Gauge::InstanceCommandQueueDepth {
                queue: MailboxQueue::Transform,
            },
            self.transform_depth as i64,
        );
        self.metrics.set(
            Gauge::InstanceCommandQueueDepth {
                queue: MailboxQueue::Entity,
            },
            self.entity_depth as i64,
        );
    }

    fn saturated(&self, lane: MailboxLane) {
        self.metrics.incr(Counter::InstanceMailboxSaturated {
            queue: lane.metric_queue(),
        });
    }

    fn dropped(&self, lane: MailboxLane) {
        self.metrics.incr(Counter::InstanceMailboxDropped {
            queue: lane.metric_queue(),
        });
    }

    fn try_send(&mut self, command: InstanceCommand) -> Result<MailboxEnqueue, MailboxSendError> {
        match command {
            InstanceCommand::UpdateTransform { entity_id, .. } => {
                let key = TransformKey(entity_id);
                if let Some(previous) = self.transform_pending.get_mut(&key) {
                    *previous = command;
                    self.dropped(MailboxLane::Transform);
                    self.record_depth();
                    return Ok(MailboxEnqueue::Coalesced);
                }
                match self.transform_tx.try_send(key) {
                    Ok(()) => {
                        self.transform_pending.insert(key, command);
                        self.transform_depth += 1;
                        self.record_depth();
                        Ok(MailboxEnqueue::Enqueued)
                    }
                    Err(TrySendError::Full(_)) => {
                        self.saturated(MailboxLane::Transform);
                        self.record_depth();
                        Err(MailboxSendError::Full {
                            lane: MailboxLane::Transform,
                        })
                    }
                    Err(TrySendError::Disconnected(_)) => Err(MailboxSendError::Closed {
                        lane: MailboxLane::Transform,
                    }),
                }
            }
            command
                if matches!(
                    command,
                    InstanceCommand::Join { .. }
                        | InstanceCommand::Leave { .. }
                        | InstanceCommand::Shutdown
                        | InstanceCommand::Tick
                ) =>
            {
                match self.control_tx.try_send(command) {
                    Ok(()) => {
                        self.control_depth += 1;
                        self.record_depth();
                        Ok(MailboxEnqueue::Enqueued)
                    }
                    Err(TrySendError::Full(_)) => {
                        self.saturated(MailboxLane::Control);
                        self.record_depth();
                        Err(MailboxSendError::Full {
                            lane: MailboxLane::Control,
                        })
                    }
                    Err(TrySendError::Disconnected(_)) => Err(MailboxSendError::Closed {
                        lane: MailboxLane::Control,
                    }),
                }
            }
            command => match self.entity_tx.try_send(command) {
                Ok(()) => {
                    self.entity_depth += 1;
                    self.record_depth();
                    Ok(MailboxEnqueue::Enqueued)
                }
                Err(TrySendError::Full(_)) => {
                    self.saturated(MailboxLane::Entity);
                    self.record_depth();
                    Err(MailboxSendError::Full {
                        lane: MailboxLane::Entity,
                    })
                }
                Err(TrySendError::Disconnected(_)) => Err(MailboxSendError::Closed {
                    lane: MailboxLane::Entity,
                }),
            },
        }
    }

    fn pop_next(&mut self) -> Option<InstanceCommand> {
        // Shutdown is a control command but has priority over older control
        // work. Draining the bounded lane here is intentional: shutdown is an
        // admission boundary, so queued work must not run after it.
        let mut control: VecDeque<InstanceCommand> = self.control_rx.try_iter().collect();
        self.control_depth = 0;
        if let Some(index) = control
            .iter()
            .position(|command| matches!(command, InstanceCommand::Shutdown))
        {
            let shutdown = control.remove(index);
            self.transform_pending.clear();
            while self.transform_rx.try_recv().is_ok() {}
            self.transform_depth = 0;
            while self.entity_rx.try_recv().is_ok() {}
            self.entity_depth = 0;
            self.record_depth();
            return shutdown;
        }
        if let Some(command) = control.pop_front() {
            // Preserve control ordering without making the unbounded-looking
            // `try_iter` result part of the mailbox: all items are reinserted
            // into the same bounded channel.
            // The send cannot fail because these items came from this channel.
            for pending in control {
                if self.control_tx.try_send(pending).is_ok() {
                    self.control_depth += 1;
                }
            }
            self.record_depth();
            return Some(command);
        }
        if let Ok(key) = self.transform_rx.try_recv() {
            let command = self.transform_pending.remove(&key);
            self.transform_depth = self.transform_depth.saturating_sub(1);
            self.record_depth();
            return command;
        }
        let command = self.entity_rx.try_recv().ok();
        if command.is_some() {
            self.entity_depth = self.entity_depth.saturating_sub(1);
        }
        self.record_depth();
        command
    }
}

/// Lightweight interest view for the delivery hot path (N-5).
///
/// Contains only the four fields required for interest filtering
/// (`id`, `owner`, `position`, `visibility`) and avoids cloning the
/// heavy `Entity.components` map per message per receiver. The view is
/// shared through an indexed `InterestSnapshot` that receivers clone in O(1)
/// (`with_avatar_visibility_radius` documents the `f32` cast).
#[derive(Debug, Clone, PartialEq)]
pub struct EntityInterestView {
    /// Entity identifier.
    pub id: EntityId,
    /// Owning user, if any.
    pub owner: Option<UserId>,
    /// World position, if the entity has a transform.
    pub position: Option<Vec3>,
    /// Visibility policy, sharing its role/user sets through `Arc`.
    pub visibility: VisibilityPolicy,
}

impl EntityInterestView {
    /// Builds a view from an `Entity`.
    #[must_use]
    pub fn from_entity(entity: &orbisync_domain::Entity) -> Self {
        Self {
            id: entity.id(),
            owner: entity.owner(),
            position: entity.transform().map(|t| t.position()),
            visibility: entity.visibility().clone(),
        }
    }
}

/// The instance actor, owning ephemeral state and serializing commands.
#[derive(Debug)]
pub struct InstanceActor {
    admission: Option<admission::AdmissionState>,
    descriptor: InstanceRuntimeDescriptor,
    state: InstanceState,
    outbox: Outbox,
    persistence_outbox: PersistenceOutbox,
    avatar_visibility_radius: f32,
    interest_views: Arc<InterestSnapshot>,
    /// Bounded revision history for resume resync decision (W-18, D-25).
    ///
    /// MRIB-03 leaves history retention undecided; a new config key is forbidden
    /// by D-25, so the capacity is derived from the existing
    /// `realtime.outbound_queue_capacity` (default 256) via injection from the
    /// composition root.  Each applied revision pushes one entry; oldest is evicted
    /// when full.  This keeps the window bounded without unbounded memory and
    /// mirrors the per-connection queue bound.  Future MRIB-03 work will tune the
    /// value precisely; until then this is the deliberate D-25 trade-off.
    history: VecDeque<Revision>,
    history_capacity: usize,
    reliable_events: Arc<VecDeque<(Revision, Arc<[u8]>)>>,
    reliable_ids: VecDeque<String>,
    reliable_by_id: HashMap<String, Arc<[u8]>>,
    reliable_floor: Revision,
    reliable_bytes: usize,
    max_speed: f64,
    max_acceleration: f64,
    /// `world.speed_acceleration_check_enabled` (ADR-024). Gates only the
    /// speed/acceleration comparison in `validate_transform_update_with_limits`;
    /// ownership, revision, distance, and finiteness checks are unaffected.
    speed_acceleration_check_enabled: bool,
    component_updates_per_sec: u32,
    component_update_tokens: f64,
    component_update_last_refill: Option<Timestamp>,
    mailbox: Mailbox,
    /// Shared, live-updating "does an Active `hooks:entity:spawn`
    /// registration currently exist" flag (ADR-025 "既知の迂回経路").
    ///
    /// A plain `Arc<AtomicBool>` read, not a value copied once at
    /// activation: the composition root and every already-running actor
    /// share the same `Arc`, so a registration added, suspended, or removed
    /// after this instance activated is reflected the next time this actor
    /// checks it (bounded by the periodic refresh interval), not only at
    /// the next restart. Reading it is a single atomic load — no I/O,
    /// consistent with `InstanceActor::handle`'s No-I/O invariant.
    spawn_hook_active: Arc<std::sync::atomic::AtomicBool>,
}

impl InstanceActor {
    /// Default spatial visibility radius for auto-created avatars, in meters.
    ///
    /// Matches `InterestConfig::near_radius`'s own default. The composition
    /// root overrides it via [`Self::with_avatar_visibility_radius`] so the
    /// deployed `interest.near_radius` actually takes effect (D-2); the
    /// constant only serves callers that have no configuration, such as tests.
    pub const DEFAULT_AVATAR_VISIBILITY_RADIUS: f32 = 30.0;
    /// Default per-instance custom component update rate.
    pub const DEFAULT_COMPONENT_UPDATES_PER_SEC: u32 = 10;

    /// Creates an actor in `Starting` state with the default avatar radius.
    #[must_use]
    pub fn new(descriptor: InstanceRuntimeDescriptor) -> Self {
        Self::with_avatar_visibility_radius(descriptor, Self::DEFAULT_AVATAR_VISIBILITY_RADIUS)
    }

    /// Creates an actor whose auto-created avatars use `radius` for their
    /// [`orbisync_domain::VisibilityPolicy::Spatial`] policy.
    ///
    /// `world-runtime` may depend on neither `config` nor `interest`
    /// (`repo-crate-conventions.md` §3.2), so the value is injected by the
    /// composition root from `interest.near_radius` (D-2). A non-finite or
    /// non-positive `radius` falls back to
    /// [`Self::DEFAULT_AVATAR_VISIBILITY_RADIUS`].
    #[must_use]
    pub fn with_avatar_visibility_radius(
        descriptor: InstanceRuntimeDescriptor,
        radius: f32,
    ) -> Self {
        Self::with_history_capacity(descriptor, radius, Self::DEFAULT_HISTORY_CAPACITY)
    }

    /// D-25: history capacity derived from `realtime.outbound_queue_capacity`.
    ///
    /// `world-runtime` must not depend on `config`, so the composition root
    /// injects the value.  The default matches the config default (256) so
    /// tests that call `new` still have a bounded history.
    pub const DEFAULT_HISTORY_CAPACITY: usize = Outbox::DEFAULT_CAPACITY;

    /// Creates an actor with explicit avatar radius and history capacity (W-18).
    #[must_use]
    pub fn with_history_capacity(
        descriptor: InstanceRuntimeDescriptor,
        radius: f32,
        history_capacity: usize,
    ) -> Self {
        Self::with_runtime_limits(
            descriptor,
            radius,
            history_capacity,
            DEFAULT_MAX_SPEED,
            DEFAULT_MAX_ACCELERATION,
        )
    }

    /// Creates an actor with configured movement limits.
    #[must_use]
    pub fn with_runtime_limits(
        descriptor: InstanceRuntimeDescriptor,
        radius: f32,
        history_capacity: usize,
        max_speed: f64,
        max_acceleration: f64,
    ) -> Self {
        let state = InstanceState::new(descriptor.instance_id);
        let avatar_visibility_radius = if radius.is_finite() && radius > 0.0 {
            radius
        } else {
            Self::DEFAULT_AVATAR_VISIBILITY_RADIUS
        };
        let history_capacity = if history_capacity == 0 {
            Self::DEFAULT_HISTORY_CAPACITY
        } else {
            history_capacity
        };
        Self {
            admission: None,
            descriptor,
            state,
            outbox: Outbox::with_capacity(history_capacity),
            persistence_outbox: PersistenceOutbox::with_capacity(history_capacity),
            avatar_visibility_radius,
            interest_views: Arc::new(InterestSnapshot::default()),
            history: VecDeque::with_capacity(history_capacity),
            history_capacity,
            reliable_events: Arc::new(VecDeque::new()),
            reliable_ids: VecDeque::new(),
            reliable_by_id: HashMap::new(),
            reliable_floor: descriptor.revision,
            reliable_bytes: 0,
            max_speed: if max_speed.is_finite() && max_speed > 0.0 {
                max_speed
            } else {
                DEFAULT_MAX_SPEED
            },
            max_acceleration: if max_acceleration.is_finite() && max_acceleration > 0.0 {
                max_acceleration
            } else {
                DEFAULT_MAX_ACCELERATION
            },
            speed_acceleration_check_enabled: true,
            component_updates_per_sec: Self::DEFAULT_COMPONENT_UPDATES_PER_SEC,
            component_update_tokens: Self::DEFAULT_COMPONENT_UPDATES_PER_SEC as f64,
            component_update_last_refill: None,
            mailbox: Mailbox::new(MailboxConfig::default(), Arc::new(NoopMetrics)),
            spawn_hook_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Sets the per-instance custom component update rate.
    ///
    /// The instance actor owns this limiter because it already serializes
    /// component commands and enforces entity ownership. Rejections are
    /// returned as command outcomes, so the transport can keep the connection
    /// alive.
    #[must_use]
    pub fn with_component_updates_per_sec(mut self, per_sec: u32) -> Self {
        let per_sec = per_sec.max(1);
        self.component_updates_per_sec = per_sec;
        self.component_update_tokens = per_sec as f64;
        self.component_update_last_refill = None;
        self
    }

    /// Enables or disables the speed/acceleration kinematics check
    /// (`world.speed_acceleration_check_enabled`, ADR-024).
    ///
    /// When `false`, `UpdateTransform` no longer rejects updates for
    /// exceeding `max_speed`/`max_acceleration`. Every other invariant stays
    /// enforced regardless of this setting: ownership, `expected_revision`,
    /// the per-tick teleport distance limit, and numeric finiteness. This is
    /// a use-case policy toggle, not a relaxation of Core's server-authority
    /// guarantees.
    #[must_use]
    pub fn with_speed_acceleration_check(mut self, enabled: bool) -> Self {
        self.speed_acceleration_check_enabled = enabled;
        self
    }

    /// Shares a live-updating "spawn hook active" flag with this actor
    /// (ADR-025 "既知の迂回経路"). See the field doc comment on
    /// `Self::spawn_hook_active`'s declaration for why this takes a
    /// shared `Arc` rather than a `bool` snapshot.
    #[must_use]
    pub fn with_spawn_hook_active(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.spawn_hook_active = flag;
        self
    }

    /// Creates an actor with configured mailbox capacities and metrics.
    #[must_use]
    pub fn with_mailbox_config(
        descriptor: InstanceRuntimeDescriptor,
        radius: f32,
        history_capacity: usize,
        max_speed: f64,
        max_acceleration: f64,
        mailbox_config: MailboxConfig,
        metrics: Arc<dyn MetricsRecorder>,
    ) -> Self {
        let mut actor = Self::with_runtime_limits(
            descriptor,
            radius,
            history_capacity,
            max_speed,
            max_acceleration,
        );
        actor.mailbox = Mailbox::new(mailbox_config, metrics);
        actor
    }

    /// Restores an actor from a validated durable checkpoint with configured
    /// runtime and mailbox limits. Live membership and ephemeral entity fields
    /// are intentionally not restored.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when checkpoint entities are inconsistent
    /// with the checkpoint instance or revision.
    #[allow(clippy::too_many_arguments)]
    pub fn from_checkpoint_with_mailbox_config(
        checkpoint: Checkpoint,
        radius: f32,
        history_capacity: usize,
        max_speed: f64,
        max_acceleration: f64,
        mailbox_config: MailboxConfig,
        metrics: Arc<dyn MetricsRecorder>,
    ) -> Result<Self, orbisync_domain::DomainError> {
        let descriptor = InstanceRuntimeDescriptor {
            instance_id: checkpoint.instance_id,
            state: RuntimeState::Starting,
            revision: checkpoint.revision,
        };
        let state = InstanceState::from_persisted(
            checkpoint.instance_id,
            checkpoint.revision,
            checkpoint.entities,
        )?;
        let mut actor = Self::with_mailbox_config(
            descriptor,
            radius,
            history_capacity,
            max_speed,
            max_acceleration,
            mailbox_config,
            metrics,
        );
        actor.state = state;
        actor.rebuild_interest_views();
        Ok(actor)
    }

    /// Attempts to put one command on this instance's bounded mailbox.
    ///
    /// Transform inputs are latest-wins by entity: replacing a queued input
    /// increments the dropped counter but does not consume another slot.
    /// Call [`Self::process_mailbox`] to apply queued work in priority order.
    pub fn try_send(
        &mut self,
        command: InstanceCommand,
    ) -> Result<MailboxEnqueue, MailboxSendError> {
        self.mailbox.try_send(command)
    }

    /// Applies one queued command, prioritizing shutdown over other control
    /// commands, then control, transform and entity work.
    pub fn process_mailbox(&mut self) -> Option<CommandOutcome> {
        self.mailbox.pop_next().map(|command| self.handle(command))
    }

    /// Enqueues a command and drains the mailbox so synchronous adapters can
    /// await the command outcome without bypassing the mailbox boundary.
    pub fn submit(&mut self, command: InstanceCommand) -> Result<CommandOutcome, MailboxSendError> {
        self.try_send(command)?;
        let mut outcome = None;
        while let Some(next) = self.process_mailbox() {
            outcome = Some(next);
        }
        // A successful send always produces one item, except for a closed
        // receiver (which is reported by try_send), so this branch is only a
        // defensive rejection if mailbox internals are changed later.
        outcome.ok_or(MailboxSendError::Closed {
            lane: MailboxLane::Control,
        })
    }

    fn component_update_allowed(&mut self, now: Timestamp) -> bool {
        let rate = self.component_updates_per_sec as f64;
        if let Some(last_refill) = self.component_update_last_refill {
            if let (Ok(now_millis), Ok(last_millis)) =
                (now.to_unix_millis(), last_refill.to_unix_millis())
            {
                let elapsed_millis = now_millis.saturating_sub(last_millis);
                if elapsed_millis > 0 {
                    let elapsed_seconds = elapsed_millis as f64 / 1000.0;
                    self.component_update_tokens =
                        (self.component_update_tokens + elapsed_seconds * rate).min(rate);
                    self.component_update_last_refill = Some(now);
                }
            }
        } else {
            self.component_update_last_refill = Some(now);
            self.component_update_tokens = rate;
        }

        if self.component_update_tokens >= 1.0 {
            self.component_update_tokens -= 1.0;
            true
        } else {
            self.mailbox.metrics.incr(Counter::RateLimitRejected {
                scope: RateLimitScope::Instance,
            });
            false
        }
    }

    /// Returns the current depth of each bounded lane.
    #[must_use]
    pub fn mailbox_depth(&self) -> (usize, usize, usize) {
        (
            self.mailbox.control_depth,
            self.mailbox.transform_depth,
            self.mailbox.entity_depth,
        )
    }

    /// Builds the index once when restoring a complete checkpoint.
    fn rebuild_interest_views(&mut self) {
        let views: InterestSnapshot = self
            .state
            .iter_entities()
            .map(EntityInterestView::from_entity)
            .collect();
        self.interest_views = Arc::new(views);
    }

    fn refresh_interest_view(&mut self, id: EntityId) {
        let view = self
            .state
            .get_entity(id)
            .map(EntityInterestView::from_entity);
        // Component-only updates do not alter visibility or position.
        if self.interest_views.get(id) == view.as_ref() {
            return;
        }
        let snapshot = Arc::make_mut(&mut self.interest_views);
        if let Some(view) = view {
            snapshot.insert(view);
        } else {
            snapshot.remove(id);
        }
    }

    /// Returns descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> InstanceRuntimeDescriptor {
        self.descriptor
    }

    /// Transitions to running.
    pub fn start(&mut self) {
        if !self.generation_start_allowed() {
            return;
        }
        self.descriptor.state = RuntimeState::Running;
    }

    /// Returns a view of the extension outbox.
    #[must_use]
    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// Returns mutable outbox.
    pub(crate) fn outbox_mut(&mut self) -> &mut Outbox {
        &mut self.outbox
    }

    /// Drains deliverable extension events in insertion order.
    /// Generation actors release only the committed prefix.
    #[must_use]
    pub fn drain_outbox(&mut self) -> Vec<ExtensionEvent> {
        if !self.generation_reads_available() {
            return Vec::new();
        }
        if let Some(state) = &mut self.admission {
            return self
                .outbox
                .drain_prefix(core::mem::take(&mut state.committed_events));
        }
        self.outbox.drain()
    }

    /// Records an extension event and reports a bounded-outbox eviction.
    fn push_extension_event(&mut self, event: ExtensionEvent) {
        if self.outbox.push(event) {
            self.mailbox
                .metrics
                .incr(Counter::ExtensionOutboxDroppedTotal);
        }
    }

    /// Returns mutable access to the persistence outbox (HIGH-002).
    ///
    /// Used by the registry to republish drained-but-unpersisted events when
    /// an idle reap is aborted.
    pub(crate) fn persistence_outbox_mut(&mut self) -> &mut PersistenceOutbox {
        &mut self.persistence_outbox
    }

    /// Drains deliverable persistence effects in insertion order (HIGH-002).
    /// Generation actors release only the committed prefix.
    ///
    /// Called once per tick by the coordinator, alongside
    /// [`Self::drain_outbox`], so entity/component writes are batched at
    /// tick cadence instead of issued once per command.
    #[must_use]
    pub fn drain_persistence_batch(&mut self) -> Vec<EntityPersistenceEvent> {
        if !self.generation_reads_available() {
            return Vec::new();
        }
        if let Some(state) = &mut self.admission {
            return self
                .persistence_outbox
                .drain_prefix(core::mem::take(&mut state.committed_persistence_events));
        }
        self.persistence_outbox.drain()
    }

    /// Records a persistence effect to be written by the coordinator.
    fn push_persistence_event(&mut self, event: EntityPersistenceEvent) {
        self.persistence_outbox.push(event);
    }

    /// Returns true when the authenticated requester may mutate an entity.
    ///
    /// The order is security-sensitive: an entity without an owner is
    /// server-only, `entity.update.any` applies next, and only then may the
    /// owner use `entity.update.own`.
    fn is_owner_allowed(
        owner: Option<UserId>,
        requester: UserId,
        permissions: WorldPermissions,
    ) -> bool {
        if owner.is_none() {
            return false;
        }
        if permissions.entity_update_any {
            return true;
        }
        permissions.entity_update_own && owner == Some(requester)
    }

    /// Handles one command, returning the outcome. No I/O is performed here;
    /// the caller (coordinator) distributes the resulting state view.
    pub fn handle(&mut self, command: InstanceCommand) -> CommandOutcome {
        if self.admission.is_some() {
            return self.admit(command, None).outcome;
        }
        self.handle_legacy(command)
    }

    fn handle_legacy(&mut self, command: InstanceCommand) -> CommandOutcome {
        if !self.descriptor.state.accepts_commands()
            && !matches!(command, InstanceCommand::Shutdown)
        {
            return CommandOutcome::Rejected {
                code: "INSTANCE_NOT_RUNNING",
                detail: format!("instance is {}", self.descriptor.state),
            };
        }
        let changed_entity = match &command {
            InstanceCommand::Join { .. }
            | InstanceCommand::RetainReliable { .. }
            | InstanceCommand::Leave { .. }
            | InstanceCommand::PublishEvent { .. }
            | InstanceCommand::Tick
            | InstanceCommand::Shutdown => None,
            InstanceCommand::UpdateTransform { entity_id, .. }
            | InstanceCommand::WithExpectedEntityState { entity_id, .. }
            | InstanceCommand::SpawnEntity { entity_id, .. }
            | InstanceCommand::DeleteEntity { entity_id, .. }
            | InstanceCommand::UpdateEntityComponent { entity_id, .. }
            | InstanceCommand::TransferOwnership { entity_id, .. } => Some(*entity_id),
        };
        let needs_persistence = match &command {
            InstanceCommand::WithExpectedEntityState { .. }
            | InstanceCommand::SpawnEntity { .. }
            | InstanceCommand::DeleteEntity { .. }
            | InstanceCommand::UpdateEntityComponent { .. }
            | InstanceCommand::TransferOwnership { .. } => true,
            InstanceCommand::UpdateTransform { entity_id, .. } => {
                self.state.get_entity(*entity_id).is_none()
            }
            _ => false,
        };
        if needs_persistence && self.persistence_outbox.len() >= self.persistence_outbox.capacity()
        {
            return CommandOutcome::Rejected {
                code: "PERSISTENCE_BACKPRESSURE",
                detail: "pending entity writes are full; retry after persistence catches up"
                    .to_owned(),
            };
        }
        let outcome = match command {
            InstanceCommand::WithExpectedEntityState {
                entity_id,
                expected,
                command,
            } => {
                let same_target = match command.as_ref() {
                    InstanceCommand::SpawnEntity { entity_id: id, .. }
                    | InstanceCommand::UpdateEntityComponent { entity_id: id, .. }
                    | InstanceCommand::DeleteEntity { entity_id: id, .. } => *id == entity_id,
                    _ => false,
                };
                if !same_target {
                    return CommandOutcome::Rejected {
                        code: "INVALID_ARGUMENT",
                        detail: "invalid pre-commit target".into(),
                    };
                }
                let current = self.state.get_entity(entity_id);
                if current != expected.as_deref() {
                    return CommandOutcome::Rejected {
                        code: if current.is_none() && expected.is_some() {
                            "ENTITY_NOT_FOUND"
                        } else {
                            "REVISION_MISMATCH"
                        },
                        detail: "entity state changed during external validation".into(),
                    };
                }
                return self.handle(*command);
            }
            InstanceCommand::Join {
                presence_id,
                user_id,
                instance_id,
                capacity,
            } => {
                if instance_id != self.state.instance_id() {
                    return CommandOutcome::Rejected {
                        code: "INSTANCE_MISMATCH",
                        detail: String::from("join target does not match actor"),
                    };
                }
                if !self.state.add_member(presence_id, user_id, capacity) {
                    return CommandOutcome::Rejected {
                        code: "INSTANCE_FULL",
                        detail: String::from("instance is full"),
                    };
                }
                match self.state.advance_revision() {
                    Ok(rev) => {
                        self.descriptor.revision = rev;
                        self.push_extension_event(ExtensionEvent::MemberJoined {
                            instance_id: self.state.instance_id(),
                            presence_id,
                            user_id,
                        });
                        CommandOutcome::Applied {
                            revision: rev,
                            entity_revision: None,
                            committed_entity: None,
                        }
                    }
                    Err(e) => CommandOutcome::Rejected {
                        code: "REVISION_OVERFLOW",
                        detail: e.to_string(),
                    },
                }
            }
            InstanceCommand::Leave { presence_id } => {
                let user = self.state.user_for_presence(presence_id);
                self.state.remove_member(presence_id);
                match self.state.advance_revision() {
                    Ok(rev) => {
                        self.descriptor.revision = rev;
                        if let Some(user_id) = user {
                            self.push_extension_event(ExtensionEvent::MemberLeft {
                                instance_id: self.state.instance_id(),
                                presence_id,
                                user_id,
                            });
                        }
                        CommandOutcome::Applied {
                            revision: rev,
                            entity_revision: None,
                            committed_entity: None,
                        }
                    }
                    Err(e) => CommandOutcome::Rejected {
                        code: "REVISION_OVERFLOW",
                        detail: e.to_string(),
                    },
                }
            }
            InstanceCommand::UpdateTransform {
                entity_id,
                transform,
                expected_revision,
                user_id,
                now,
                permissions,
            } => {
                // For M2, entities are auto-created on first transform if missing.
                if let Some(entity) = self.state.get_entity_mut(entity_id) {
                    if !Self::is_owner_allowed(entity.owner(), user_id, permissions) {
                        return CommandOutcome::Rejected {
                            code: "NOT_OWNER",
                            detail: String::from("only owner may update transform"),
                        };
                    }
                    let previous = entity.transform();
                    let last = entity.updated_at();
                    let delta_secs = match (now.to_unix_millis(), last.to_unix_millis()) {
                        (Ok(n), Ok(l)) => ((n - l) as f64 / 1000.0)
                            .clamp(crate::validation::MIN_DELTA_SECONDS, 1.0),
                        _ => crate::validation::MIN_DELTA_SECONDS,
                    };
                    if let Err(e) = validate_transform_update_with_limits(
                        previous,
                        transform,
                        delta_secs,
                        self.max_speed,
                        self.max_acceleration,
                        self.speed_acceleration_check_enabled,
                    ) {
                        return CommandOutcome::Rejected {
                            code: "INVALID_TRANSFORM",
                            detail: e.to_string(),
                        };
                    }
                    match entity.update_transform(expected_revision, transform, now) {
                        Ok(()) => {
                            let entity_rev = entity.revision();
                            match self.state.advance_revision() {
                                Ok(rev) => {
                                    self.descriptor.revision = rev;
                                    self.push_extension_event(ExtensionEvent::EntityUpdated {
                                        instance_id: self.state.instance_id(),
                                        entity_id,
                                    });
                                    CommandOutcome::Applied {
                                        revision: rev,
                                        entity_revision: Some(entity_rev),
                                        committed_entity: self
                                            .state
                                            .get_entity(entity_id)
                                            .cloned()
                                            .map(Box::new),
                                    }
                                }
                                Err(e) => CommandOutcome::Rejected {
                                    code: "REVISION_OVERFLOW",
                                    detail: e.to_string(),
                                },
                            }
                        }
                        Err(e) => {
                            let code = match e.kind() {
                                orbisync_domain::DomainErrorKind::RevisionMismatch => {
                                    "REVISION_MISMATCH"
                                }
                                orbisync_domain::DomainErrorKind::RevisionOverflow => {
                                    "REVISION_OVERFLOW"
                                }
                                _ => "INVALID_VALUE",
                            };
                            CommandOutcome::Rejected {
                                code,
                                detail: e.to_string(),
                            }
                        }
                    }
                } else {
                    if !permissions.entity_spawn {
                        return CommandOutcome::Rejected {
                            code: "PERMISSION_DENIED",
                            detail: String::from("entity.spawn permission is required"),
                        };
                    }
                    // ADR-025 "既知の迂回経路": when an Active
                    // `hooks:entity:spawn` pre-commit registration exists,
                    // the M2 auto-create path below would otherwise let a
                    // client create an unapproved entity without ever
                    // going through `SpawnEntity`'s hook consultation. This
                    // is a single atomic load of a flag shared live with
                    // the composition root (no I/O, no HTTP call on this
                    // 20Hz path) — see `Self::spawn_hook_active`'s doc
                    // comment.
                    if self
                        .spawn_hook_active
                        .load(std::sync::atomic::Ordering::Acquire)
                    {
                        return CommandOutcome::Rejected {
                            code: "EXPLICIT_SPAWN_REQUIRED",
                            detail: String::from(
                                "an active spawn pre-commit hook is registered; \
                                 send SpawnEntity instead of relying on Transform auto-create",
                            ),
                        };
                    }
                    // Auto-create for M2 demo: treat first transform as spawn.
                    // Spatial (not Global): `UniformGrid::is_entity_visible` short-circuits
                    // Global before any distance check, so a Global avatar would disable
                    // interest management entirely (H-6a). Radius is injected from
                    // `interest.near_radius` by the composition root (D-2).
                    let visibility = VisibilityPolicy::spatial(self.avatar_visibility_radius)
                        .unwrap_or(VisibilityPolicy::Global);
                    let entity = orbisync_domain::Entity::new(
                        entity_id,
                        self.state.instance_id(),
                        orbisync_domain::EntityKind::Avatar,
                        Some(user_id),
                        Some(transform),
                        visibility,
                        now,
                    );
                    let persisted_entity = entity.clone();
                    self.state.upsert_entity(entity);
                    let entity_rev = self
                        .state
                        .get_entity(entity_id)
                        .map(|e| e.revision())
                        .unwrap_or(Revision::from_u64(1));
                    match self.state.advance_revision() {
                        Ok(rev) => {
                            self.descriptor.revision = rev;
                            self.push_extension_event(ExtensionEvent::EntitySpawned {
                                instance_id: self.state.instance_id(),
                                entity_id,
                                owner: Some(user_id),
                            });
                            // Auto-create is a spawn, not a transform update: the
                            // entity row and its (empty) components are written
                            // once here; the transform-only branch above never
                            // writes durable state (state-and-runtime.md §1.1).
                            self.push_persistence_event(EntityPersistenceEvent::Spawned(
                                persisted_entity,
                            ));
                            CommandOutcome::Applied {
                                revision: rev,
                                entity_revision: Some(entity_rev),
                                committed_entity: self
                                    .state
                                    .get_entity(entity_id)
                                    .cloned()
                                    .map(Box::new),
                            }
                        }
                        Err(e) => CommandOutcome::Rejected {
                            code: "REVISION_OVERFLOW",
                            detail: e.to_string(),
                        },
                    }
                }
            }
            InstanceCommand::SpawnEntity {
                entity_id,
                kind,
                owner,
                transform,
                visibility,
                requester,
                permissions,
                ..
            } => {
                if self.state.get_entity(entity_id).is_some() {
                    return CommandOutcome::Rejected {
                        code: "ENTITY_EXISTS",
                        detail: String::from("entity already exists"),
                    };
                }
                if !permissions.entity_spawn {
                    return CommandOutcome::Rejected {
                        code: "PERMISSION_DENIED",
                        detail: String::from("entity.spawn permission is required"),
                    };
                }
                let owner = if permissions.entity_update_any {
                    owner.or(Some(requester))
                } else {
                    Some(requester)
                };
                let now = match Timestamp::from_unix_millis(0) {
                    Ok(v) => v,
                    Err(e) => {
                        return CommandOutcome::Rejected {
                            code: "INVALID_TIMESTAMP",
                            detail: e.to_string(),
                        };
                    }
                };
                let entity = orbisync_domain::Entity::new(
                    entity_id,
                    self.state.instance_id(),
                    kind,
                    owner,
                    transform,
                    visibility,
                    now,
                );
                let persisted_entity = entity.clone();
                let entity_revision = entity.revision();
                self.state.upsert_entity(entity);
                match self.state.advance_revision() {
                    Ok(rev) => {
                        self.descriptor.revision = rev;
                        self.push_extension_event(ExtensionEvent::EntitySpawned {
                            instance_id: self.state.instance_id(),
                            entity_id,
                            owner,
                        });
                        self.push_persistence_event(EntityPersistenceEvent::Spawned(
                            persisted_entity,
                        ));
                        CommandOutcome::Applied {
                            revision: rev,
                            entity_revision: Some(entity_revision),
                            committed_entity: self
                                .state
                                .get_entity(entity_id)
                                .cloned()
                                .map(Box::new),
                        }
                    }
                    Err(e) => CommandOutcome::Rejected {
                        code: "REVISION_OVERFLOW",
                        detail: e.to_string(),
                    },
                }
            }
            InstanceCommand::DeleteEntity {
                entity_id,
                expected_revision,
                requester,
                permissions,
                ..
            } => {
                let owner = match self.state.get_entity(entity_id) {
                    Some(e) => e.owner(),
                    None => {
                        return CommandOutcome::Rejected {
                            code: "ENTITY_NOT_FOUND",
                            detail: String::from("entity not found"),
                        };
                    }
                };
                if !Self::is_owner_allowed(owner, requester, permissions) {
                    return CommandOutcome::Rejected {
                        code: "NOT_OWNER",
                        detail: String::from("only owner may delete entity"),
                    };
                }
                let current_rev = match self.state.get_entity(entity_id) {
                    Some(e) => e.revision(),
                    None => {
                        return CommandOutcome::Rejected {
                            code: "ENTITY_NOT_FOUND",
                            detail: String::from("entity not found"),
                        };
                    }
                };
                if let Err(e) = current_rev.ensure_matches(expected_revision) {
                    return CommandOutcome::Rejected {
                        code: "REVISION_MISMATCH",
                        detail: e.to_string(),
                    };
                }
                let committed_entity = self.state.get_entity(entity_id).cloned().map(Box::new);
                self.state.remove_entity(entity_id);
                match self.state.advance_revision() {
                    Ok(rev) => {
                        self.descriptor.revision = rev;
                        let instance_id = self.state.instance_id();
                        self.push_extension_event(ExtensionEvent::EntityDeleted {
                            instance_id,
                            entity_id,
                        });
                        self.push_persistence_event(EntityPersistenceEvent::Deleted {
                            entity_id,
                            instance_id,
                        });
                        CommandOutcome::Applied {
                            revision: rev,
                            entity_revision: None,
                            committed_entity,
                        }
                    }
                    Err(e) => CommandOutcome::Rejected {
                        code: "REVISION_OVERFLOW",
                        detail: e.to_string(),
                    },
                }
            }
            InstanceCommand::UpdateEntityComponent {
                entity_id,
                component_key,
                payload_bytes,
                expected_revision,
                now,
                requester,
                permissions,
                ..
            } => {
                let owner = match self.state.get_entity(entity_id) {
                    Some(e) => e.owner(),
                    None => {
                        return CommandOutcome::Rejected {
                            code: "ENTITY_NOT_FOUND",
                            detail: String::from("entity not found"),
                        };
                    }
                };
                if !Self::is_owner_allowed(owner, requester, permissions) {
                    return CommandOutcome::Rejected {
                        code: "NOT_OWNER",
                        detail: String::from("only owner may update component"),
                    };
                }
                if !self.component_update_allowed(now) {
                    return CommandOutcome::Rejected {
                        code: "COMPONENT_RATE_LIMITED",
                        detail: String::from("component update rate limit exceeded"),
                    };
                }
                let Some(entity) = self.state.get_entity_mut(entity_id) else {
                    return CommandOutcome::Rejected {
                        code: "ENTITY_NOT_FOUND",
                        detail: String::from("entity not found"),
                    };
                };
                let persisted_component_key = component_key.clone();
                let persisted_payload = payload_bytes.clone();
                match entity.update_component(expected_revision, component_key, payload_bytes, now)
                {
                    Ok(()) => {
                        let persisted_revision = entity.revision();
                        let persisted_updated_at = entity.updated_at();
                        match self.state.advance_revision() {
                            Ok(rev) => {
                                self.descriptor.revision = rev;
                                let instance_id = self.state.instance_id();
                                self.push_extension_event(ExtensionEvent::EntityUpdated {
                                    instance_id,
                                    entity_id,
                                });
                                self.push_persistence_event(
                                    EntityPersistenceEvent::ComponentUpserted {
                                        entity_id,
                                        instance_id,
                                        revision: persisted_revision,
                                        updated_at: persisted_updated_at,
                                        component_key: persisted_component_key,
                                        payload: persisted_payload,
                                    },
                                );
                                CommandOutcome::Applied {
                                    revision: rev,
                                    entity_revision: Some(persisted_revision),
                                    committed_entity: self
                                        .state
                                        .get_entity(entity_id)
                                        .cloned()
                                        .map(Box::new),
                                }
                            }
                            Err(e) => CommandOutcome::Rejected {
                                code: "REVISION_OVERFLOW",
                                detail: e.to_string(),
                            },
                        }
                    }
                    Err(e) => {
                        let code = match e.kind() {
                            orbisync_domain::DomainErrorKind::RevisionMismatch => {
                                "REVISION_MISMATCH"
                            }
                            orbisync_domain::DomainErrorKind::RevisionOverflow => {
                                "REVISION_OVERFLOW"
                            }
                            _ => "INVALID_COMPONENT",
                        };
                        CommandOutcome::Rejected {
                            code,
                            detail: e.to_string(),
                        }
                    }
                }
            }
            InstanceCommand::TransferOwnership {
                command_id,
                entity_id,
                new_owner,
                expected_revision,
                now,
                requester,
                permissions,
            } => {
                let owner = match self.state.get_entity(entity_id) {
                    Some(e) => e.owner(),
                    None => {
                        return CommandOutcome::Rejected {
                            code: "ENTITY_NOT_FOUND",
                            detail: String::from("entity not found"),
                        };
                    }
                };
                if !Self::is_owner_allowed(owner, requester, permissions) {
                    return CommandOutcome::Rejected {
                        code: "NOT_OWNER",
                        detail: String::from("only owner may transfer ownership"),
                    };
                }
                if new_owner == owner {
                    return CommandOutcome::Rejected {
                        code: "OWNER_UNCHANGED",
                        detail: String::from("new owner must differ from the current owner"),
                    };
                }
                if let Some(target) = new_owner
                    && !self
                        .state
                        .members_snapshot()
                        .iter()
                        .any(|(_, member)| *member == target)
                {
                    return CommandOutcome::Rejected {
                        code: "TARGET_NOT_PRESENT",
                        detail: String::from(
                            "new owner must be a live member of the same instance",
                        ),
                    };
                }
                let previous_owner = owner;
                let Some(entity) = self.state.get_entity_mut(entity_id) else {
                    return CommandOutcome::Rejected {
                        code: "ENTITY_NOT_FOUND",
                        detail: String::from("entity not found"),
                    };
                };
                match entity.transfer_owner(expected_revision, new_owner, now) {
                    Ok(()) => {
                        let persisted_entity = entity.clone();
                        let entity_revision = entity.revision();
                        match self.state.advance_revision() {
                            Ok(rev) => {
                                self.descriptor.revision = rev;
                                self.push_extension_event(ExtensionEvent::OwnershipTransferred {
                                    instance_id: self.state.instance_id(),
                                    entity_id,
                                    previous_owner,
                                    new_owner,
                                });
                                self.push_persistence_event(
                                    EntityPersistenceEvent::OwnershipTransferred {
                                        entity: persisted_entity.clone(),
                                        audit: EntityOwnershipTransferAudit {
                                            occurred_at: now,
                                            actor_id: requester,
                                            command_id,
                                            previous_owner,
                                            new_owner,
                                        },
                                    },
                                );
                                CommandOutcome::Applied {
                                    revision: rev,
                                    entity_revision: Some(entity_revision),
                                    committed_entity: Some(Box::new(persisted_entity)),
                                }
                            }
                            Err(e) => CommandOutcome::Rejected {
                                code: "REVISION_OVERFLOW",
                                detail: e.to_string(),
                            },
                        }
                    }
                    Err(e) => {
                        let code = match e.kind() {
                            orbisync_domain::DomainErrorKind::RevisionMismatch => {
                                "REVISION_MISMATCH"
                            }
                            orbisync_domain::DomainErrorKind::RevisionOverflow => {
                                "REVISION_OVERFLOW"
                            }
                            _ => "INVALID_VALUE",
                        };
                        CommandOutcome::Rejected {
                            code,
                            detail: e.to_string(),
                        }
                    }
                }
            }
            InstanceCommand::RetainReliable {
                message_id,
                revision,
                payload,
            } => {
                const MAX_REPLAY_BYTES: usize = 16 * 1024 * 1024;
                while !self.reliable_events.is_empty()
                    && (self.reliable_events.len() >= self.history_capacity
                        || self.reliable_bytes.saturating_add(payload.len()) > MAX_REPLAY_BYTES)
                {
                    if let Some((evicted, bytes)) =
                        Arc::make_mut(&mut self.reliable_events).pop_front()
                    {
                        self.reliable_floor = evicted;
                        self.reliable_bytes -= bytes.len();
                        if let Some(id) = self.reliable_ids.pop_front() {
                            self.reliable_by_id.remove(&id);
                        }
                    }
                }
                if payload.len() > MAX_REPLAY_BYTES {
                    self.reliable_floor = revision;
                } else {
                    self.reliable_bytes += payload.len();
                    self.reliable_by_id
                        .insert(message_id.clone(), Arc::clone(&payload));
                    self.reliable_ids.push_back(message_id);
                    Arc::make_mut(&mut self.reliable_events).push_back((revision, payload));
                }
                // Retention is bookkeeping, not a new mutation or revision.
                return CommandOutcome::Applied {
                    revision,
                    entity_revision: None,
                    committed_entity: None,
                };
            }
            InstanceCommand::PublishEvent {
                presence_id,
                user_id,
            } => {
                if self.state.user_for_presence(presence_id) != Some(user_id) {
                    return CommandOutcome::Rejected {
                        code: "NOT_JOINED",
                        detail: "event sender is not a member of this instance".to_owned(),
                    };
                }
                match self.state.advance_revision() {
                    Ok(revision) => {
                        self.descriptor.revision = revision;
                        CommandOutcome::Applied {
                            revision,
                            entity_revision: None,
                            committed_entity: None,
                        }
                    }
                    Err(error) => CommandOutcome::Rejected {
                        code: "REVISION_OVERFLOW",
                        detail: error.to_string(),
                    },
                }
            }
            InstanceCommand::Tick => match self.state.advance_revision() {
                Ok(rev) => {
                    self.descriptor.revision = rev;
                    CommandOutcome::Applied {
                        revision: rev,
                        entity_revision: None,
                        committed_entity: None,
                    }
                }
                Err(e) => CommandOutcome::Rejected {
                    code: "REVISION_OVERFLOW",
                    detail: e.to_string(),
                },
            },
            InstanceCommand::Shutdown => {
                self.descriptor.state = RuntimeState::Draining;
                match self.state.advance_revision() {
                    Ok(rev) => {
                        self.descriptor.revision = rev;
                        CommandOutcome::Applied {
                            revision: rev,
                            entity_revision: None,
                            committed_entity: None,
                        }
                    }
                    Err(e) => CommandOutcome::Rejected {
                        code: "REVISION_OVERFLOW",
                        detail: e.to_string(),
                    },
                }
            }
        };
        // N-5: refresh cached interest views when entities may have changed.
        // Membership, ticks and shutdown do not change entity interest views.
        // Keep their existing Arc even when these commands advance revision.
        if matches!(outcome, CommandOutcome::Applied { .. }) {
            if let Some(id) = changed_entity {
                self.refresh_interest_view(id);
            }
            // W-18, D-25: retain bounded revision history for decide_resync.
            // Each applied revision is pushed; eviction keeps memory bounded to
            // `history_capacity` (derived from realtime.outbound_queue_capacity).
            if let CommandOutcome::Applied { revision, .. } = outcome {
                self.push_history(revision);
            }
        }
        outcome
    }

    fn push_history(&mut self, revision: Revision) {
        if self.history.len() >= self.history_capacity {
            self.history.pop_front();
        }
        self.history.push_back(revision);
    }

    /// Interval between checkpoints in ticks.
    ///
    /// Default production interval: five minutes at 1 Hz.
    pub const CHECKPOINT_INTERVAL_TICKS: u64 = 300;

    /// Production checkpoint interval (5 minutes at 1 Hz).
    pub const CHECKPOINT_INTERVAL_TICKS_PROD: u64 = 300;

    /// Returns true when a checkpoint should be taken at `tick_count`.
    ///
    /// `tick_count` is the monotonic tick counter maintained by the server
    /// background task (incremented every `1 / tick_hz` seconds). A checkpoint
    /// is due every [`Self::CHECKPOINT_INTERVAL_TICKS`] ticks, excluding tick 0
    /// so startup does not immediately checkpoint.
    ///
    /// Kept for tests and backwards compatibility. Production code should use
    /// time-based checkpointing (see `should_checkpoint_with_interval` and the
    /// server ticker loop).
    #[must_use]
    pub fn should_checkpoint(tick_count: u64) -> bool {
        tick_count != 0 && tick_count.is_multiple_of(Self::CHECKPOINT_INTERVAL_TICKS)
    }

    /// Returns true when a production-interval checkpoint should be taken.
    ///
    /// Convenience for callers that want the 5-minute (300-tick) cadence at
    /// 1 Hz instead of the demo 100-tick cadence.
    #[must_use]
    pub fn should_checkpoint_prod(tick_count: u64) -> bool {
        tick_count != 0 && tick_count.is_multiple_of(Self::CHECKPOINT_INTERVAL_TICKS_PROD)
    }

    /// Returns true when a checkpoint is due given `tick_count`, `tick_hz` and `interval_secs`.
    ///
    /// The helper converts the time-based interval (`interval_secs`) to a tick
    /// count using `tick_hz * interval_secs` and checks `tick_count % interval == 0`.
    /// Returns `false` when either `tick_hz` or `interval_secs` is 0, or when
    /// `tick_count` is 0 (startup does not immediately checkpoint).
    #[must_use]
    pub fn should_checkpoint_with_interval(
        tick_count: u64,
        tick_hz: u32,
        interval_secs: u64,
    ) -> bool {
        if tick_count == 0 || tick_hz == 0 || interval_secs == 0 {
            return false;
        }
        let interval_ticks = u64::from(tick_hz).saturating_mul(interval_secs);
        if interval_ticks == 0 {
            return false;
        }
        tick_count.is_multiple_of(interval_ticks)
    }

    /// Builds a checkpoint snapshot of the current actor state at `now`.
    ///
    /// The checkpoint contains only persistent entities (M4 treats all entities
    /// as persistent via [`Checkpoint::from_state`]). High-frequency ephemeral
    /// state is already excluded by the checkpoint filter.
    #[must_use]
    pub fn build_checkpoint(&self, now: Timestamp) -> Checkpoint {
        let mut checkpoint = Checkpoint::from_state(&self.state, now);
        if let Some(admission) = &self.admission {
            checkpoint.dedup = admission
                .receipts
                .values()
                .map(|r| r.entry.clone())
                .collect();
        }
        checkpoint
    }

    /// Generates the compact runtime summary retained by the runtime API.
    ///
    /// The wire-level realtime `Snapshot` is assembled by the server
    /// coordinator, where identity, permissions, interest filtering, and
    /// chunking are available. Keeping this method free of transport concerns
    /// preserves the runtime dependency boundary.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "instance_id": self.state.instance_id().to_string(),
            "revision": self.state.revision().as_u64(),
            "members": self.state.member_count(),
        }))
        .unwrap_or_else(|_| br"{}".to_vec())
    }

    /// Returns a cloned snapshot of all entities (for composition-root filtering).
    #[must_use]
    pub fn entities_snapshot(&self) -> Vec<orbisync_domain::Entity> {
        self.state.entities_snapshot()
    }

    /// Reads one authoritative entity without cloning the whole instance.
    #[must_use]
    pub fn entity_snapshot(&self, id: EntityId) -> Option<orbisync_domain::Entity> {
        self.state.get_entity(id).cloned()
    }

    /// Returns shared lightweight interest views. Generation mode materializes its index on read.
    ///
    /// The `Arc` is swapped on each entity mutation; receivers clone the `Arc`
    /// rather than deep-cloning `Entity` with its `components` map.
    #[must_use]
    pub fn interest_views(&self) -> Arc<InterestSnapshot> {
        if let Some(index) = self.generation_interest_index() {
            return index.snapshot();
        }
        Arc::clone(&self.interest_views)
    }

    /// Alias for the interest read path (see [`Self::interest_views`]).
    #[must_use]
    pub fn interest_snapshot_arc(&self) -> Arc<InterestSnapshot> {
        self.interest_views()
    }

    /// Returns the current revision.
    #[must_use]
    pub fn revision(&self) -> Revision {
        self.state.revision()
    }

    /// Returns member count.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.state.member_count()
    }

    /// Returns presence/user pairs for coordinator-side interest filtering.
    #[must_use]
    pub fn members_snapshot(&self) -> Vec<(PresenceId, UserId)> {
        self.state.members_snapshot()
    }

    pub(crate) fn present_presences(&self) -> Arc<std::collections::HashSet<PresenceId>> {
        self.state.present_presences()
    }

    /// Returns the oldest retained revision for the resume history window (W-18).
    ///
    /// When no revision has been retained, returns `None` so
    /// `decide_resync` yields `HistoryUnavailable`.  Capacity is bounded by
    /// `realtime.outbound_queue_capacity` per D-25.
    #[must_use]
    pub fn oldest_retained_revision(&self) -> Option<Revision> {
        self.history.front().copied()
    }

    /// Returns the history length (for tests).
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Oldest cursor from which all reliable deliveries are still available.
    #[must_use]
    pub fn reliable_floor(&self) -> Revision {
        self.reliable_floor
    }

    /// Cheap shared copies of the bounded reliable delivery history.
    #[must_use]
    pub fn reliable_events(&self) -> Arc<VecDeque<(Revision, Arc<[u8]>)>> {
        Arc::clone(&self.reliable_events)
    }

    /// Looks up a retained event in O(1), without cloning the world snapshot.
    /// Only a current member can read acknowledgements.
    pub fn reliable_event(
        &self,
        presence: PresenceId,
        user: UserId,
        id: &str,
    ) -> Result<Option<Arc<[u8]>>, &'static str> {
        if self.state.user_for_presence(presence) != Some(user) {
            return Err("NOT_JOINED");
        }
        Ok(self.reliable_by_id.get(id).cloned())
    }

    /// Returns history capacity (for tests).
    #[must_use]
    pub fn history_capacity(&self) -> usize {
        self.history_capacity
    }

    /// Exposes state for tests (crate-private).
    #[cfg(test)]
    pub(crate) fn state(&self) -> &InstanceState {
        &self.state
    }

    /// Exposes mutable state for tests that need to seed server-owned entities.
    #[cfg(test)]
    pub(crate) fn state_mut(&mut self) -> &mut InstanceState {
        &mut self.state
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{InstanceActor, MailboxConfig, MailboxLane};
    use crate::{
        InstanceRuntimeDescriptor, RuntimeState,
        command::{InstanceCommand, WorldPermissions},
        extension::ExtensionEvent,
        persistence::EntityPersistenceEvent,
    };
    use orbisync_application::metrics::{
        Counter, Gauge, MailboxQueue, MetricsRecorder, NoopMetrics, RateLimitScope,
    };
    use orbisync_domain::{
        EntityId, InstanceId, PresenceId, Revision, Timestamp, Transform, UserId, VisibilityPolicy,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingMetrics {
        counters: Mutex<Vec<Counter>>,
        gauges: Mutex<Vec<(Gauge, i64)>>,
    }

    impl MetricsRecorder for RecordingMetrics {
        fn incr(&self, counter: Counter) {
            self.counters.lock().unwrap().push(counter);
        }

        fn add(&self, counter: Counter, n: u64) {
            for _ in 0..n {
                self.incr(counter);
            }
        }

        fn set(&self, gauge: Gauge, value: i64) {
            self.gauges.lock().unwrap().push((gauge, value));
        }

        fn observe(&self, _histogram: orbisync_application::metrics::Histogram, _value: f64) {}
    }

    fn mailbox_actor(config: MailboxConfig, metrics: Arc<dyn MetricsRecorder>) -> InstanceActor {
        InstanceActor::with_mailbox_config(
            InstanceRuntimeDescriptor {
                instance_id: InstanceId::generate(),
                state: RuntimeState::Running,
                revision: Revision::INITIAL,
            },
            30.0,
            256,
            50.0,
            10.0,
            config,
            metrics,
        )
    }

    fn transform(entity_id: EntityId) -> InstanceCommand {
        InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::identity(),
            expected_revision: Revision::from_u64(1),
            user_id: UserId::generate(),
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            permissions: WorldPermissions::all(),
        }
    }

    fn spawn(entity_id: EntityId) -> InstanceCommand {
        InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: None,
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: UserId::generate(),
            permissions: WorldPermissions::all(),
        }
    }

    #[test]
    fn precommit_guard_rejects_recreated_entity_with_same_revision() {
        use crate::command::CommandOutcome;
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: InstanceId::generate(),
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let id = EntityId::generate();
        let owner = UserId::generate();
        let mut create = spawn(id);
        if let InstanceCommand::SpawnEntity { owner: field, .. } = &mut create {
            *field = Some(owner);
        }
        assert!(matches!(
            actor.submit(create),
            Ok(CommandOutcome::Applied { .. })
        ));
        let before = actor.entity_snapshot(id).unwrap();
        let delete = InstanceCommand::DeleteEntity {
            command_id: None,
            entity_id: id,
            expected_revision: before.revision(),
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        assert!(matches!(
            actor.submit(delete.clone()),
            Ok(CommandOutcome::Applied { .. })
        ));
        let mut replacement = spawn(id);
        if let InstanceCommand::SpawnEntity { owner: field, .. } = &mut replacement {
            *field = Some(UserId::generate());
        }
        assert!(matches!(
            actor.submit(replacement),
            Ok(CommandOutcome::Applied { .. })
        ));
        assert_eq!(
            actor.entity_snapshot(id).unwrap().revision(),
            before.revision()
        );
        let guarded = InstanceCommand::WithExpectedEntityState {
            entity_id: id,
            expected: Some(Box::new(before)),
            command: Box::new(delete),
        };
        assert!(matches!(
            actor.submit(guarded),
            Ok(CommandOutcome::Rejected {
                code: "REVISION_MISMATCH",
                ..
            })
        ));
        assert!(actor.entity_snapshot(id).is_some());
    }

    #[test]
    fn transform_mailbox_coalesces_same_entity_but_not_other_entities() {
        let metrics = Arc::new(RecordingMetrics::default());
        let mut actor = mailbox_actor(
            MailboxConfig {
                control_capacity: 4,
                transform_capacity: 2,
                entity_capacity: 2,
            },
            Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
        );
        let first = EntityId::generate();
        let second = EntityId::generate();
        assert_eq!(
            actor.try_send(transform(first)),
            Ok(super::MailboxEnqueue::Enqueued)
        );
        assert_eq!(
            actor.try_send(transform(second)),
            Ok(super::MailboxEnqueue::Enqueued)
        );
        assert_eq!(
            actor.try_send(transform(first)),
            Ok(super::MailboxEnqueue::Coalesced)
        );
        assert_eq!(actor.mailbox_depth(), (0, 2, 0));
        assert!(metrics.counters.lock().unwrap().iter().any(|counter| {
            matches!(
                counter,
                Counter::InstanceMailboxDropped {
                    queue: MailboxQueue::Transform
                }
            )
        }));
    }

    #[test]
    fn mailbox_saturation_is_bounded_and_recorded() {
        let metrics = Arc::new(RecordingMetrics::default());
        let mut actor = mailbox_actor(
            MailboxConfig {
                control_capacity: 1,
                transform_capacity: 1,
                entity_capacity: 1,
            },
            Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
        );
        assert_eq!(
            actor.try_send(InstanceCommand::Tick),
            Ok(super::MailboxEnqueue::Enqueued)
        );
        assert_eq!(
            actor.try_send(InstanceCommand::Tick),
            Err(super::MailboxSendError::Full {
                lane: MailboxLane::Control
            })
        );
        assert!(metrics.counters.lock().unwrap().iter().any(|counter| {
            matches!(
                counter,
                Counter::InstanceMailboxSaturated {
                    queue: MailboxQueue::Control
                }
            )
        }));
        assert_eq!(actor.mailbox_depth().0, 1);

        assert!(actor.try_send(spawn(EntityId::generate())).is_ok());
        assert_eq!(
            actor.try_send(spawn(EntityId::generate())),
            Err(super::MailboxSendError::Full {
                lane: MailboxLane::Entity
            })
        );
        assert!(metrics.counters.lock().unwrap().iter().any(|counter| {
            matches!(
                counter,
                Counter::InstanceMailboxSaturated {
                    queue: MailboxQueue::Entity
                }
            )
        }));
    }

    #[test]
    fn shutdown_preempts_older_control_work() {
        let mut actor = mailbox_actor(MailboxConfig::default(), Arc::new(NoopMetrics));
        let id = actor.descriptor().instance_id;
        assert!(
            actor
                .try_send(InstanceCommand::Join {
                    presence_id: PresenceId::generate(),
                    user_id: UserId::generate(),
                    instance_id: id,
                    capacity: 10,
                })
                .is_ok()
        );
        assert!(actor.try_send(InstanceCommand::Shutdown).is_ok());
        assert!(matches!(
            actor.process_mailbox(),
            Some(crate::command::CommandOutcome::Applied { .. })
        ));
        assert_eq!(actor.descriptor().state, RuntimeState::Draining);
        assert_eq!(actor.member_count(), 0);
    }

    #[test]
    fn production_checkpoint_cadence_is_300_ticks() {
        assert!(!InstanceActor::should_checkpoint(299));
        assert!(InstanceActor::should_checkpoint(300));
        assert!(!InstanceActor::should_checkpoint(301));
    }

    #[test]
    fn join_and_transform_flow() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let presence = PresenceId::generate();
        let user = UserId::generate();
        let join = InstanceCommand::Join {
            presence_id: presence,
            user_id: user,
            instance_id: instance,
            capacity: 100,
        };
        let outcome = actor.handle(join);
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        // Verify MemberJoined event was emitted.
        let events = actor.outbox().events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ExtensionEvent::MemberJoined {
                instance_id: instance,
                presence_id: presence,
                user_id: user
            }
        );
        let entity = EntityId::generate();
        let cmd = InstanceCommand::UpdateTransform {
            entity_id: entity,
            transform: Transform::identity(),
            expected_revision: Revision::from_u64(1),
            user_id: user,
            now: Timestamp::from_unix_millis(1_000).expect("valid"),
            permissions: WorldPermissions::all(),
        };
        // First transform auto-creates, so any expected revision is accepted via creation path.
        let outcome = actor.handle(cmd);
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        // Auto-create should emit EntitySpawned.
        let events = actor.outbox().events();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            ExtensionEvent::EntitySpawned {
                instance_id: instance,
                entity_id: entity,
                owner: Some(user)
            }
        );
    }

    #[test]
    fn full_persistence_outbox_rejects_before_mutation_and_recovers_after_drain() {
        let instance_id = InstanceId::generate();
        let owner = UserId::generate();
        let mut actor = InstanceActor::with_history_capacity(
            InstanceRuntimeDescriptor {
                instance_id,
                state: RuntimeState::Running,
                revision: Revision::INITIAL,
            },
            30.0,
            2,
        );
        let spawn = |entity_id| InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: Some(Transform::identity()),
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        let ids = [
            EntityId::generate(),
            EntityId::generate(),
            EntityId::generate(),
        ];
        for id in &ids[..2] {
            assert!(matches!(
                actor.handle(spawn(*id)),
                crate::command::CommandOutcome::Applied { .. }
            ));
        }
        let revision = actor.revision();
        let snapshot = actor.interest_views();
        assert!(matches!(
            actor.handle(spawn(ids[2])),
            crate::command::CommandOutcome::Rejected {
                code: "PERSISTENCE_BACKPRESSURE",
                ..
            }
        ));
        assert_eq!(actor.revision(), revision);
        assert!(Arc::ptr_eq(&snapshot, &actor.interest_views()));
        assert!(actor.state().get_entity(ids[2]).is_none());
        assert!(matches!(
            actor.handle(InstanceCommand::Tick),
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert!(
            matches!(
                actor.handle(InstanceCommand::UpdateTransform {
                    entity_id: ids[0],
                    transform: Transform::identity(),
                    expected_revision: Revision::from_u64(1),
                    user_id: owner,
                    now: Timestamp::from_unix_millis(1_000).unwrap(),
                    permissions: WorldPermissions::all(),
                }),
                crate::command::CommandOutcome::Applied { .. }
            ),
            "transient movement must continue"
        );
        let accepted = actor.drain_persistence_batch();
        assert_eq!(accepted.len(), 2, "accepted writes must not be evicted");
        for (effect, id) in accepted.iter().zip(ids) {
            assert!(matches!(effect, EntityPersistenceEvent::Spawned(entity) if entity.id() == id));
        }
        assert!(matches!(
            actor.handle(spawn(ids[2])),
            crate::command::CommandOutcome::Applied { .. }
        ));
    }

    #[test]
    fn component_changes_reuse_interest_index_and_owner_changes_publish_a_new_snapshot() {
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: InstanceId::generate(),
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let entity_id = EntityId::generate();
        let now = Timestamp::from_unix_millis(1_000).unwrap();
        assert!(matches!(
            actor.handle(InstanceCommand::UpdateTransform {
                entity_id,
                transform: Transform::identity(),
                expected_revision: Revision::INITIAL,
                user_id: owner,
                now,
                permissions: WorldPermissions::all(),
            }),
            crate::command::CommandOutcome::Applied { .. }
        ));
        let before = actor.interest_views();
        assert!(matches!(
            actor.handle(InstanceCommand::UpdateEntityComponent {
                command_id: None,
                entity_id,
                component_key: "com.test.data".into(),
                payload_bytes: b"{}".to_vec(),
                expected_revision: Revision::from_u64(1),
                now,
                requester: owner,
                permissions: WorldPermissions::all(),
            }),
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert!(Arc::ptr_eq(&before, &actor.interest_views()));
        let next_owner = UserId::generate();
        actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: next_owner,
            instance_id: actor.descriptor().instance_id,
            capacity: 10,
        });
        assert!(matches!(
            actor.handle(InstanceCommand::TransferOwnership {
                command_id: None,
                entity_id,
                new_owner: Some(next_owner),
                expected_revision: Revision::from_u64(2),
                now,
                requester: owner,
                permissions: WorldPermissions::all(),
            }),
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert_eq!(before.get(entity_id).unwrap().owner, Some(owner));
        assert_eq!(
            actor.interest_views().get(entity_id).unwrap().owner,
            Some(next_owner)
        );
        assert!(std::sync::Weak::ptr_eq(
            &before.membership_token(),
            &actor.interest_views().membership_token()
        ));
    }

    #[test]
    fn spawn_creates_entity() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let entity_id = EntityId::generate();
        let cmd = InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: Some(Transform::identity()),
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        let outcome = actor.handle(cmd);
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        let stored = actor.state().get_entity(entity_id).expect("entity stored");
        assert_eq!(stored.owner(), Some(owner));
        assert_eq!(stored.revision(), Revision::from_u64(1));
        assert_eq!(
            actor.outbox().events(),
            &[ExtensionEvent::EntitySpawned {
                instance_id: instance,
                entity_id,
                owner: Some(owner)
            }]
        );
    }

    #[test]
    fn interest_cache_is_reused_for_non_entity_commands_and_refreshed_for_mutations() {
        let instance_id = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let entity_id = EntityId::generate();
        let owner = UserId::generate();
        assert!(matches!(
            actor.handle(InstanceCommand::UpdateTransform {
                entity_id,
                transform: Transform::identity(),
                expected_revision: Revision::INITIAL,
                user_id: owner,
                now: Timestamp::from_unix_millis(1_000).unwrap(),
                permissions: WorldPermissions::all(),
            }),
            crate::command::CommandOutcome::Applied { .. }
        ));
        let before = actor.interest_views();
        assert_eq!(before.len(), 1);
        let presence_id = PresenceId::generate();
        for command in [
            InstanceCommand::Tick,
            InstanceCommand::Join {
                presence_id,
                user_id: owner,
                instance_id,
                capacity: 10,
            },
            InstanceCommand::Leave { presence_id },
        ] {
            let revision = actor.descriptor().revision;
            assert!(matches!(
                actor.handle(command),
                crate::command::CommandOutcome::Applied { .. }
            ));
            assert!(actor.descriptor().revision > revision);
            assert!(Arc::ptr_eq(&before, &actor.interest_views()));
        }
        assert!(matches!(
            actor.handle(InstanceCommand::DeleteEntity {
                command_id: None,
                entity_id,
                expected_revision: Revision::from_u64(1),
                requester: owner,
                permissions: WorldPermissions::all(),
            }),
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert!(actor.interest_views().is_empty());
        assert_eq!(
            before.len(),
            1,
            "a published view remains an immutable snapshot"
        );
        assert!(!Arc::ptr_eq(&before, &actor.interest_views()));
    }

    #[test]
    fn spawn_ignores_client_owner_without_update_any_permission() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let requester = UserId::generate();
        let claimed_owner = UserId::generate();
        let entity_id = EntityId::generate();
        let outcome = actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(claimed_owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester,
            permissions: WorldPermissions {
                entity_spawn: true,
                entity_update_own: true,
                entity_update_any: false,
            },
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert_eq!(
            actor.state().get_entity(entity_id).unwrap().owner(),
            Some(requester)
        );
    }

    #[test]
    fn spawn_denied_without_entity_spawn_permission() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let entity_id = EntityId::generate();
        let outcome = actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: None,
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: UserId::generate(),
            permissions: WorldPermissions::default(),
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Rejected {
                code: "PERMISSION_DENIED",
                ..
            }
        ));
        assert!(!actor.state().contains_entity(entity_id));
    }

    #[test]
    fn update_any_permission_allows_other_owner() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let administrator = UserId::generate();
        let entity_id = EntityId::generate();
        actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        let outcome = actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("admin.key"),
            payload_bytes: vec![1],
            expected_revision: Revision::from_u64(1),
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: administrator,
            permissions: WorldPermissions {
                entity_spawn: false,
                entity_update_own: false,
                entity_update_any: true,
            },
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
    }

    #[test]
    fn component_rate_limit_rejects_without_disconnect_and_records_metric() {
        let metrics = Arc::new(RecordingMetrics::default());
        let mut actor = mailbox_actor(
            MailboxConfig::default(),
            Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
        )
        .with_component_updates_per_sec(1);
        let owner = UserId::generate();
        let entity_id = EntityId::generate();
        actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        drop(actor.drain_outbox());

        let first = actor
            .state()
            .get_entity(entity_id)
            .expect("entity")
            .revision();
        let applied = actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("com.example.first"),
            payload_bytes: vec![1],
            expected_revision: first,
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            applied,
            crate::command::CommandOutcome::Applied { .. }
        ));
        drop(actor.drain_outbox());

        let second = actor
            .state()
            .get_entity(entity_id)
            .expect("entity")
            .revision();
        let limited = actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("com.example.second"),
            payload_bytes: vec![2],
            expected_revision: second,
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            limited,
            crate::command::CommandOutcome::Rejected {
                code: "COMPONENT_RATE_LIMITED",
                ..
            }
        ));
        assert!(
            actor
                .state()
                .get_entity(entity_id)
                .expect("entity")
                .component("com.example.second")
                .is_none()
        );
        assert!(metrics.counters.lock().expect("metrics lock").contains(
            &Counter::RateLimitRejected {
                scope: RateLimitScope::Instance,
            }
        ));

        // The actor remains usable after a rejected update.
        let later = actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("com.example.later"),
            payload_bytes: vec![3],
            expected_revision: second,
            now: Timestamp::from_unix_millis(2_000).expect("valid timestamp"),
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            later,
            crate::command::CommandOutcome::Applied { .. }
        ));
    }

    #[test]
    fn update_any_permission_allows_assigning_claimed_owner() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let administrator = UserId::generate();
        let designated_owner = UserId::generate();
        let entity_id = EntityId::generate();
        let outcome = actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(designated_owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: administrator,
            permissions: WorldPermissions {
                entity_spawn: true,
                entity_update_own: false,
                entity_update_any: true,
            },
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert_eq!(
            actor.state().get_entity(entity_id).unwrap().owner(),
            Some(designated_owner)
        );
    }

    #[test]
    fn update_with_wrong_owner_rejected() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let intruder = UserId::generate();
        let entity_id = EntityId::generate();
        let spawn = InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: Some(Transform::identity()),
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        actor.handle(spawn);
        assert_eq!(actor.drain_outbox().len(), 1);
        let stored_rev = actor
            .state()
            .get_entity(entity_id)
            .expect("exists")
            .revision();
        let update = InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("test.key"),
            payload_bytes: vec![1, 2, 3],
            expected_revision: stored_rev,
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: intruder,
            permissions: WorldPermissions {
                entity_spawn: false,
                entity_update_own: true,
                entity_update_any: false,
            },
        };
        let outcome = actor.handle(update);
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Rejected {
                code: "NOT_OWNER",
                ..
            }
        ));
        // Ensure no event was emitted for rejected command.
        assert!(actor.outbox().is_empty());
        // Ensure component not applied
        assert!(
            actor
                .state()
                .get_entity(entity_id)
                .expect("exists")
                .component("test.key")
                .is_none()
        );
    }

    #[test]
    fn delete_with_wrong_revision_rejected() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let entity_id = EntityId::generate();
        let spawn = InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        actor.handle(spawn);
        drop(actor.drain_outbox());
        let wrong_rev = Revision::from_u64(99);
        let del = InstanceCommand::DeleteEntity {
            command_id: None,
            entity_id,
            expected_revision: wrong_rev,
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        let outcome = actor.handle(del);
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Rejected {
                code: "REVISION_MISMATCH",
                ..
            }
        ));
        assert!(actor.state().get_entity(entity_id).is_some());
        assert!(actor.outbox().is_empty());
    }

    #[test]
    fn transfer_ownership_succeeds() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let new_owner = UserId::generate();
        let entity_id = EntityId::generate();
        actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: owner,
            instance_id: instance,
            capacity: 100,
        });
        actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: new_owner,
            instance_id: instance,
            capacity: 100,
        });
        let spawn = InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::OwnerOnly,
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        actor.handle(spawn);
        drop(actor.drain_outbox());
        drop(actor.drain_persistence_batch());
        let rev = actor
            .state()
            .get_entity(entity_id)
            .expect("exists")
            .revision();
        let transfer = InstanceCommand::TransferOwnership {
            command_id: None,
            entity_id,
            new_owner: Some(new_owner),
            expected_revision: rev,
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: owner,
            permissions: WorldPermissions::all(),
        };
        let outcome = actor.handle(transfer);
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        {
            let stored = actor.state().get_entity(entity_id).expect("exists");
            assert_eq!(stored.owner(), Some(new_owner));
            assert_eq!(stored.revision(), Revision::from_u64(2));
        }
        assert_eq!(
            actor.outbox().events(),
            &[ExtensionEvent::OwnershipTransferred {
                instance_id: instance,
                entity_id,
                previous_owner: Some(owner),
                new_owner: Some(new_owner)
            }]
        );
        let persistence = actor.drain_persistence_batch();
        assert!(matches!(
            persistence.as_slice(),
            [EntityPersistenceEvent::OwnershipTransferred { entity, audit }]
                if entity.owner() == Some(new_owner)
                    && audit.actor_id == owner
                    && audit.previous_owner == Some(owner)
                    && audit.new_owner == Some(new_owner)
        ));
        // New owner can now update
        drop(actor.drain_outbox());
        let rev_after_transfer = {
            let stored = actor.state().get_entity(entity_id).expect("exists");
            stored.revision()
        };
        let update = InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("com.example.state"),
            payload_bytes: vec![9],
            expected_revision: rev_after_transfer,
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: new_owner,
            permissions: WorldPermissions::all(),
        };
        let outcome2 = actor.handle(update);
        assert!(matches!(
            outcome2,
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert_eq!(
            actor.outbox().events(),
            &[ExtensionEvent::EntityUpdated {
                instance_id: instance,
                entity_id
            }]
        );
    }

    #[test]
    fn transfer_ownership_rejects_absent_or_unchanged_target_without_side_effects() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let absent = UserId::generate();
        let entity_id = EntityId::generate();
        actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: owner,
            instance_id: instance,
            capacity: 100,
        });
        actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::OwnerOnly,
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        drop(actor.drain_outbox());
        drop(actor.drain_persistence_batch());
        let revision = actor
            .state()
            .get_entity(entity_id)
            .expect("exists")
            .revision();

        let absent_outcome = actor.handle(InstanceCommand::TransferOwnership {
            command_id: None,
            entity_id,
            new_owner: Some(absent),
            expected_revision: revision,
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            absent_outcome,
            crate::command::CommandOutcome::Rejected {
                code: "TARGET_NOT_PRESENT",
                ..
            }
        ));

        let unchanged_outcome = actor.handle(InstanceCommand::TransferOwnership {
            command_id: None,
            entity_id,
            new_owner: Some(owner),
            expected_revision: revision,
            now: Timestamp::from_unix_millis(2_000).expect("valid timestamp"),
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            unchanged_outcome,
            crate::command::CommandOutcome::Rejected {
                code: "OWNER_UNCHANGED",
                ..
            }
        ));
        let stored = actor.state().get_entity(entity_id).expect("exists");
        assert_eq!(stored.owner(), Some(owner));
        assert_eq!(stored.revision(), revision);
        assert!(actor.outbox().is_empty());
        assert!(actor.drain_persistence_batch().is_empty());
    }

    #[test]
    fn delete_removes_entity() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let owner = UserId::generate();
        let entity_id = EntityId::generate();
        actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: None,
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        drop(actor.drain_outbox());
        let rev = actor
            .state()
            .get_entity(entity_id)
            .expect("exists")
            .revision();
        let outcome = actor.handle(InstanceCommand::DeleteEntity {
            command_id: None,
            entity_id,
            expected_revision: rev,
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert!(actor.state().get_entity(entity_id).is_none());
        assert_eq!(
            actor.outbox().events(),
            &[ExtensionEvent::EntityDeleted {
                instance_id: instance,
                entity_id
            }]
        );
    }

    #[test]
    fn server_owned_owner_none_rejects_client_update() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let other = UserId::generate();
        let entity_id = EntityId::generate();
        let entity = orbisync_domain::Entity::new(
            entity_id,
            instance,
            orbisync_domain::EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            Timestamp::from_unix_millis(0).expect("valid"),
        );
        actor.state_mut().upsert_entity(entity);
        let outcome = actor.handle(InstanceCommand::UpdateEntityComponent {
            command_id: None,
            entity_id,
            component_key: String::from("open.key"),
            payload_bytes: vec![0],
            expected_revision: Revision::from_u64(1),
            now: Timestamp::from_unix_millis(1_000).expect("valid timestamp"),
            requester: other,
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Rejected {
                code: "NOT_OWNER",
                ..
            }
        ));
    }

    #[test]
    fn member_left_emits_event() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let presence = PresenceId::generate();
        let user = UserId::generate();
        actor.handle(InstanceCommand::Join {
            presence_id: presence,
            user_id: user,
            instance_id: instance,
            capacity: 100,
        });
        drop(actor.drain_outbox());
        let outcome = actor.handle(InstanceCommand::Leave {
            presence_id: presence,
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert_eq!(
            actor.outbox().events(),
            &[ExtensionEvent::MemberLeft {
                instance_id: instance,
                presence_id: presence,
                user_id: user
            }]
        );
    }

    #[test]
    fn rejected_join_does_not_emit_event() {
        let instance = InstanceId::generate();
        let wrong_instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let outcome = actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: UserId::generate(),
            instance_id: wrong_instance,
            capacity: 100,
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Rejected { .. }
        ));
        assert!(actor.outbox().is_empty());
    }

    #[test]
    fn update_transform_emits_entity_updated_when_entity_exists() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        let entity_id = EntityId::generate();
        let owner = UserId::generate();
        actor.handle(InstanceCommand::SpawnEntity {
            command_id: None,
            entity_id,
            kind: orbisync_domain::EntityKind::Object,
            owner: Some(owner),
            transform: Some(Transform::identity()),
            visibility: VisibilityPolicy::Global,
            requester: owner,
            permissions: WorldPermissions::all(),
        });
        drop(actor.drain_outbox());
        let rev = actor
            .state()
            .get_entity(entity_id)
            .expect("exists")
            .revision();
        // Use small movement within limits.
        let new_pos = orbisync_domain::Vec3::new(0.5, 0.0, 0.0).expect("valid");
        let new_rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("valid");
        let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("valid");
        let new_transform = Transform::new(new_pos, new_rot, scale).expect("valid");
        let outcome = actor.handle(InstanceCommand::UpdateTransform {
            entity_id,
            transform: new_transform,
            expected_revision: rev,
            user_id: owner,
            now: Timestamp::from_unix_millis(2_000).expect("valid"),
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        assert_eq!(
            actor.outbox().events(),
            &[ExtensionEvent::EntityUpdated {
                instance_id: instance,
                entity_id
            }]
        );
    }

    /// D-2: the composition root's `interest.near_radius` must reach the
    /// auto-created avatar. Hard-coding the radius back into `actor.rs` (the
    /// state this replaced) turns this red.
    #[test]
    fn auto_created_avatar_uses_injected_visibility_radius() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::with_avatar_visibility_radius(
            InstanceRuntimeDescriptor {
                instance_id: instance,
                state: RuntimeState::Running,
                revision: Revision::INITIAL,
            },
            50.0,
        );
        let entity_id = EntityId::generate();
        let outcome = actor.handle(InstanceCommand::UpdateTransform {
            entity_id,
            transform: Transform::identity(),
            expected_revision: Revision::from_u64(1),
            user_id: UserId::generate(),
            now: Timestamp::from_unix_millis(1_000).expect("valid"),
            permissions: WorldPermissions::all(),
        });
        assert!(matches!(
            outcome,
            crate::command::CommandOutcome::Applied { .. }
        ));
        let stored = actor.state().get_entity(entity_id).expect("auto-created");
        assert_eq!(
            stored.visibility(),
            &VisibilityPolicy::spatial(50.0).expect("valid")
        );
    }

    /// An invalid radius must not silently become `Global`, which would
    /// short-circuit every spatial check (H-6a).
    #[test]
    fn invalid_avatar_radius_falls_back_to_default_spatial() {
        let instance = InstanceId::generate();
        for bad in [0.0_f32, -1.0, f32::NAN] {
            let mut actor = InstanceActor::with_avatar_visibility_radius(
                InstanceRuntimeDescriptor {
                    instance_id: instance,
                    state: RuntimeState::Running,
                    revision: Revision::INITIAL,
                },
                bad,
            );
            let entity_id = EntityId::generate();
            actor.handle(InstanceCommand::UpdateTransform {
                entity_id,
                transform: Transform::identity(),
                expected_revision: Revision::from_u64(1),
                user_id: UserId::generate(),
                now: Timestamp::from_unix_millis(1_000).expect("valid"),
                permissions: WorldPermissions::all(),
            });
            let stored = actor.state().get_entity(entity_id).expect("auto-created");
            assert_eq!(
                stored.visibility(),
                &VisibilityPolicy::spatial(InstanceActor::DEFAULT_AVATAR_VISIBILITY_RADIUS)
                    .expect("valid"),
                "radius {bad} must fall back to the default, never to Global"
            );
        }
    }

    #[test]
    fn outbox_drain_clears_events() {
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: instance,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        actor.handle(InstanceCommand::Join {
            presence_id: PresenceId::generate(),
            user_id: UserId::generate(),
            instance_id: instance,
            capacity: 100,
        });
        assert_eq!(actor.outbox().len(), 1);
        let drained = actor.drain_outbox();
        assert_eq!(drained.len(), 1);
        assert!(actor.outbox().is_empty());
        // Second drain returns empty.
        assert!(actor.drain_outbox().is_empty());
    }

    #[test]
    fn outbox_capacity_is_derived_from_history_capacity_and_records_drops() {
        let metrics = Arc::new(RecordingMetrics::default());
        let instance = InstanceId::generate();
        let mut actor = InstanceActor::with_mailbox_config(
            InstanceRuntimeDescriptor {
                instance_id: instance,
                state: RuntimeState::Running,
                revision: Revision::INITIAL,
            },
            30.0,
            2,
            50.0,
            10.0,
            MailboxConfig::default(),
            Arc::clone(&metrics) as Arc<dyn MetricsRecorder>,
        );

        for _ in 0..3 {
            actor.handle(InstanceCommand::Join {
                presence_id: PresenceId::generate(),
                user_id: UserId::generate(),
                instance_id: instance,
                capacity: 100,
            });
        }

        assert_eq!(actor.outbox().capacity(), 2);
        assert_eq!(actor.outbox().len(), 2);
        assert_eq!(
            metrics
                .counters
                .lock()
                .unwrap()
                .iter()
                .filter(|counter| matches!(counter, Counter::ExtensionOutboxDroppedTotal))
                .count(),
            1
        );
    }
}

#[cfg(test)]
mod reliable_history_tests {
    use super::*;
    use orbisync_domain::InstanceId;

    #[test]
    fn retained_snapshot_is_shared_until_mutation_and_survives_eviction() {
        let mut actor = InstanceActor::with_history_capacity(
            InstanceRuntimeDescriptor {
                instance_id: InstanceId::generate(),
                state: RuntimeState::Running,
                revision: Revision::INITIAL,
            },
            1.0,
            2,
        );
        for revision in 1..=2 {
            let result = actor.handle(InstanceCommand::RetainReliable {
                message_id: revision.to_string(),
                revision: Revision::from_u64(revision),
                payload: Arc::from([revision as u8].as_slice()),
            });
            assert!(matches!(result, CommandOutcome::Applied { .. }));
        }
        let captured = actor.reliable_events();
        assert!(Arc::ptr_eq(&captured, &actor.reliable_events()));
        let result = actor.handle(InstanceCommand::RetainReliable {
            message_id: "3".to_owned(),
            revision: Revision::from_u64(3),
            payload: Arc::from([3_u8].as_slice()),
        });
        assert!(matches!(result, CommandOutcome::Applied { .. }));
        assert_eq!(captured.front().map(|entry| entry.0.as_u64()), Some(1));
        assert_eq!(
            actor
                .reliable_events()
                .front()
                .map(|entry| entry.0.as_u64()),
            Some(2)
        );
        assert_eq!(actor.reliable_floor().as_u64(), 1);
        assert!(!actor.reliable_by_id.contains_key("1"));
        assert_eq!(actor.reliable_by_id.len(), 2);
        assert_eq!(
            actor.revision(),
            Revision::INITIAL,
            "retention must not mutate world revision"
        );
    }
}
