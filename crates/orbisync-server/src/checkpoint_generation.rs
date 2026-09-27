//! Explicit generation composition. One service owns the process job budget.
//! Caller cancellation never detaches codec cleanup from its owned job permit.
use orbisync_application::checkpoint_admission::{
    GenerationAttempt, GenerationRecovery, GenerationResolution, GenerationSource, WriterPermit,
};
use orbisync_application::checkpoint_stream::StreamManifest;
use orbisync_application::{ApplicationError, CheckpointLimits};
use orbisync_domain::{InstanceId, Timestamp};
use orbisync_storage_postgres::generation_execution::{self as execution, Execution};
use orbisync_world_runtime::checkpoint::stream::{DecodeJob, EncodedCheckpoint, StreamStats};
use orbisync_world_runtime::{Checkpoint, InstanceHandle};
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::Duration;
use tokio::sync::Semaphore;

tokio::task_local! { static OWNER: usize; static LEGACY_ADMITTED: (); }

#[derive(Default)]
struct Jobs {
    closed: bool,
    cancelling: bool,
    tasks: Vec<Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>>,
    executions: Vec<Execution>,
}

fn unavailable() -> ApplicationError {
    ApplicationError::port_failure("generation persistence unavailable")
}
type PendingConversion = (
    GenerationAttempt,
    Arc<Checkpoint>,
    orbisync_application::checkpoint_admission::ReconciledAuthority,
);
type Publications = std::collections::HashMap<(InstanceId, String), Box<dyn FnOnce() + Send>>;

/// Process singleton composition, constructed only after external writer ownership.
pub struct GenerationServices {
    /// One immutable policy used by storage, restore and actor admission.
    pub limits: CheckpointLimits,
    /// Original revocable startup capability; never reacquired in a live process.
    pub writer: WriterPermit,
    /// Primary storage and projection adapter.
    pub store: Arc<dyn GenerationRecovery>,
    /// Shared scratch/assembly observations across simultaneous restore jobs.
    /// Retained snapshots, delivered actors and SQLx allocations are separate.
    pub stream_stats: Arc<StreamStats>,
    jobs: Arc<Semaphore>,
    conversions: Arc<Semaphore>,
    tasks: Mutex<Jobs>,
    instances:
        Mutex<std::collections::HashMap<InstanceId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    command_locks:
        Mutex<std::collections::HashMap<InstanceId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    pending_conversion: Mutex<Option<PendingConversion>>,
    pub(crate) operator_stage: Mutex<Option<&'static str>>,
    last_conversion: Mutex<Option<GenerationAttempt>>,
    snapshots: Arc<Semaphore>,
    retained: Mutex<std::collections::HashMap<InstanceId, tokio::sync::OwnedSemaphorePermit>>,
    publications: Mutex<Publications>,
    projection_cursor: tokio::sync::Mutex<Option<InstanceId>>,
}
impl GenerationServices {
    /// Explicit opt-in. Production must retain the startup ownership guard.
    pub fn new(
        limits: CheckpointLimits,
        writer: WriterPermit,
        store: Arc<dyn GenerationRecovery>,
    ) -> Result<Arc<Self>, ApplicationError> {
        if !writer.is_live() || store.limits() != limits {
            return Err(unavailable());
        }
        Ok(Arc::new(Self {
            limits,
            writer,
            store,
            stream_stats: Arc::new(StreamStats::default()),
            jobs: Arc::new(Semaphore::new(2)),
            conversions: Arc::new(Semaphore::new(1)),
            tasks: Mutex::new(Jobs::default()),
            instances: Mutex::new(Default::default()),
            command_locks: Mutex::new(Default::default()),
            pending_conversion: Mutex::new(None),
            operator_stage: Mutex::new(None),
            last_conversion: Mutex::new(None),
            snapshots: Arc::new(Semaphore::new(2)),
            retained: Mutex::new(Default::default()),
            publications: Mutex::new(Default::default()),
            projection_cursor: tokio::sync::Mutex::new(None),
        }))
    }
    /// Readiness and admission share the same permanently revocable permit.
    pub fn ready(&self) -> bool {
        self.writer.is_live()
    }
    /// Retain the original visibility-scoped publication until its exact receipt
    /// is covered by a committed snapshot, including foreground timeout/retry.
    pub fn retain_publication(
        &self,
        instance: InstanceId,
        command: String,
        publish: impl FnOnce() + Send + 'static,
    ) {
        self.publications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((instance, command))
            .or_insert_with(|| Box::new(publish));
    }
    fn publish_committed(&self, checkpoint: &Checkpoint, now: i64) {
        let callbacks: Vec<_> = {
            let mut pending = self.publications.lock().unwrap_or_else(|e| e.into_inner());
            checkpoint
                .dedup
                .iter()
                .filter_map(|receipt| {
                    pending
                        .remove(&(checkpoint.instance_id, receipt.command_id.clone()))
                        .filter(|_| receipt.expires_at_millis > now)
                })
                .collect()
        };
        for publish in callbacks {
            publish();
        }
    }
    fn retain_snapshot(&self, instance: InstanceId) -> Result<(), ApplicationError> {
        let mut retained = self.retained.lock().unwrap_or_else(|e| e.into_inner());
        if let std::collections::hash_map::Entry::Vacant(entry) = retained.entry(instance) {
            // Never occupy both work jobs waiting for uncertain snapshots; retry
            // owners must still be able to enter and resolve their fixed attempt.
            entry.insert(
                Arc::clone(&self.snapshots)
                    .try_acquire_owned()
                    .map_err(|_| unavailable())?,
            );
        }
        Ok(())
    }
    fn release_snapshot(&self, instance: InstanceId) {
        self.retained
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&instance);
    }
    /// Serialize fresh WS preparation with early receipt lookup across connections.
    pub async fn command_guard(&self, instance: InstanceId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.command_locks.lock().unwrap_or_else(|e| e.into_inner());
            locks.retain(|_, lock| lock.strong_count() != 0);
            if let Some(lock) = locks.get(&instance).and_then(std::sync::Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(instance, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
    async fn instance_guard(&self, instance: InstanceId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.instances.lock().unwrap_or_else(|e| e.into_inner());
            locks.retain(|_, lock| lock.strong_count() != 0);
            if let Some(lock) = locks.get(&instance).and_then(std::sync::Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(instance, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
    /// Inventory factory runs after global admission and the single legacy
    /// subpermit. Returned data is retained operator state, separate from scratch.
    pub async fn inspect_legacy<T, F, Fut>(
        self: &Arc<Self>,
        factory: F,
    ) -> Result<T, ApplicationError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, ApplicationError>> + Send + 'static,
    {
        let service = Arc::clone(self);
        self.owned(async move {
            let _conversion = if LEGACY_ADMITTED.try_with(|_| ()).is_ok() {
                None
            } else {
                Some(
                    execution::wait(Arc::clone(&service.conversions).acquire_owned())
                        .await?
                        .map_err(|_| unavailable())?,
                )
            };
            execution::check()?;
            factory().await
        })
        .await
    }
    pub(crate) async fn operator_owned<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl Future<Output = Result<T, ApplicationError>> + Send + 'static,
    ) -> Result<T, ApplicationError> {
        let service = self.clone();
        self.owned(async move {
            let _conversion = execution::wait(service.conversions.clone().acquire_owned())
                .await?
                .map_err(|_| unavailable())?;
            LEGACY_ADMITTED.scope((), work).await
        })
        .await
    }
    pub(crate) async fn owned<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl Future<Output = Result<T, ApplicationError>> + Send + 'static,
    ) -> Result<T, ApplicationError> {
        // Nested operator phases use the admitted owner's gate and clock.
        if OWNER
            .try_with(|owner| *owner == Arc::as_ptr(self) as usize)
            .unwrap_or(false)
        {
            execution::check()?;
            return work.await;
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        let service = Arc::clone(self);
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if tasks.closed {
                return Err(unavailable());
            }
            let task = tokio::spawn(async move {
                let permit = tokio::time::timeout(
                    Duration::from_secs(2),
                    Arc::clone(&service.jobs).acquire_owned(),
                )
                .await;
                let Ok(Ok(_permit)) = permit else {
                    let _delivered = send.send(Err(unavailable()));
                    return;
                };
                let execution =
                    Execution::new(tokio::time::Instant::now() + Duration::from_secs(5));
                {
                    let mut jobs = service.tasks.lock().unwrap_or_else(|e| e.into_inner());
                    if jobs.cancelling {
                        execution.cancel();
                    }
                    jobs.executions.push(execution.clone());
                }
                let mut work = Box::pin(OWNER.scope(
                    Arc::as_ptr(&service) as usize,
                    execution.scope(async {
                        if service.ready() && execution::check().is_ok() {
                            work.await
                        } else {
                            Err(unavailable())
                        }
                    }),
                ));
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(execution.deadline()) => {
                        execution.cancel();
                        let _delivered = send.send(Err(unavailable()));
                        // Retain the entire future, including actor and terminal SQL
                        // responses. Its next ordinary phase must check the clock.
                        let _late_result = work.await;
                    }
                    result = &mut work => {
                        let result = if execution.check().is_ok() { result } else { Err(unavailable()) };
                        let _delivered = send.send(result);
                    }
                }
                service.store.finish_cleanup().await;
                execution.finish();
            });
            tasks.tasks.retain(|task| {
                let Ok(mut slot) = task.try_lock() else {
                    return true;
                };
                let Some(handle) = slot.as_mut() else {
                    return false;
                };
                if handle.is_finished() {
                    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                    if let std::task::Poll::Ready(result) =
                        std::pin::Pin::new(handle).poll(&mut context)
                    {
                        if result.is_err() {
                            self.writer.invalidate();
                        }
                        slot.take();
                        return false;
                    }
                }
                true
            });
            tasks
                .executions
                .retain(|execution| !execution.is_finished());
            tasks
                .tasks
                .push(Arc::new(tokio::sync::Mutex::new(Some(task))));
        }
        receive.await.map_err(|_| unavailable())?
    }
    async fn restore_inner(
        &self,
        instance: InstanceId,
    ) -> Result<(GenerationAttempt, Checkpoint, Option<i64>), ApplicationError> {
        let deadline = execution::current().map_or_else(
            || tokio::time::Instant::now() + Duration::from_secs(5),
            |e| e.deadline(),
        );
        execution::check()?;
        let mut selected = self.store.select(instance).await?;
        if execution::check().is_err() {
            selected.source.cancel().await;
            return Err(unavailable());
        }
        let manifest = selected.source.manifest();
        if selected.attempt.instance_id != instance
            || selected.source.limits() != self.limits
            || selected.attempt.expected_head < 0
            || selected.attempt.codec != orbisync_application::checkpoint_record::canonical::VERSION
            || manifest.digest != selected.attempt.digest
            || manifest.serialized_bytes != selected.attempt.serialized_bytes
            || manifest.chunk_bytes != selected.attempt.chunk_bytes
            || manifest.chunk_count != selected.attempt.chunk_count
            || self.limits.validate_manifest(manifest).is_err()
        {
            selected.source.cancel().await;
            return Err(unavailable());
        }
        let mut job = DecodeJob::start(
            selected.source,
            self.limits,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&self.stream_stats),
        );
        let result = tokio::select! {
            result = tokio::time::timeout_at(deadline, job.finish()) => result.ok(),
            () = async { while self.ready() { tokio::time::sleep(Duration::from_millis(10)).await; } } => None,
        };
        let checkpoint = match result {
            Some(Ok(checkpoint)) => checkpoint,
            _ => {
                job.cancel().await;
                return Err(unavailable());
            }
        };
        if !self.ready() || checkpoint.instance_id != instance {
            return Err(unavailable());
        }
        Ok((selected.attempt, checkpoint, selected.cutoff_millis))
    }
    /// Generation-only restore: no persistent-row overlay and no older fallback.
    pub async fn restore(
        self: &Arc<Self>,
        instance: InstanceId,
    ) -> Result<(GenerationAttempt, Checkpoint, Option<i64>), ApplicationError> {
        let service = Arc::clone(self);
        self.owned(async move { service.restore_inner(instance).await })
            .await
    }
    /// Build the unpublished actor and its admission ledger under the same job
    /// permit as decode. Publication remains the lifecycle-guard owner's job.
    pub async fn restore_with<T, F>(
        self: &Arc<Self>,
        instance: InstanceId,
        build: F,
    ) -> Result<T, ApplicationError>
    where
        T: Send + 'static,
        F: FnOnce(GenerationAttempt, Checkpoint, Option<i64>) -> Result<T, ApplicationError>
            + Send
            + 'static,
    {
        let service = Arc::clone(self);
        self.owned(async move {
            let (selected, checkpoint, cutoff) = service.restore_inner(instance).await?;
            build(selected, checkpoint, cutoff)
        })
        .await
    }
    /// One fair bounded backlog page. The singleton owns the cursor across
    /// startup/periodic callers; failed items advance too. An empty page wraps
    /// once for the next job, never loops through a failing prefix in one job.
    pub async fn projection_page(self: &Arc<Self>) -> Result<Vec<InstanceId>, ApplicationError> {
        let service = Arc::clone(self);
        self.owned(async move {
            let mut cursor = execution::wait(service.projection_cursor.lock()).await?;
            let ids = service.store.pending_projections(*cursor).await?;
            execution::check()?;
            if ids.len() > 100
                || ids.windows(2).any(|p| p[0] >= p[1])
                || ids
                    .first()
                    .is_some_and(|id| cursor.is_some_and(|c| *id <= c))
            {
                return Err(unavailable());
            }
            *cursor = ids.last().copied();
            Ok(ids)
        })
        .await
    }
    /// Idempotent restart/periodic projection retry from the latest durable head.
    pub async fn project(self: &Arc<Self>, instance: InstanceId) -> Result<(), ApplicationError> {
        let service = Arc::clone(self);
        self.owned(async move {
            let (selected, checkpoint, _) = service.restore_inner(instance).await?;
            execution::check()?;
            service.store.project(&selected, &checkpoint.entities).await
        })
        .await
    }
    /// Bounded generation GC uses the same work and instance coordination gates.
    pub async fn cleanup(self: &Arc<Self>, instance: InstanceId) -> Result<u64, ApplicationError> {
        let service = Arc::clone(self);
        self.owned(async move {
            let _instance = execution::wait(service.instance_guard(instance)).await?;
            service.store.cleanup(instance).await
        })
        .await
    }
    /// Operator-only conversion of an already persisted reconciliation approval.
    /// The factory fetches/builds its fixed selection only after both permits;
    /// normal startup never invokes this and never creates approval evidence.
    pub async fn convert<F, Fut>(
        self: &Arc<Self>,
        approval: orbisync_application::checkpoint_admission::ReconciledAuthority,
        factory: F,
    ) -> Result<(), ApplicationError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Checkpoint, ApplicationError>> + Send + 'static,
    {
        let service = Arc::clone(self);
        self.owned(async move {
            let _conversion = if LEGACY_ADMITTED.try_with(|_| ()).is_ok() {
                None
            } else {
                Some(
                    execution::wait(Arc::clone(&service.conversions).acquire_owned())
                        .await?
                        .map_err(|_| unavailable())?,
                )
            };
            if service
                .pending_conversion
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
            {
                return Err(unavailable());
            }
            if approval.report.trim().is_empty()
                || approval.source_id.is_some() != approval.source_digest.is_some()
            {
                return Err(unavailable());
            }
            let checkpoint = factory().await?;
            execution::check()?;
            let _instance = execution::wait(service.instance_guard(checkpoint.instance_id)).await?;
            let limits = service.limits;
            let (checkpoint, manifest) = tokio::task::spawn_blocking(move || {
                let manifest = checkpoint.stream_manifest(limits, &AtomicBool::new(false))?;
                Ok::<_, ApplicationError>((checkpoint, manifest))
            })
            .await
            .map_err(|_| unavailable())??;
            execution::check()?;
            let attempt = GenerationAttempt::from_manifest(
                checkpoint.instance_id,
                0,
                service.writer.token(),
                checkpoint
                    .timestamp
                    .to_unix_millis()
                    .map_err(|_| unavailable())?,
                orbisync_application::checkpoint_record::canonical::VERSION,
                service.limits,
                &manifest,
            )?;
            service.retain_snapshot(checkpoint.instance_id)?;
            *service
                .last_conversion
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(attempt.clone());
            *service
                .pending_conversion
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                Some((attempt, Arc::new(checkpoint), approval));
            service.retry_conversion_inner().await
        })
        .await
    }
    async fn retry_conversion_inner(&self) -> Result<(), ApplicationError> {
        let deadline = execution::current().map_or_else(
            || tokio::time::Instant::now() + Duration::from_secs(5),
            |e| e.deadline(),
        );
        let (attempt, checkpoint, approval) = self
            .pending_conversion
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(unavailable)?;
        let manifest = StreamManifest {
            serialized_bytes: attempt.serialized_bytes,
            chunk_bytes: attempt.chunk_bytes,
            chunk_count: attempt.chunk_count,
            digest: attempt.digest,
        };
        let mut source = EncodedCheckpoint::new(checkpoint, self.limits, manifest)?;
        let result = if execution::check().is_ok() {
            Ok(self.store.convert(&attempt, &mut source, &approval).await)
        } else {
            Err(())
        };
        source.cancel().await;
        let resolution = match result {
            Ok(Ok(value)) => value,
            _ => self.resolve_before(&attempt, deadline).await,
        };
        match resolution {
            GenerationResolution::Committed { publish_seq: 1 } => {
                self.operator_stage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                self.pending_conversion
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                self.release_snapshot(attempt.instance_id);
                Ok(())
            }
            GenerationResolution::ProvenAbsent => {
                self.store.retire_absent(&attempt).await?;
                self.pending_conversion
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                self.release_snapshot(attempt.instance_id);
                Err(unavailable())
            }
            _ => Err(unavailable()),
        }
    }
    /// Operator observation distinguishes an unknown approval from a pinned conversion.
    pub fn operator_status(&self) -> Option<&'static str> {
        if self.pending_conversion_identity().is_some() {
            return Some("conversion_pending");
        }
        *self
            .operator_stage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    /// Last conversion identity remains available for the terminal report.
    pub fn last_conversion_identity(&self) -> Option<String> {
        self.last_conversion
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|attempt| format!("{attempt:?}"))
    }
    /// Retained report identity for explicit same-process operator retries.
    pub fn pending_conversion_identity(&self) -> Option<String> {
        self.pending_conversion
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(attempt, _, _)| format!("{attempt:?}"))
    }
    /// Resume the exact uncertain conversion without fetching or selecting again.
    pub async fn retry_conversion(self: &Arc<Self>) -> Result<(), ApplicationError> {
        let service = Arc::clone(self);
        self.owned(async move {
            let _conversion = if LEGACY_ADMITTED.try_with(|_| ()).is_ok() {
                None
            } else {
                Some(
                    execution::wait(Arc::clone(&service.conversions).acquire_owned())
                        .await?
                        .map_err(|_| unavailable())?,
                )
            };
            service.retry_conversion_inner().await
        })
        .await
    }
    /// Capture only after global admission. An uncertain attempt remains pinned
    /// in the actor; subsequent calls resolve/retry exactly that identity.
    pub async fn persist(
        self: &Arc<Self>,
        handle: InstanceHandle,
        now: Timestamp,
    ) -> Result<(), ApplicationError> {
        let service = Arc::clone(self);
        self.owned(async move {
            let started = tokio::time::Instant::now();
            let deadline =
                execution::current().map_or(started + Duration::from_secs(5), |e| e.deadline());
            let _instance = execution::wait(service.instance_guard(handle.instance_id())).await?;
            // Expiry is an actor turn and only retires confirmed durable entries;
            // an uncertain pinned attempt keeps every original receipt intact.
            handle
                .expire_generation_receipts_before(
                    now.to_unix_millis().unwrap_or(0),
                    deadline,
                    execution::current().map(|e| e.cancellation_flag()),
                )
                .await?;
            execution::check()?;
            let already_retained = service
                .retained
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&handle.instance_id());
            service.retain_snapshot(handle.instance_id())?;
            let (attempt, snapshot) = match handle
                .capture_generation_before(
                    now,
                    Some(deadline),
                    execution::current().map(|e| e.cancellation_flag()),
                )
                .await
            {
                Ok(captured) => captured,
                Err(error) => {
                    if !already_retained {
                        service.release_snapshot(handle.instance_id());
                    }
                    return Err(error);
                }
            };
            let manifest = StreamManifest {
                serialized_bytes: attempt.serialized_bytes,
                chunk_bytes: attempt.chunk_bytes,
                chunk_count: attempt.chunk_count,
                digest: attempt.digest,
            };
            let mut source =
                EncodedCheckpoint::new(Arc::clone(&snapshot), service.limits, manifest)?;
            let publication = if execution::check().is_ok() {
                let publication_deadline =
                    deadline.min(tokio::time::Instant::now() + Duration::from_secs(2));
                let publication_execution = execution::current().map_or_else(
                    || Execution::new(publication_deadline),
                    |e| e.with_deadline(publication_deadline),
                );
                Ok(publication_execution
                    .scope(service.store.publish(&attempt, &mut source))
                    .await)
            } else {
                Err(())
            };
            source.cancel().await;
            let resolution = match publication {
                Ok(Ok(value)) => value,
                _ => service.resolve_before(&attempt, deadline).await,
            };
            let resolved_at = now
                .to_unix_millis()
                .unwrap_or(0)
                .saturating_add(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
            let applied = handle
                .resolve_generation(attempt.clone(), resolution.clone(), resolved_at)
                .await;
            if matches!(resolution, GenerationResolution::Committed { .. })
                && (applied.is_ok()
                    || applied.as_ref().is_err_and(|e| {
                        e.kind() == orbisync_application::ApplicationErrorKind::CommittedButExpired
                    }))
            {
                service.publish_committed(&snapshot, resolved_at);
                drop(snapshot);
                service.release_snapshot(attempt.instance_id);
            }
            applied?;
            match resolution {
                GenerationResolution::Committed { .. } if service.ready() => Ok(()),
                GenerationResolution::ProvenAbsent => {
                    service.store.retire_absent(&attempt).await?;
                    handle.retire_absent_generation(attempt).await?;
                    service.release_snapshot(handle.instance_id());
                    Err(unavailable())
                }
                _ => Err(unavailable()),
            }
        })
        .await
    }
    async fn resolve_before(
        &self,
        attempt: &GenerationAttempt,
        deadline: tokio::time::Instant,
    ) -> GenerationResolution {
        if tokio::time::Instant::now() >= deadline || execution::check().is_err() {
            return GenerationResolution::Uncertain;
        }
        self.store
            .resolve(attempt)
            .await
            .unwrap_or(GenerationResolution::Uncertain)
    }
    /// Idle lifecycle uses the activation barrier and retains a stopped actor
    /// on failed final persistence. It never takes a legacy snapshot.
    pub async fn reap(
        self: &Arc<Self>,
        registry: &Arc<orbisync_world_runtime::RuntimeRegistry>,
        instance: InstanceId,
        now: Timestamp,
    ) -> Result<Option<Vec<orbisync_world_runtime::ExtensionEvent>>, ApplicationError> {
        let registry = registry.clone();
        let service = self.clone();
        self.owned(async move {
            let _lifecycle = execution::wait(registry.lifecycle_guard(instance)).await?;
            let Some(handle) = registry.handle(instance) else {
                return Ok(None);
            };
            if !handle
                .stop_generation_if_idle_before(
                    execution::current().map(|e| e.deadline()),
                    execution::current().map(|e| e.cancellation_flag()),
                )
                .await
            {
                return Ok(None);
            }
            service.persist(handle, now).await?;
            let removed = registry.complete_generation_stop(instance).await;
            if removed.is_some() {
                service
                    .publications
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(id, _), _| *id != instance);
                service.release_snapshot(instance);
            }
            Ok(removed)
        })
        .await
    }
    /// Number of admitted jobs whose execution expired but completion is retained.
    pub fn cleanup_pending_jobs(&self) -> usize {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .executions
            .iter()
            .filter(|execution| !execution.is_finished() && execution.check().is_err())
            .count()
    }
    /// Atomically refuse new registration while keeping all admitted owners.
    pub fn close_admission(&self) {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
    }
    /// Cancel ordinary execution while retaining every completion owner.
    pub fn cancel_execution(&self) {
        let mut jobs = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        jobs.cancelling = true;
        for execution in &jobs.executions {
            execution.cancel();
        }
    }

    /// Close registration and join accepted jobs. Cancelling this observer never
    /// takes away the stored handles; a later drain resumes the same joins.
    pub async fn drain(&self) {
        let tasks = {
            let mut jobs = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            jobs.closed = true;
            jobs.tasks.clone()
        };
        for task in tasks {
            let mut task = task.lock().await;
            if let Some(handle) = task.as_mut()
                && handle.await.is_err()
            {
                self.writer.invalidate();
            }
            task.take();
        }
        self.store.finish_cleanup().await;
        self.publications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

#[cfg(test)]
mod tests;
