//! Execution observation shared by the composition owner and PostgreSQL adapter.
//! Expiry stops new phases; completion/connection cleanup remains separately owned.
use orbisync_application::ApplicationError;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::time::Instant;

tokio::task_local! { static CURRENT: Execution; }

/// One absolute execution clock, shared by nested operations in an admitted job.
#[derive(Clone)]
pub struct Execution {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}
impl Execution {
    /// Construct an execution clock without changing admission/cleanup budgets.
    pub fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Clamp a subphase while preserving the parent's cancellation witness.
    pub fn with_deadline(&self, deadline: Instant) -> Self {
        let mut child = self.clone();
        child.deadline = self.deadline.min(deadline);
        child
    }
    /// Mark joined cleanup complete so the registry can discard this clock.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Release);
    }
    /// Whether all completion ownership has settled.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
    /// Stop initiation of ordinary work. This is not proof of database absence.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    /// Shared cancellation witness for a queued actor turn.
    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
    /// Absolute deadline for this attempt.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Run nested phases under this same clock.
    pub async fn scope<T>(&self, work: impl Future<Output = T>) -> T {
        CURRENT.scope(self.clone(), work).await
    }
    /// Refuse initiation of further ordinary work after expiry.
    pub fn check(&self) -> Result<(), ApplicationError> {
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            Err(ApplicationError::port_failure(
                "generation execution expired; completion may remain pending",
            ))
        } else {
            Ok(())
        }
    }
}
/// Current encompassing execution, if the caller is the generation job owner.
pub fn current() -> Option<Execution> {
    CURRENT.try_with(Clone::clone).ok()
}
/// Check before initiating another phase; standalone adapters retain SQL limits.
pub fn check() -> Result<(), ApplicationError> {
    current().map_or(Ok(()), |execution| execution.check())
}
/// Bound a cancellation-safe wait (semaphore/mutex only), never a consuming
/// transaction, actor response, codec completion, or blocking worker.
/// The acquired value must also be safe to drop if execution expired while waiting.
pub async fn wait<T>(work: impl Future<Output = T>) -> Result<T, ApplicationError> {
    check()?;
    let acquired = match current() {
        Some(execution) => tokio::time::timeout_at(execution.deadline(), work)
            .await
            .map_err(|_| ApplicationError::port_failure("generation execution expired")),
        None => Ok(work.await),
    }?;
    // A ready acquisition can win the timeout race, and cancellation can arrive
    // while queued. Release its guard instead of starting the next phase.
    check()?;
    Ok(acquired)
}
