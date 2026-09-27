//! Process-wide shutdown state shared by HTTP and realtime handlers.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::{OwnedRwLockReadGuard, RwLock, broadcast};

/// The two notifications sent to an accepted realtime connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownNotice {
    /// Stop admitting new work and finish already accepted work.
    Begin,
    /// The drain deadline expired; close what remains.
    Force,
}

/// Shared lifecycle state for graceful shutdown.
pub struct ShutdownState {
    rejecting: Arc<AtomicBool>,
    notices: broadcast::Sender<ShutdownNotice>,
    admissions: Arc<RwLock<()>>,
}

impl ShutdownState {
    /// Creates a running shutdown state.
    #[must_use]
    pub fn new() -> Self {
        let (notices, _) = broadcast::channel(32);
        Self {
            rejecting: Arc::new(AtomicBool::new(false)),
            notices,
            admissions: Arc::new(RwLock::new(())),
        }
    }

    /// Returns the flag used by HTTP readiness and admission checks.
    #[must_use]
    pub fn rejecting_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.rejecting)
    }

    /// Marks the process unready. This operation is idempotent.
    pub fn begin(&self) {
        self.rejecting.store(true, Ordering::Release);
    }

    /// Returns whether shutdown admission controls are active.
    #[must_use]
    pub fn is_rejecting(&self) -> bool {
        self.rejecting.load(Ordering::Acquire)
    }

    /// Admits one realtime operation. The flag load is its acceptance point;
    /// the guard keeps actor draining/final capture behind accepted work.
    /// No guard is held while an idle connection waits for another message.
    pub fn admit(&self) -> Option<OwnedRwLockReadGuard<()>> {
        let guard = self.admissions.clone().try_read_owned().ok()?;
        if self.is_rejecting() {
            return None;
        }
        Some(guard)
    }

    /// Called after begin, within the existing outer shutdown deadline.
    /// Does not cancel accepted operations or open a new timeout budget.
    pub async fn drain_admissions(&self) {
        debug_assert!(self.is_rejecting());
        drop(self.admissions.write().await);
    }

    /// Subscribes an accepted connection to lifecycle notices.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ShutdownNotice> {
        self.notices.subscribe()
    }

    /// Notifies accepted connections that the server is draining.
    pub fn notify_connections(&self) {
        drop(self.notices.send(ShutdownNotice::Begin));
    }

    /// Notifies accepted connections that the force deadline expired.
    pub fn force_connections(&self) {
        drop(self.notices.send(ShutdownNotice::Force));
    }
}

impl Default for ShutdownState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ShutdownNotice, ShutdownState};
    use std::sync::atomic::Ordering;

    #[test]
    fn readiness_flag_and_connection_notices_are_ordered() {
        let state = ShutdownState::new();
        let mut receiver = state.subscribe();
        assert!(!state.is_rejecting());
        assert!(!state.rejecting_flag().load(Ordering::Acquire));

        state.begin();
        assert!(state.is_rejecting());
        assert!(receiver.try_recv().is_err());

        state.notify_connections();
        assert_eq!(
            receiver.try_recv().expect("begin notice"),
            ShutdownNotice::Begin
        );
        state.force_connections();
        assert_eq!(
            receiver.try_recv().expect("force notice"),
            ShutdownNotice::Force
        );
    }
}
