//! Stage boundaries use the same logical byte/control work units as the reader.
//! Each boundary observes cancellation before executing its bounded operation.
//! These counters are source reservations, not elapsed CPU or allocator time.
//! Async storage uses an inert flag and must yield between reserved units;
//! future cancellation is observed by the executor at those yield boundaries.
use super::{ScalarError, WORK_QUANTUM};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Audited synchronous work categories (parser includes scalar/domain/hash).
#[derive(Clone, Copy)]
pub enum Stage {
    /// Canonical parser including its internal reservations.
    Parser,
    /// String, scalar and record emission.
    Emit,
    /// Framing, lexical guards and output copies.
    Copy,
    /// Envelope or chunk hashing.
    Hash,
    /// Record acceptance and stream-relative validation.
    Domain,
    /// Identity heap operations.
    Index,
    /// Bounded destruction of retained or temporary state.
    Cleanup,
}
static PEAKS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static CALLS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
/// Observe a stage, including terminal cleanup which must continue on cancel.
pub fn observe(flag: &AtomicBool, stage: Stage, work: usize) -> Result<(), ScalarError> {
    assert!(work <= WORK_QUANTUM);
    PEAKS[stage as usize].fetch_max(work, Ordering::Relaxed);
    CALLS[stage as usize].fetch_add(1, Ordering::Relaxed);
    if flag.load(Ordering::Acquire) {
        Err(ScalarError::Cancelled)
    } else {
        Ok(())
    }
}
/// Process-wide maxima and observation counts; concurrent jobs may contribute.
pub fn evidence() -> ([usize; 7], [usize; 7]) {
    (
        std::array::from_fn(|i| PEAKS[i].load(Ordering::Relaxed)),
        std::array::from_fn(|i| CALLS[i].load(Ordering::Relaxed)),
    )
}
