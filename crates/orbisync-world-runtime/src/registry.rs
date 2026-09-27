//! Runtime registry for instance actors.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

struct CancelPreparedOnDrop {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    armed: bool,
}
impl Drop for CancelPreparedOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

use orbisync_domain::{Entity, EntityId, InstanceId, PresenceId, Revision, Timestamp, UserId};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, mpsc, oneshot};

use crate::actor::AdmissionResult;
use crate::actor::InstanceActor;
use crate::command::{CommandOutcome, InstanceCommand};
use crate::{
    Checkpoint, EntityPersistenceEvent, ExtensionEvent, MailboxLane, MailboxSendError, RuntimeState,
};
use orbisync_application::ApplicationError;
use orbisync_application::checkpoint_admission::AdmissionRequest;
use orbisync_application::checkpoint_admission::{GenerationAttempt, GenerationResolution};

/// Read-only state returned by an instance runtime task.
#[derive(Debug, Clone)]
pub struct InstanceReadSnapshot {
    /// Lightweight views used by interest filtering.
    pub interest_views: Arc<crate::InterestSnapshot>,
    /// Cloned entities used to build a protocol snapshot.
    pub entities: Vec<Entity>,
    /// Current presence membership.
    pub members: Vec<(PresenceId, UserId)>,
    /// Current canonical revision.
    pub revision: Revision,
    /// Oldest revision retained for resume decisions.
    pub oldest_retained_revision: Option<Revision>,
    /// Earliest client cursor supported by the reliable payload history.
    pub reliable_floor: Revision,
    /// Reliable transport payloads with their canonical instance revisions.
    pub reliable_events: Arc<std::collections::VecDeque<(Revision, Arc<[u8]>)>>,
}

/// Work performed by one runtime task during a server tick.
#[derive(Debug)]
pub struct InstanceTickResult {
    /// Facts emitted by the actor during this tick.
    pub events: Vec<ExtensionEvent>,
    /// Persistence effects emitted by the actor during this tick (HIGH-002).
    pub persistence_events: Vec<EntityPersistenceEvent>,
    /// Checkpoint captured when the server requested one.
    pub checkpoint: Option<Checkpoint>,
    /// Actor lifecycle after the tick.
    pub state: RuntimeState,
}

/// Ordered effects transferred from an actor to its persistence worker.
#[derive(Debug, Default)]
pub struct InstanceEffects {
    /// Extension facts in their original order.
    pub events: Vec<ExtensionEvent>,
    /// Entity writes in their original order.
    pub persistence_events: Vec<EntityPersistenceEvent>,
}

/// Work returned when an idle instance is reaped.
#[derive(Debug)]
pub struct InstanceReapResult {
    /// Final checkpoint captured before removal.
    pub checkpoint: Checkpoint,
    /// Facts emitted before the task stopped.
    pub events: Vec<ExtensionEvent>,
    /// Persistence effects emitted before the task stopped (HIGH-002).
    pub persistence_events: Vec<EntityPersistenceEvent>,
    /// Stopped actor retained until its final checkpoint has been persisted.
    ///
    /// Keeping the actor in memory lets the coordinator abort the reap without
    /// reconstructing state when durable persistence is temporarily unavailable.
    stopped_actor: InstanceActor,
}

enum RuntimeTaskRequest {
    LookupGeneration {
        command_id: orbisync_domain::CommandId,
        fingerprint: [u8; 32],
        now: i64,
        reply: oneshot::Sender<Option<AdmissionResult>>,
    },
    RejectPrepared {
        request: AdmissionRequest,
        code: &'static str,
        detail: String,
        reply: oneshot::Sender<AdmissionResult>,
    },
    FinishGenerationStop {
        reply: oneshot::Sender<Option<Vec<ExtensionEvent>>>,
    },
    StopGenerationIfIdle {
        deadline: Option<tokio::time::Instant>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
        reply: oneshot::Sender<bool>,
    },
    GenerationCapture {
        now: Timestamp,
        deadline: Option<tokio::time::Instant>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
        reply: oneshot::Sender<Result<(GenerationAttempt, Arc<Checkpoint>), ApplicationError>>,
    },
    GenerationApply {
        operation: GenerationOperation,
        deadline: Option<tokio::time::Instant>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
        reply: oneshot::Sender<Result<(), ApplicationError>>,
    },
    Prepared {
        command: InstanceCommand,
        request: AdmissionRequest,
        reply: oneshot::Sender<AdmissionResult>,
    },
    EntityRead {
        id: orbisync_domain::EntityId,
        reply: oneshot::Sender<Option<Entity>>,
    },
    ReliableEvent {
        presence: PresenceId,
        user: UserId,
        id: String,
        reply: oneshot::Sender<Result<Option<Arc<[u8]>>, &'static str>>,
    },
    Submit {
        command: InstanceCommand,
        reply: oneshot::Sender<Result<CommandOutcome, MailboxSendError>>,
    },
    Read {
        reply: oneshot::Sender<Option<InstanceReadSnapshot>>,
    },
    ReadEntity {
        id: EntityId,
        reply: oneshot::Sender<Option<Option<Entity>>>,
    },
    Checkpoint {
        now: Timestamp,
        reply: oneshot::Sender<Option<Checkpoint>>,
    },
    Tick {
        now: Timestamp,
        checkpoint_due: bool,
        drain_effects: bool,
        reply: oneshot::Sender<InstanceTickResult>,
    },
    DrainEffects {
        reply: oneshot::Sender<InstanceEffects>,
    },
    ReapIfIdle {
        now: Timestamp,
        reply: oneshot::Sender<Option<InstanceReapResult>>,
    },
    #[cfg(test)]
    PanicForChaosTest,
}

enum GenerationOperation {
    Resolve(GenerationAttempt, GenerationResolution, i64),
    Retire(GenerationAttempt),
    Expire(i64),
}
fn apply_generation(
    actor: &mut InstanceActor,
    operation: GenerationOperation,
) -> Result<(), ApplicationError> {
    match operation {
        GenerationOperation::Resolve(attempt, resolution, now) => {
            actor.resolve_generation(&attempt, resolution, now)
        }
        GenerationOperation::Retire(attempt) => actor.retire_absent_generation(&attempt),
        GenerationOperation::Expire(now) => {
            actor.expire_generation_receipts(now);
            Ok(())
        }
    }
}

/// Handle for the task that owns one instance actor.
#[derive(Debug, Clone)]
pub struct InstanceHandle {
    generation_interest: Option<crate::actor::GenerationInterestIndex>,
    id: InstanceId,
    requests: mpsc::Sender<RuntimeTaskRequest>,
    interest_views: Arc<RwLock<Arc<crate::InterestSnapshot>>>,
    member_count: Arc<AtomicUsize>,
    /// Cached set of currently-present `PresenceId`s (ADR-025 "権限・所有・
    /// 参加状態のwindow"), refreshed on the same cadence as `interest_views`
    /// (after every `Submit`/`Tick`/`ReapIfIdle`) — a lock read, not an
    /// actor round trip, so a WS connection can cheaply check "has this
    /// presence left the instance" immediately before/after a pending
    /// pre-commit hook wait without adding an extra `read_snapshot` call to
    /// every checked command.
    present_presences: Arc<RwLock<Arc<HashSet<PresenceId>>>>,
}

impl InstanceHandle {
    /// Read-only receipt check before mutable transport validation.
    pub async fn lookup_generation(
        &self,
        command_id: orbisync_domain::CommandId,
        fingerprint: [u8; 32],
        now: i64,
    ) -> Result<Option<AdmissionResult>, ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::LookupGeneration {
                command_id,
                fingerprint,
                now,
                reply,
            })
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))?;
        response
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))
    }
    /// Record an already-authorized deterministic validation rejection.
    pub async fn reject_prepared(
        &self,
        request: AdmissionRequest,
        code: &'static str,
        detail: String,
    ) -> Result<AdmissionResult, ApplicationError> {
        let mut cancel = CancelPreparedOnDrop {
            cancelled: Arc::clone(&request.cancelled),
            armed: true,
        };
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::RejectPrepared {
                request,
                code,
                detail,
                reply,
            })
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))?;
        let result = response
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"));
        cancel.armed = false;
        result
    }

    /// Atomically stop an idle generation actor before final capture. Failure
    /// retains the same task and state for subsequent durability recovery.
    pub async fn stop_generation_if_idle(&self) -> bool {
        self.stop_generation_if_idle_before(None, None).await
    }
    /// Stop an idle actor only while its admitted execution remains live.
    pub async fn stop_generation_if_idle_before(
        &self,
        deadline: Option<tokio::time::Instant>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> bool {
        let (reply, response) = oneshot::channel();
        if self
            .requests
            .send(RuntimeTaskRequest::StopGenerationIfIdle {
                deadline,
                cancelled,
                reply,
            })
            .await
            .is_err()
        {
            return false;
        }
        response.await.unwrap_or(false)
    }
    /// Phase 3 capture connection: acquire global work permits before calling.
    pub async fn capture_generation(
        &self,
        now: Timestamp,
    ) -> Result<(GenerationAttempt, Arc<Checkpoint>), ApplicationError> {
        self.capture_generation_before(now, None, None).await
    }
    /// Capture only if its execution clock has not expired in the queue.
    pub async fn capture_generation_before(
        &self,
        now: Timestamp,
        deadline: Option<tokio::time::Instant>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<(GenerationAttempt, Arc<Checkpoint>), ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::GenerationCapture {
                now,
                deadline,
                cancelled,
                reply,
            })
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))?;
        response
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))?
    }
    async fn generation_operation(
        &self,
        operation: GenerationOperation,
        deadline: Option<tokio::time::Instant>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<(), ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::GenerationApply {
                operation,
                deadline,
                cancelled,
                reply,
            })
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))?;
        response
            .await
            .map_err(|_| ApplicationError::port_failure("actor unavailable"))?
    }
    /// Applies primary-store evidence for the original immutable attempt.
    pub async fn resolve_generation(
        &self,
        attempt: GenerationAttempt,
        resolution: GenerationResolution,
        now: i64,
    ) -> Result<(), ApplicationError> {
        self.generation_operation(
            GenerationOperation::Resolve(attempt, resolution, now),
            None,
            None,
        )
        .await
    }
    /// Retires only an attempt whose absence has been proven.
    pub async fn retire_absent_generation(
        &self,
        attempt: GenerationAttempt,
    ) -> Result<(), ApplicationError> {
        self.generation_operation(GenerationOperation::Retire(attempt), None, None)
            .await
    }
    /// Expires only durable receipts in the serialized owner task.
    pub async fn expire_generation_receipts(&self, now: i64) -> Result<(), ApplicationError> {
        self.generation_operation(GenerationOperation::Expire(now), None, None)
            .await
    }
    /// Expire confirmed receipts only if the request reaches its turn in time.
    pub async fn expire_generation_receipts_before(
        &self,
        now: i64,
        deadline: tokio::time::Instant,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<(), ApplicationError> {
        self.generation_operation(GenerationOperation::Expire(now), Some(deadline), cancelled)
            .await
    }
    /// Dispatches a prepared outcome contract to the actor's serialized turn.
    pub async fn submit_prepared(
        &self,
        command: InstanceCommand,
        request: AdmissionRequest,
    ) -> Result<AdmissionResult, MailboxSendError> {
        let mut cancel_on_drop = CancelPreparedOnDrop {
            cancelled: Arc::clone(&request.cancelled),
            armed: true,
        };
        let lane = command_lane(&command);
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(RuntimeTaskRequest::Prepared {
                command,
                request,
                reply,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => MailboxSendError::Full { lane },
                mpsc::error::TrySendError::Closed(_) => MailboxSendError::Closed { lane },
            })?;
        let result = response
            .await
            .map_err(|_| MailboxSendError::Closed { lane });
        cancel_on_drop.armed = false;
        result
    }
    fn spawn(mut actor: InstanceActor) -> Self {
        let generation_interest = actor.generation_interest_index();
        let id = actor.descriptor().instance_id;
        let interest_views = Arc::new(RwLock::new(actor.interest_views()));
        let member_count = Arc::new(AtomicUsize::new(actor.member_count()));
        let present_presences = Arc::new(RwLock::new(actor.present_presences()));
        let task_interest_views = Arc::clone(&interest_views);
        let task_member_count = Arc::clone(&member_count);
        let task_present_presences = Arc::clone(&present_presences);
        let (requests, mut receiver) = mpsc::channel(256);
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                match request {
                    RuntimeTaskRequest::GenerationCapture {
                        now,
                        deadline,
                        cancelled,
                        reply,
                    } => {
                        if reply.is_closed() {
                            continue;
                        }
                        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
                            || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            drop(reply.send(Err(ApplicationError::port_failure(
                                "capture execution expired",
                            ))));
                        } else {
                            drop(reply.send(actor.capture_generation(now)));
                        }
                    }
                    RuntimeTaskRequest::GenerationApply {
                        operation,
                        deadline,
                        cancelled,
                        reply,
                    } => {
                        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
                            || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            drop(reply.send(Err(ApplicationError::port_failure(
                                "actor execution expired",
                            ))));
                        } else {
                            drop(reply.send(apply_generation(&mut actor, operation)));
                        }
                    }
                    RuntimeTaskRequest::Prepared {
                        command,
                        request,
                        reply,
                    } => {
                        // Before this turn a dropped caller cancels admission;
                        // after commit the actor owns the retained outcome.
                        if reply.is_closed() {
                            continue;
                        }
                        let outcome = actor.submit_prepared(command, request);
                        refresh_runtime_cache(
                            &actor,
                            &task_interest_views,
                            &task_member_count,
                            &task_present_presences,
                        );
                        drop(reply.send(outcome));
                    }
                    RuntimeTaskRequest::LookupGeneration {
                        command_id,
                        fingerprint,
                        now,
                        reply,
                    } => {
                        drop(reply.send(actor.lookup_generation(command_id, fingerprint, now)));
                    }
                    RuntimeTaskRequest::RejectPrepared {
                        request,
                        code,
                        detail,
                        reply,
                    } => {
                        if !reply.is_closed() {
                            drop(reply.send(actor.reject_prepared(request, code, detail)));
                        }
                    }
                    RuntimeTaskRequest::EntityRead { id, reply } => {
                        drop(reply.send(actor.entity_snapshot(id)));
                    }
                    RuntimeTaskRequest::ReliableEvent {
                        presence,
                        user,
                        id,
                        reply,
                    } => {
                        drop(reply.send(actor.reliable_event(presence, user, &id)));
                    }
                    RuntimeTaskRequest::Submit { command, reply } => {
                        let outcome = actor.submit(command);
                        refresh_runtime_cache(
                            &actor,
                            &task_interest_views,
                            &task_member_count,
                            &task_present_presences,
                        );
                        drop(reply.send(outcome));
                    }
                    RuntimeTaskRequest::Read { reply } => {
                        drop(
                            reply.send(
                                actor
                                    .generation_reads_available()
                                    .then(|| read_snapshot(&actor)),
                            ),
                        );
                    }
                    RuntimeTaskRequest::ReadEntity { id, reply } => {
                        drop(
                            reply.send(
                                actor
                                    .generation_reads_available()
                                    .then(|| actor.entity_snapshot(id)),
                            ),
                        );
                    }
                    RuntimeTaskRequest::Checkpoint { now, reply } => {
                        drop(
                            reply.send(
                                (!actor.uses_generation_admission())
                                    .then(|| actor.build_checkpoint(now)),
                            ),
                        );
                    }
                    RuntimeTaskRequest::Tick {
                        now,
                        checkpoint_due,
                        drain_effects,
                        reply,
                    } => {
                        drop(actor.submit(InstanceCommand::Tick));
                        let events = if drain_effects {
                            actor.drain_outbox()
                        } else {
                            Vec::new()
                        };
                        let persistence_events = if drain_effects {
                            actor.drain_persistence_batch()
                        } else {
                            Vec::new()
                        };
                        let checkpoint = (checkpoint_due && !actor.uses_generation_admission())
                            .then(|| actor.build_checkpoint(now));
                        let state = actor.descriptor().state;
                        refresh_runtime_cache(
                            &actor,
                            &task_interest_views,
                            &task_member_count,
                            &task_present_presences,
                        );
                        drop(reply.send(InstanceTickResult {
                            events,
                            persistence_events,
                            checkpoint,
                            state,
                        }));
                    }
                    RuntimeTaskRequest::FinishGenerationStop { reply } => {
                        let durable = actor.generation_stop_is_durable();
                        let _sent = reply.send(durable.then(|| actor.drain_outbox()));
                        if durable {
                            break;
                        }
                    }
                    RuntimeTaskRequest::StopGenerationIfIdle {
                        deadline,
                        cancelled,
                        reply,
                    } => {
                        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
                            || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            let _sent = reply.send(false);
                            continue;
                        }
                        let stopped = actor.uses_generation_admission()
                            && actor.member_count() == 0
                            && (actor.descriptor().state == RuntimeState::Draining
                                || matches!(
                                    actor.submit(InstanceCommand::Shutdown),
                                    Ok(CommandOutcome::Applied { .. })
                                ));
                        let _sent = reply.send(stopped);
                    }
                    RuntimeTaskRequest::DrainEffects { reply } => {
                        // Cancellation of the receiver must not discard accepted writes.
                        // Restore the batch synchronously, before processing another command.
                        if let Err(effects) = reply.send(InstanceEffects {
                            events: actor.drain_outbox(),
                            persistence_events: actor.drain_persistence_batch(),
                        }) {
                            for event in effects.events {
                                actor.outbox_mut().push(event);
                            }
                            for event in effects.persistence_events {
                                actor.persistence_outbox_mut().push(event);
                            }
                        }
                    }
                    RuntimeTaskRequest::ReapIfIdle { now, reply } => {
                        // No legacy final-save fallback for generation actors.
                        if actor.uses_generation_admission() {
                            drop(reply.send(None));
                            continue;
                        }
                        if actor.member_count() != 0 {
                            drop(reply.send(None));
                            continue;
                        }
                        let checkpoint = actor.build_checkpoint(now);
                        drop(actor.submit(InstanceCommand::Shutdown));
                        let events = actor.drain_outbox();
                        let persistence_events = actor.drain_persistence_batch();
                        refresh_runtime_cache(
                            &actor,
                            &task_interest_views,
                            &task_member_count,
                            &task_present_presences,
                        );
                        drop(reply.send(Some(InstanceReapResult {
                            checkpoint,
                            events,
                            persistence_events,
                            stopped_actor: actor,
                        })));
                        break;
                    }
                    #[cfg(test)]
                    RuntimeTaskRequest::PanicForChaosTest => {
                        panic_for_chaos_test();
                    }
                }
            }
        });
        Self {
            generation_interest,
            id,
            requests,
            interest_views,
            member_count,
            present_presences,
        }
    }

    /// Returns the instance ID owned by this handle.
    #[must_use]
    pub const fn instance_id(&self) -> InstanceId {
        self.id
    }

    /// Submits one command to this instance's owner task.
    pub async fn submit(
        &self,
        command: InstanceCommand,
    ) -> Result<CommandOutcome, MailboxSendError> {
        let lane = command_lane(&command);
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(RuntimeTaskRequest::Submit { command, reply })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => MailboxSendError::Full { lane },
                mpsc::error::TrySendError::Closed(_) => MailboxSendError::Closed { lane },
            })?;
        response
            .await
            .unwrap_or(Err(MailboxSendError::Closed { lane }))
    }

    /// Attempts to submit a command without waiting for its outcome.
    pub fn try_submit(&self, command: InstanceCommand) -> Result<(), MailboxSendError> {
        let lane = command_lane(&command);
        let (reply, _response) = oneshot::channel();
        self.requests
            .try_send(RuntimeTaskRequest::Submit { command, reply })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => MailboxSendError::Full { lane },
                mpsc::error::TrySendError::Closed(_) => MailboxSendError::Closed { lane },
            })
    }

    /// Returns a consistent read model without touching the registry lock.
    pub async fn read_snapshot(&self) -> Option<InstanceReadSnapshot> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::Read { reply })
            .await
            .ok()?;
        response.await.ok().flatten()
    }

    /// Reads one entity on its owning task. Outer `None` means unavailable;
    /// `Some(None)` means the entity does not exist.
    pub async fn read_entity(&self, id: EntityId) -> Option<Option<Entity>> {
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(RuntimeTaskRequest::ReadEntity { id, reply })
            .ok()?;
        response.await.ok().flatten()
    }

    /// Reads one entity through the owning task; does not clone the full instance.
    pub async fn read_entity_direct(
        &self,
        id: EntityId,
    ) -> Result<Option<Entity>, &'static str> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::EntityRead { id, reply })
            .await
            .map_err(|_| "instance unavailable")?;
        response.await.map_err(|_| "instance unavailable")
    }

    /// Looks up one retained acknowledgement without building a world snapshot.
    pub async fn reliable_event(
        &self,
        presence: PresenceId,
        user: UserId,
        id: String,
    ) -> Result<Option<Arc<[u8]>>, &'static str> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::ReliableEvent {
                presence,
                user,
                id,
                reply,
            })
            .await
            .map_err(|_| "SERVER_BUSY")?;
        response.await.map_err(|_| "SERVER_BUSY")?
    }

    /// Returns a checkpoint from the task-owned actor.
    pub async fn build_checkpoint(&self, now: Timestamp) -> Option<Checkpoint> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::Checkpoint { now, reply })
            .await
            .ok()?;
        response.await.ok().flatten()
    }

    /// Processes the periodic tick and drains actor-owned extension events.
    pub async fn tick(&self, now: Timestamp, checkpoint_due: bool) -> Option<InstanceTickResult> {
        self.tick_inner(now, checkpoint_due, true).await
    }

    /// Advances simulation without transferring effects to the tick coordinator.
    /// The independent persistence worker drains them using [`Self::drain_effects`].
    pub async fn advance_tick(&self, now: Timestamp) -> Option<InstanceTickResult> {
        self.tick_inner(now, false, false).await
    }

    async fn tick_inner(
        &self,
        now: Timestamp,
        checkpoint_due: bool,
        drain_effects: bool,
    ) -> Option<InstanceTickResult> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::Tick {
                now,
                checkpoint_due,
                drain_effects,
                reply,
            })
            .await
            .ok()?;
        response.await.ok()
    }

    /// Transfers one bounded batch without advancing simulation.
    /// Only the instance's persistence worker should call this in production.
    pub async fn drain_effects(&self) -> Option<InstanceEffects> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::DrainEffects { reply })
            .await
            .ok()?;
        response.await.ok()
    }

    /// Reaps the actor if it is still idle and returns its final work.
    pub async fn reap_if_idle(&self, now: Timestamp) -> Option<InstanceReapResult> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(RuntimeTaskRequest::ReapIfIdle { now, reply })
            .await
            .ok()?;
        response.await.ok().flatten()
    }

    #[cfg(test)]
    async fn panic_for_chaos_test(&self) -> Result<(), MailboxSendError> {
        self.requests
            .send(RuntimeTaskRequest::PanicForChaosTest)
            .await
            .map_err(|_| MailboxSendError::Closed {
                lane: MailboxLane::Control,
            })
    }

    /// Returns the latest cached interest views for synchronous delivery code.
    #[must_use]
    pub fn cached_interest_views(&self) -> Arc<crate::InterestSnapshot> {
        if let Some(index) = &self.generation_interest {
            return index.snapshot();
        }
        self.interest_views
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns the last member count published by the owner task.
    #[must_use]
    pub fn cached_member_count(&self) -> usize {
        self.member_count.load(Ordering::Acquire)
    }

    /// Returns whether `presence` was present as of the last refresh (ADR-025
    /// "権限・所有・参加状態のwindow"). A lock read, not an actor round trip.
    #[must_use]
    pub fn is_present(&self, presence: PresenceId) -> bool {
        if self
            .generation_interest
            .as_ref()
            .is_some_and(|index| !index.available())
        {
            return false;
        }
        self.present_presences
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&presence)
    }
}

fn refresh_runtime_cache(
    actor: &InstanceActor,
    interest_views: &RwLock<Arc<crate::InterestSnapshot>>,
    member_count: &AtomicUsize,
    present_presences: &RwLock<Arc<HashSet<PresenceId>>>,
) {
    if !actor.uses_generation_admission() {
        *interest_views.write().unwrap_or_else(|e| e.into_inner()) = actor.interest_views();
    }
    member_count.store(actor.member_count(), Ordering::Release);
    *present_presences.write().unwrap_or_else(|e| e.into_inner()) = actor.present_presences();
}

fn read_snapshot(actor: &InstanceActor) -> InstanceReadSnapshot {
    InstanceReadSnapshot {
        interest_views: actor.interest_views(),
        entities: actor.entities_snapshot(),
        members: actor.members_snapshot(),
        revision: actor.revision(),
        oldest_retained_revision: actor.oldest_retained_revision(),
        reliable_floor: actor.reliable_floor(),
        reliable_events: actor.reliable_events(),
    }
}

fn command_lane(command: &InstanceCommand) -> MailboxLane {
    match command {
        InstanceCommand::UpdateTransform { .. } => MailboxLane::Transform,
        InstanceCommand::Join { .. }
        | InstanceCommand::Leave { .. }
        | InstanceCommand::Shutdown
        | InstanceCommand::Tick => MailboxLane::Control,
        _ => MailboxLane::Entity,
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
fn panic_for_chaos_test() -> ! {
    panic!("intentional instance panic for E-4 chaos test");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::{CommandOutcome, InstanceActor, InstanceCommand, RuntimeRegistry};
    use crate::command::WorldPermissions;
    use crate::{InstanceRuntimeDescriptor, RuntimeState};
    use orbisync_domain::{EntityId, EntityKind, InstanceId, Timestamp, UserId, VisibilityPolicy};

    fn actor() -> InstanceActor {
        InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id: InstanceId::generate(),
            state: RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        })
    }

    #[tokio::test]
    async fn presence_cache_tracks_join_leave_without_rebuilding_on_tick() {
        let registry = RuntimeRegistry::new();
        let actor = actor();
        let instance_id = actor.descriptor().instance_id;
        let handle = registry.ensure_instance(actor);
        let presence_id = orbisync_domain::PresenceId::generate();
        handle
            .submit(InstanceCommand::Join {
                presence_id,
                instance_id,
                user_id: UserId::generate(),
                capacity: 2,
            })
            .await
            .unwrap();
        assert!(handle.is_present(presence_id));
        let joined = handle.present_presences.read().unwrap().clone();
        handle
            .tick(Timestamp::from_unix_millis(1).unwrap(), false)
            .await
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &joined,
            &handle.present_presences.read().unwrap()
        ));
        handle
            .submit(InstanceCommand::Leave { presence_id })
            .await
            .unwrap();
        assert!(!handle.is_present(presence_id));
        assert!(joined.contains(&presence_id));
    }

    #[tokio::test]
    async fn ticks_retain_effects_and_a_cancelled_drain_receiver_cannot_discard_them() {
        let registry = RuntimeRegistry::new();
        let handle = registry.ensure_instance(actor());
        let requester = UserId::generate();
        assert!(matches!(
            handle
                .submit(InstanceCommand::SpawnEntity {
                    command_id: None,
                    entity_id: EntityId::generate(),
                    kind: EntityKind::Object,
                    owner: Some(requester),
                    transform: None,
                    visibility: VisibilityPolicy::Global,
                    requester,
                    permissions: WorldPermissions::all(),
                })
                .await
                .unwrap(),
            CommandOutcome::Applied { .. }
        ));
        let tick = handle
            .advance_tick(Timestamp::from_unix_millis(1).unwrap())
            .await
            .unwrap();
        assert!(tick.events.is_empty() && tick.persistence_events.is_empty());
        let (reply, response) = tokio::sync::oneshot::channel();
        drop(response);
        handle
            .requests
            .send(super::RuntimeTaskRequest::DrainEffects { reply })
            .await
            .unwrap();
        let effects = handle.drain_effects().await.unwrap();
        assert_eq!(effects.events.len(), 1);
        assert_eq!(effects.persistence_events.len(), 1);
        let next = handle.drain_effects().await.unwrap();
        assert!(next.events.is_empty() && next.persistence_events.is_empty());
    }

    #[tokio::test]
    async fn chaos_instance_panic_isolated_to_one_task() {
        let registry = RuntimeRegistry::new();
        let failed = registry.ensure_instance(actor());
        let healthy = registry.ensure_instance(actor());

        failed
            .panic_for_chaos_test()
            .await
            .expect("panic request must reach the instance task");
        tokio::task::yield_now().await;

        assert!(
            failed.submit(InstanceCommand::Tick).await.is_err(),
            "panicked instance task must close its mailbox"
        );
        assert!(
            healthy.submit(InstanceCommand::Tick).await.is_ok(),
            "a panic in one instance must not stop another instance"
        );
    }

    #[tokio::test]
    async fn aborted_reap_republishes_the_same_state_and_outbox() {
        let registry = RuntimeRegistry::new();
        let actor = actor();
        let instance_id = actor.descriptor().instance_id;
        let entity_id = EntityId::generate();
        let requester = UserId::generate();
        let handle = registry.ensure_instance(actor);

        let outcome = handle
            .submit(InstanceCommand::SpawnEntity {
                command_id: None,
                entity_id,
                kind: EntityKind::Object,
                owner: Some(requester),
                transform: None,
                visibility: VisibilityPolicy::Global,
                requester,
                permissions: WorldPermissions::all(),
            })
            .await
            .expect("spawn request reaches actor");
        assert!(matches!(outcome, CommandOutcome::Applied { .. }));

        let reap = handle
            .reap_if_idle(Timestamp::from_unix_millis(1).expect("valid timestamp"))
            .await
            .expect("actor is idle");
        assert_eq!(reap.checkpoint.entity_count(), 1);
        assert_eq!(reap.events.len(), 1);
        assert!(handle.submit(InstanceCommand::Tick).await.is_err());

        let restarted = registry.restart_reaped_instance(reap);
        let snapshot = restarted.read_snapshot().await.expect("actor restarted");
        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].id(), entity_id);

        let tick = restarted
            .tick(
                Timestamp::from_unix_millis(2).expect("valid timestamp"),
                false,
            )
            .await
            .expect("restarted actor ticks");
        assert_eq!(tick.events.len(), 1, "drained events must be restored");
        assert_eq!(
            registry.handle(instance_id).map(|h| h.instance_id()),
            Some(instance_id)
        );
    }
}

/// Shared registry of instance task handles.
///
/// `inner` remains as a source-compatible compatibility view for existing
/// library callers and tests. Production code uses `ensure_instance`, `handle`
/// and `handles`; actors created through those methods are owned exclusively by
/// their Tokio task and are not inserted into `inner`.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistry {
    inner: Arc<Mutex<HashMap<InstanceId, InstanceActor>>>,
    tasks: Arc<Mutex<HashMap<InstanceId, InstanceHandle>>>,
    lifecycle: Arc<Mutex<HashMap<InstanceId, Weak<AsyncMutex<()>>>>>,
}

impl RuntimeRegistry {
    /// Remove only a stopped generation actor whose own mailbox confirms final
    /// durability. Ordinary remove_task remains unavailable for generation actors.
    pub async fn complete_generation_stop(&self, id: InstanceId) -> Option<Vec<ExtensionEvent>> {
        let handle = self.handle(id)?;
        let (reply, response) = oneshot::channel();
        if handle
            .requests
            .send(RuntimeTaskRequest::FinishGenerationStop { reply })
            .await
            .is_err()
        {
            return None;
        }
        let events = response.await.ok()??;
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if tasks
            .get(&id)
            .is_some_and(|current| current.requests.same_channel(&handle.requests))
        {
            tasks.remove(&id);
            Some(events)
        } else {
            None
        }
    }
    /// Submits a generation command without falling back to a legacy save path.
    pub async fn submit_prepared(
        &self,
        id: InstanceId,
        command: InstanceCommand,
        request: AdmissionRequest,
    ) -> Result<AdmissionResult, MailboxSendError> {
        if let Some(handle) = self.handle(id) {
            return handle.submit_prepared(command, request).await;
        }
        self.legacy_with_actor(id, |actor| actor.submit_prepared(command, request))
            .ok_or(MailboxSendError::Closed {
                lane: MailboxLane::Entity,
            })
    }
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires the process-local lifecycle barrier for one instance.
    ///
    /// First activation/join and idle reap hold this async guard across their
    /// respective publish or persist/remove sequence so an instance cannot be
    /// recreated from a stale checkpoint while its previous actor is stopping.
    /// Locks are keyed by instance so storage latency for one world does not
    /// block unrelated joins or reaps, and weak entries are pruned after use.
    pub async fn lifecycle_guard(&self, instance_id: InstanceId) -> OwnedMutexGuard<()> {
        self.lifecycle_lock(instance_id).lock_owned().await
    }

    /// Shared instance barrier. Long-lived connections cache this Arc so their
    /// publication hot path does not acquire the global lock map per message.
    #[must_use]
    pub fn lifecycle_lock(&self, instance_id: InstanceId) -> Arc<AsyncMutex<()>> {
        {
            let mut locks = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(lock) = locks.get(&instance_id).and_then(Weak::upgrade) {
                lock
            } else {
                locks.retain(|_, lock| lock.strong_count() != 0);
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(instance_id, Arc::downgrade(&lock));
                lock
            }
        }
    }

    /// Returns the legacy shared inner handle for compatibility tests only.
    ///
    /// Production code must use task handles. The test-support feature is
    /// enabled only by test package/dev dependencies.
    /// Task-owned instances are represented here by inert compatibility views;
    /// the live actor remains exclusively owned by its task.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn inner(&self) -> Arc<Mutex<HashMap<InstanceId, InstanceActor>>> {
        let inner = Arc::clone(&self.inner);
        let task_ids = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut legacy = inner.lock().unwrap_or_else(|e| e.into_inner());
        for id in task_ids {
            legacy.entry(id).or_insert_with(|| {
                InstanceActor::new(crate::InstanceRuntimeDescriptor {
                    instance_id: id,
                    state: RuntimeState::Running,
                    revision: Revision::INITIAL,
                })
            });
        }
        drop(legacy);
        inner
    }

    /// Ensures an actor is owned by a dedicated Tokio task and returns its handle.
    pub fn ensure_instance(&self, actor: InstanceActor) -> InstanceHandle {
        let id = actor.descriptor().instance_id;
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = tasks.get(&id) {
            return handle.clone();
        }
        let actor = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
            .unwrap_or(actor);
        let handle = InstanceHandle::spawn(actor);
        tasks.insert(id, handle.clone());
        handle
    }

    /// Returns a task handle for an instance, if it is task-owned.
    #[must_use]
    pub fn handle(&self, id: InstanceId) -> Option<InstanceHandle> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// Returns all task handles. Only the registry lock is held while cloning them.
    #[must_use]
    pub fn handles(&self) -> Vec<InstanceHandle> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Removes a task handle after its owner has stopped.
    pub fn remove_task(&self, id: InstanceId) -> bool {
        // Generation lifecycle removal requires phase 4's durable coordinator;
        // a legacy reap/removal cannot discard its dirty receipts or snapshot.
        if self
            .handle(id)
            .is_some_and(|h| h.generation_interest.is_some())
        {
            return false;
        }
        let removed = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
            .is_some();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        removed
    }

    /// Aborts an idle reap and republishes the same in-memory actor.
    ///
    /// Callers use this when the final checkpoint could not be serialized or
    /// persisted. The old task has already stopped, so its handle is removed
    /// before the retained actor is returned to the registry in `Running` state.
    /// This operation should be performed while holding
    /// [`Self::lifecycle_guard`] for the same instance so a concurrent
    /// activation cannot publish a second actor.
    pub fn restart_reaped_instance(&self, mut result: InstanceReapResult) -> InstanceHandle {
        let instance_id = result.stopped_actor.descriptor().instance_id;
        self.remove_task(instance_id);
        for event in result.events {
            let dropped = result.stopped_actor.outbox_mut().push(event);
            debug_assert!(
                !dropped,
                "re-inserting a freshly drained outbox must not exceed its capacity"
            );
        }
        for event in result.persistence_events {
            let dropped = result.stopped_actor.persistence_outbox_mut().push(event);
            debug_assert!(
                !dropped,
                "re-inserting a freshly drained persistence outbox must not exceed its capacity"
            );
        }
        result.stopped_actor.start();
        self.ensure_instance(result.stopped_actor)
    }

    /// Returns whether the registry contains either a task-owned or legacy actor.
    #[must_use]
    pub fn contains(&self, id: InstanceId) -> bool {
        if self.handle(id).is_some() {
            return true;
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&id)
    }

    /// Returns cached interest views without processing an actor under a registry lock.
    #[must_use]
    pub fn interest_views(&self, id: InstanceId) -> Option<Arc<crate::InterestSnapshot>> {
        if let Some(handle) = self.handle(id) {
            return Some(handle.cached_interest_views());
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(InstanceActor::interest_views)
    }

    /// Returns whether `presence` is currently a member of instance `id`,
    /// from the cached (not actor-round-trip) view, when the instance is
    /// known (ADR-025 "権限・所有・参加状態のwindow"). `None` when the
    /// instance itself is not known to this registry — a caller resolving
    /// "not found" separately should not treat that as "not present".
    #[must_use]
    pub fn is_present(&self, id: InstanceId, presence: PresenceId) -> Option<bool> {
        if let Some(handle) = self.handle(id) {
            return Some(handle.is_present(presence));
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(|actor| {
                actor
                    .members_snapshot()
                    .into_iter()
                    .any(|(member_presence, _)| member_presence == presence)
            })
    }

    /// Returns the aggregate member count from task caches and compatibility actors.
    #[must_use]
    pub fn member_count(&self) -> usize {
        let task_members = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(InstanceHandle::cached_member_count)
            .sum::<usize>();
        let legacy_members = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(InstanceActor::member_count)
            .sum::<usize>();
        task_members + legacy_members
    }

    /// Submits a command to the task owner, or to a legacy test actor outside
    /// the registry lock when no task handle exists.
    pub async fn submit(
        &self,
        id: InstanceId,
        command: InstanceCommand,
    ) -> Result<CommandOutcome, MailboxSendError> {
        if let Some(handle) = self.handle(id) {
            return handle.submit(command).await;
        }
        self.legacy_with_actor(id, |actor| actor.submit(command))
            .unwrap_or(Err(MailboxSendError::Closed {
                lane: MailboxLane::Control,
            }))
    }

    /// Submits a command synchronously for compatibility callers.
    ///
    /// Task-owned actors receive the command without crossing the registry
    /// lock. The legacy path temporarily removes the actor, performs the
    /// operation, and restores it so the lock is never held during processing.
    pub fn submit_sync(
        &self,
        id: InstanceId,
        command: InstanceCommand,
    ) -> Result<(), MailboxSendError> {
        if let Some(handle) = self.handle(id) {
            return handle.try_submit(command);
        }
        self.legacy_with_actor(id, |actor| actor.submit(command))
            .map(|_| ())
            .ok_or(MailboxSendError::Closed {
                lane: MailboxLane::Control,
            })
    }

    /// Reads an instance snapshot from its owner task.
    pub async fn reliable_event(
        &self,
        instance: InstanceId,
        presence: PresenceId,
        user: UserId,
        id: String,
    ) -> Result<Option<Arc<[u8]>>, &'static str> {
        if let Some(handle) = self.handle(instance) {
            return handle.reliable_event(presence, user, id).await;
        }
        self.legacy_with_actor(instance, |actor| actor.reliable_event(presence, user, &id))
            .unwrap_or(Err("SERVER_BUSY"))
    }

    /// Reads an instance snapshot from its owner task.
    pub async fn read_snapshot(&self, id: InstanceId) -> Option<InstanceReadSnapshot> {
        if let Some(handle) = self.handle(id) {
            return handle.read_snapshot().await;
        }
        self.legacy_with_actor(id, |actor| {
            actor
                .generation_reads_available()
                .then(|| read_snapshot(actor))
        })
        .flatten()
    }

    /// Reads only the target entity, consistently with actor mutations.
    pub async fn read_entity(&self, id: InstanceId, entity_id: EntityId) -> Option<Option<Entity>> {
        if let Some(handle) = self.handle(id) {
            return handle.read_entity(entity_id).await;
        }
        self.legacy_with_actor(id, |actor| {
            actor
                .generation_reads_available()
                .then(|| actor.entity_snapshot(entity_id))
        })
        .flatten()
    }

    /// Builds a checkpoint without holding the registry lock during actor work.
    pub async fn build_checkpoint(&self, id: InstanceId, now: Timestamp) -> Option<Checkpoint> {
        if let Some(handle) = self.handle(id) {
            return handle.build_checkpoint(now).await;
        }
        self.legacy_with_actor(id, |actor| {
            (!actor.uses_generation_admission()).then(|| actor.build_checkpoint(now))
        })
        .flatten()
    }

    /// Returns the number of registered instances.
    pub fn len(&self) -> Result<usize, String> {
        let legacy_ids = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        Ok(legacy_ids.len() + tasks.keys().filter(|id| !legacy_ids.contains(id)).count())
    }

    /// Returns true when no instance is registered.
    pub fn is_empty(&self) -> Result<bool, String> {
        Ok(self.len()? == 0)
    }

    /// Inserts an actor into the legacy compatibility view.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert(
        &self,
        id: InstanceId,
        actor: InstanceActor,
    ) -> Result<Option<InstanceActor>, String> {
        Ok(self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, actor))
    }

    /// Removes an actor from either registry view.
    #[cfg(any(test, feature = "test-support"))]
    pub fn remove(&self, id: &InstanceId) -> Result<Option<InstanceActor>, String> {
        if self.remove_task(*id) {
            return Ok(None);
        }
        Ok(self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id))
    }

    fn legacy_with_actor<R>(
        &self,
        id: InstanceId,
        operation: impl FnOnce(&mut InstanceActor) -> R,
    ) -> Option<R> {
        let actor = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)?;
        let mut actor = actor;
        let result = operation(&mut actor);
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, actor);
        Some(result)
    }
}
