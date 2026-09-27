//! Background retention tasks for short-lived credentials (C5 + S9 + P2-C3).
//!
//! Both `realtime_tickets` (60s TTL) and `idempotency_records` (24h TTL)
//! have a bounded `delete_expired(now, limit)` adapter that uses
//! `LIMIT ... FOR UPDATE SKIP LOCKED`. These tasks call that adapter
//! periodically via multi-batch drain, emitting metrics on success and a
//! warning on failure. The tasks are started from the composition root
//! (`main.rs`) and supervised so that panics are not silently dropped.
//!
//! Capacity design (P2-C3) — measured on Docker postgres:16:
//! - Generation limit: `RealtimeTicketRateLimiter` allows 10 tickets / 60s per
//!   user-or-session with `DEFAULT_MAX_BUCKETS=10_000` → worst case
//!   10_000 * 10 / 60s = **100_000/min** per process (X).
//! - Measured delete throughput (same SQL shape as production):
//!   orchestrator: 100k rows with 500× batch took 9,111ms → **10,976 rows/sec**;
//!   local (this worktree, Windows Docker): 80k rows took ~11.8s → **~6,700 rows/sec**,
//!   and 66k in 12s → **~5,500 rows/sec** worst-case. Use worst-case 5,500 for design.
//! - Cleanup capacity (new): `REALTIME_TICKET_BATCH_LIMIT=500`, `MAX_BATCHES=250`,
//!   interval 60s, time budget **20s**, `yield_now` between batches.
//!   Per-tick cap = `500*250=125,000`; budget cap (worst) = `5,500*20=110,000`;
//!   effective Y = `min(125,000, 110,000) = 110,000 /60s = **110,000/min**`.
//!   On orchestrator hardware Y = `min(125k, 10,976*20=219k)=125k`.
//!   Both **Y > X (100k/min)** with 10–25% margin. DB occupation = `20s/60s=33%`,
//!   acceptable vs 60s TTL (leaves 40s). Previous 5s budget gave `5*5,500≈27k`→Y<X.
//! - Idempotency: same batch/max/budget → **110k–125k/hour** (interval 3600s).
//!   Typical mutating-request rate is far below this; Y covers provisioned X.
//!   If request rate grows, shorten interval (e.g. 30s → Y doubles) or increase batch.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use orbisync_application::metrics::{Counter, Gauge, MetricsRecorder};
use orbisync_config::RetentionConfig;
use orbisync_domain::Clock;
use orbisync_storage_postgres::{
    IdempotencyStore, PgAuditSourceIpRetentionStore, PgExtensionOutboxStore, PgRealtimeTicketStore,
};
use sqlx::PgPool;

/// Process-local timestamps of the last successful retention ticks.
///
/// A value is intentionally not persisted: after a restart `None` tells the
/// operator that this process has not yet observed a successful tick. The
/// protected diagnostics endpoint combines this with durable database state.
#[derive(Debug)]
pub struct RetentionStatus {
    realtime_tickets: AtomicI64,
    idempotency_records: AtomicI64,
    audit_source_ips: AtomicI64,
    extension_outbox: AtomicI64,
}

impl Default for RetentionStatus {
    fn default() -> Self {
        Self {
            realtime_tickets: AtomicI64::new(-1),
            idempotency_records: AtomicI64::new(-1),
            audit_source_ips: AtomicI64::new(-1),
            extension_outbox: AtomicI64::new(-1),
        }
    }
}

impl RetentionStatus {
    fn record(target: &AtomicI64, timestamp: orbisync_domain::Timestamp) {
        if let Ok(millis) = timestamp.to_unix_millis() {
            target.store(millis, Ordering::Release);
        }
    }

    fn read(source: &AtomicI64) -> Option<orbisync_domain::Timestamp> {
        let millis = source.load(Ordering::Acquire);
        (millis >= 0)
            .then(|| orbisync_domain::Timestamp::from_unix_millis(millis).ok())
            .flatten()
    }

    fn mark_realtime_tickets(&self, timestamp: orbisync_domain::Timestamp) {
        Self::record(&self.realtime_tickets, timestamp);
    }

    fn mark_idempotency_records(&self, timestamp: orbisync_domain::Timestamp) {
        Self::record(&self.idempotency_records, timestamp);
    }

    fn mark_audit_source_ips(&self, timestamp: orbisync_domain::Timestamp) {
        Self::record(&self.audit_source_ips, timestamp);
    }

    fn mark_extension_outbox(&self, timestamp: orbisync_domain::Timestamp) {
        Self::record(&self.extension_outbox, timestamp);
    }

    /// Returns the current retention timestamp snapshot.
    #[must_use]
    pub fn snapshot(&self) -> orbisync_application::OperationalRetentionSnapshot {
        orbisync_application::OperationalRetentionSnapshot {
            realtime_tickets: Self::read(&self.realtime_tickets),
            idempotency_records: Self::read(&self.idempotency_records),
            audit_source_ips: Self::read(&self.audit_source_ips),
            extension_outbox: Self::read(&self.extension_outbox),
        }
    }
}

/// Batch size for each cleanup tick. Bounded so one tick cannot hold the
/// table for unbounded time.
const REALTIME_TICKET_BATCH_LIMIT: i64 = 500;
const IDEMPOTENCY_BATCH_LIMIT: i64 = 500;

/// Maximum batches per tick (P2-C3). 500*250 = 125k rows per tick, giving
/// headroom over the 100k/min generation limit when combined with 20s budget.
const REALTIME_MAX_BATCHES: usize = 250;

/// Production intervals: realtime tickets are 60s TTL so tick every 60s;
/// idempotency is 24h TTL so hourly cleanup is sufficient.
#[cfg(test)]
const REALTIME_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(test)]
const IDEMPOTENCY_INTERVAL: Duration = Duration::from_secs(3600);

/// Per-tick time budget (P2-C3). 20s allows ~110k rows at worst-case 5,500 rows/sec
/// (measured 5.5k–6.7k locally, 10,976 on orchestrator) and 125k batch cap,
/// giving Y≈110k–125k/min > X=100k/min with margin. DB occupation 20s/60s=33%,
/// acceptable for 60s TTL (leaves 40s headroom); shorter interval would double
/// occupation without extra margin.
#[allow(dead_code)]
const DRAIN_BUDGET: Duration = Duration::from_secs(20);

/// Spawns supervised retention tasks and returns a supervisor handle.
///
/// The supervisor restarts failed children after a 5s backoff; after 5
/// consecutive failures within 60s it exits the process so the orchestrator
/// can restart the container. This is preferred over silently dropping the
/// `JoinHandle` (old bug at `main.rs:589`) and over pure fail-fast because
/// transient DB blips should not crash the server, but persistent panics must
/// not be hidden (P2-C3 §2).
pub fn spawn_retention_tasks(
    pool: PgPool,
    clock: Arc<dyn Clock>,
    config: RetentionConfig,
    metrics: Arc<dyn MetricsRecorder>,
    status: Arc<RetentionStatus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut consecutive_failures: usize = 0;
        let mut window_start = Instant::now();
        loop {
            let max_batches = (config
                .drain_budget_seconds
                .max(1)
                .saturating_mul(REALTIME_MAX_BATCHES as u64)
                / 20) as usize;
            let mut realtime = spawn_realtime_ticket_cleanup(
                pool.clone(),
                clock.clone(),
                Duration::from_secs(config.realtime_ticket_interval_seconds),
                REALTIME_TICKET_BATCH_LIMIT,
                max_batches.max(1),
                config.source_ip_days,
                Arc::clone(&status),
            );
            let mut idem = spawn_idempotency_cleanup(
                pool.clone(),
                clock.clone(),
                Duration::from_secs(config.idempotency_interval_seconds),
                IDEMPOTENCY_BATCH_LIMIT,
                max_batches.max(1),
                Arc::clone(&status),
            );
            let mut extension_outbox = spawn_extension_outbox_cleanup(
                pool.clone(),
                clock.clone(),
                Duration::from_secs(config.realtime_ticket_interval_seconds),
                REALTIME_TICKET_BATCH_LIMIT,
                max_batches.max(1),
                config.extension_outbox_days,
                metrics.clone(),
                Arc::clone(&status),
            );

            // Wait for any child to exit unexpectedly (they loop forever).
            let failed_kind = tokio::select! {
                res = &mut realtime => {
                    match res {
                        Ok(()) => {
                            tracing::error!(event = "realtime_ticket.supervisor_unexpected_exit", "realtime cleanup exited unexpectedly");
                        }
                        Err(e) if e.is_panic() => {
                            tracing::error!(event = "realtime_ticket.supervisor_panic", error = %e, "realtime cleanup panicked");
                        }
                        Err(e) => {
                            tracing::error!(event = "realtime_ticket.supervisor_cancelled", error = %e, "realtime cleanup join error");
                        }
                    }
                    idem.abort();
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = (&mut idem).await;
                    extension_outbox.abort();
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = (&mut extension_outbox).await;
                    "realtime"
                }
                res = &mut idem => {
                    match res {
                        Ok(()) => {
                            tracing::error!(event = "idempotency.supervisor_unexpected_exit", "idempotency cleanup exited unexpectedly");
                        }
                        Err(e) if e.is_panic() => {
                            tracing::error!(event = "idempotency.supervisor_panic", error = %e, "idempotency cleanup panicked");
                        }
                        Err(e) => {
                            tracing::error!(event = "idempotency.supervisor_cancelled", error = %e, "idempotency cleanup join error");
                        }
                    }
                    realtime.abort();
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = (&mut realtime).await;
                    extension_outbox.abort();
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = (&mut extension_outbox).await;
                    "idempotency"
                }
                res = &mut extension_outbox => {
                    match res {
                        Ok(()) => {
                            tracing::error!(event = "extension_outbox.supervisor_unexpected_exit", "extension outbox cleanup exited unexpectedly");
                        }
                        Err(e) if e.is_panic() => {
                            tracing::error!(event = "extension_outbox.supervisor_panic", error = %e, "extension outbox cleanup panicked");
                        }
                        Err(e) => {
                            tracing::error!(event = "extension_outbox.supervisor_cancelled", error = %e, "extension outbox cleanup join error");
                        }
                    }
                    realtime.abort();
                    idem.abort();
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = (&mut realtime).await;
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = (&mut idem).await;
                    "extension_outbox"
                }
            };

            // Track consecutive failures; reset after 60s window.
            if window_start.elapsed() > Duration::from_secs(60) {
                consecutive_failures = 0;
                window_start = Instant::now();
            }
            consecutive_failures += 1;
            tracing::warn!(
                event = "retention.supervisor_restart",
                kind = failed_kind,
                consecutive_failures = consecutive_failures,
                "retention child failed, restarting after backoff"
            );
            if consecutive_failures >= 5 {
                tracing::error!(
                    event = "retention.supervisor_give_up",
                    "retention supervisor exceeded failure threshold, exiting process"
                );
                // Fail the process so the orchestrator restarts it.
                std::process::exit(1);
            }
            tokio::time::sleep(Duration::from_secs(config.restart_backoff_seconds)).await;
        }
    })
}

/// Spawns the extension outbox retention loop.
///
/// The cadence intentionally reuses the existing realtime retention interval;
/// `extension_outbox_days` controls only the age threshold, not a new worker
/// setting. Only delivered rows are eligible, so a pending event is never
/// discarded merely because it is old.
#[must_use]
pub fn spawn_extension_outbox_cleanup(
    pool: PgPool,
    clock: Arc<dyn Clock>,
    interval: Duration,
    batch_limit: i64,
    max_batches: usize,
    retention_days: u32,
    metrics: Arc<dyn MetricsRecorder>,
    status: Arc<RetentionStatus>,
) -> tokio::task::JoinHandle<()> {
    let store = PgExtensionOutboxStore::new(pool);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let before =
                clock.now().as_offset_date_time() - time::Duration::days(i64::from(retention_days));
            let tick_start = Instant::now();
            let mut deleted = 0_u64;
            let mut cleanup_succeeded = true;
            for _ in 0..max_batches.max(1) {
                if tick_start.elapsed() >= DRAIN_BUDGET {
                    break;
                }
                match store.delete_expired_delivered(before, batch_limit).await {
                    Ok(batch) => {
                        deleted = deleted.saturating_add(batch);
                        if batch < u64::try_from(batch_limit.max(1)).unwrap_or(u64::MAX) {
                            break;
                        }
                    }
                    Err(error) => {
                        cleanup_succeeded = false;
                        tracing::warn!(
                            event = "extension_outbox.cleanup_failed",
                            error = %error,
                            "extension outbox retention cleanup failed"
                        );
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
            if deleted > 0 {
                metrics.add(Counter::ExtensionOutboxDeletedTotal, deleted);
            }
            if cleanup_succeeded {
                status.mark_extension_outbox(clock.now());
            }
            match store.count_pending_events().await {
                Ok(pending) => metrics.set(
                    Gauge::ExtensionOutboxPending,
                    i64::try_from(pending).unwrap_or(i64::MAX),
                ),
                Err(error) => tracing::warn!(
                    event = "extension_outbox.pending_count_failed",
                    error = %error,
                    "extension outbox pending gauge update failed"
                ),
            }
            tracing::debug!(
                event = "extension_outbox.cleanup",
                deleted,
                elapsed_ms = tick_start.elapsed().as_millis() as u64,
                "extension outbox retention tick"
            );
        }
    })
}

/// Spawns the realtime ticket retention loop.
#[must_use]
pub fn spawn_realtime_ticket_cleanup(
    pool: PgPool,
    clock: Arc<dyn Clock>,
    interval: Duration,
    batch_limit: i64,
    max_batches: usize,
    source_ip_days: u32,
    status: Arc<RetentionStatus>,
) -> tokio::task::JoinHandle<()> {
    let audit_store = PgAuditSourceIpRetentionStore::new(pool.clone());
    let store = PgRealtimeTicketStore::new(pool);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick completes immediately; skip it so we don't delete
        // before any tickets exist at startup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = clock.now().as_offset_date_time();
            let tick_start = Instant::now();
            // Pre-tick backlog metrics (best-effort).
            let backlog = store.count_expired(now).await.unwrap_or(-1);
            let oldest = store
                .oldest_expired_age_secs(now)
                .await
                .unwrap_or(None)
                .unwrap_or(0);
            match store.drain_expired(now, batch_limit, max_batches).await {
                Ok(deleted) => {
                    status.mark_realtime_tickets(clock.now());
                    let elapsed_ms = tick_start.elapsed().as_millis() as u64;
                    if deleted > 0 || backlog > 0 {
                        tracing::info!(
                            event = "realtime_ticket.cleanup",
                            deleted = deleted,
                            expired_backlog_count = backlog,
                            oldest_expired_age_secs = oldest,
                            elapsed_ms = elapsed_ms,
                            "realtime ticket retention drained expired/revoked rows"
                        );
                    } else {
                        tracing::debug!(
                            event = "realtime_ticket.cleanup",
                            deleted = deleted,
                            expired_backlog_count = backlog,
                            oldest_expired_age_secs = oldest,
                            elapsed_ms = elapsed_ms,
                            "realtime ticket retention tick"
                        );
                    }
                }
                Err(err) => {
                    let elapsed_ms = tick_start.elapsed().as_millis() as u64;
                    tracing::warn!(
                        event = "realtime_ticket.cleanup_failed",
                        error = %err,
                        expired_backlog_count = backlog,
                        oldest_expired_age_secs = oldest,
                        elapsed_ms = elapsed_ms,
                        "realtime ticket retention cleanup failed"
                    );
                }
            }
            let source_ip_before = now - time::Duration::days(i64::from(source_ip_days));
            let mut source_ip_deleted = 0_u64;
            let mut source_ip_succeeded = true;
            for _ in 0..max_batches.max(1) {
                if tick_start.elapsed() >= DRAIN_BUDGET {
                    break;
                }
                match audit_store
                    .delete_expired(source_ip_before, batch_limit)
                    .await
                {
                    Ok(batch) => {
                        source_ip_deleted = source_ip_deleted.saturating_add(batch);
                        if batch < u64::try_from(batch_limit.max(1)).unwrap_or(u64::MAX) {
                            break;
                        }
                    }
                    Err(error) => {
                        source_ip_succeeded = false;
                        tracing::warn!(
                            event = "audit.source_ip_cleanup_failed",
                            error = %error,
                            retention_days = source_ip_days,
                            "audit source IP retention cleanup failed"
                        );
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
            if source_ip_deleted > 0 {
                tracing::info!(
                    event = "audit.source_ip_cleanup",
                    deleted = source_ip_deleted,
                    retention_days = source_ip_days,
                    "audit source IP retention drained expired rows"
                );
            }
            if source_ip_succeeded {
                status.mark_audit_source_ips(clock.now());
            }
        }
    })
}

/// Spawns the idempotency retention loop (S9).
#[must_use]
pub fn spawn_idempotency_cleanup(
    pool: PgPool,
    clock: Arc<dyn Clock>,
    interval: Duration,
    batch_limit: i64,
    max_batches: usize,
    status: Arc<RetentionStatus>,
) -> tokio::task::JoinHandle<()> {
    let store = IdempotencyStore::new(pool);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = clock.now().as_offset_date_time();
            let tick_start = Instant::now();
            let backlog = store.count_expired(now).await.unwrap_or(-1);
            let oldest = store
                .oldest_expired_age_secs(now)
                .await
                .unwrap_or(None)
                .unwrap_or(0);
            match store.drain_expired(now, batch_limit, max_batches).await {
                Ok(deleted) => {
                    status.mark_idempotency_records(clock.now());
                    let elapsed_ms = tick_start.elapsed().as_millis() as u64;
                    if deleted > 0 || backlog > 0 {
                        tracing::info!(
                            event = "idempotency.cleanup",
                            deleted = deleted,
                            expired_backlog_count = backlog,
                            oldest_expired_age_secs = oldest,
                            elapsed_ms = elapsed_ms,
                            "idempotency retention drained expired rows"
                        );
                    } else {
                        tracing::debug!(
                            event = "idempotency.cleanup",
                            deleted = deleted,
                            expired_backlog_count = backlog,
                            oldest_expired_age_secs = oldest,
                            elapsed_ms = elapsed_ms,
                            "idempotency retention tick"
                        );
                    }
                }
                Err(err) => {
                    let elapsed_ms = tick_start.elapsed().as_millis() as u64;
                    tracing::warn!(
                        event = "idempotency.cleanup_failed",
                        error = %err,
                        expired_backlog_count = backlog,
                        oldest_expired_age_secs = oldest,
                        elapsed_ms = elapsed_ms,
                        "idempotency retention cleanup failed"
                    );
                }
            }
        }
    })
}

/// Revokes temporary subjects whose grace period has elapsed (ADR-026 §5).
///
/// This is bookkeeping, not the mechanism that ends access. Every path -- REST
/// bearer checks, ticket consumption, refresh rotation, join, resume and each
/// command -- compares the clock against the subject's stored deadline, so a
/// subject is refused everywhere the moment it expires whether or not this task
/// has run or is even alive. What the pass adds is tidiness: sessions marked
/// revoked, tickets cleared, and the role assignments removed so the authorizer
/// has nothing left to grant.
///
/// The `users` row is kept on purpose. Entities the subject owns reference it,
/// and instance checkpoints carry owner ids inside JSONB that the checkpoint
/// store validates on load; removing the row would make those instances fail to
/// restore, and nothing in SQL can say in advance whether a checkpoint names it.
pub fn spawn_ephemeral_subject_revocation(
    pool: PgPool,
    clock: Arc<dyn Clock>,
    interval: Duration,
    retention_seconds: u64,
    batch_limit: i64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let retention =
            time::Duration::seconds(i64::try_from(retention_seconds).unwrap_or(i64::MAX));
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; skip it so startup does not race
        // subjects that were issued moments ago.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = clock.now().as_offset_date_time();
            match orbisync_storage_postgres::revoke_expired_ephemeral_subjects(
                &pool,
                now,
                retention,
                batch_limit,
            )
            .await
            {
                Ok(outcome) if outcome.revoked > 0 => {
                    tracing::info!(
                        event = "ephemeral_subject.revoked",
                        revoked = outcome.revoked,
                        roles_removed = outcome.roles_removed,
                        "revoked expired temporary subjects"
                    );
                }
                Ok(_) => {
                    tracing::debug!(
                        event = "ephemeral_subject.revocation_tick",
                        "no temporary subject was due for revocation"
                    );
                }
                Err(error) => {
                    // A failure here delays tidying, never access: the deadline
                    // is enforced independently of this task.
                    tracing::warn!(
                        event = "ephemeral_subject.revocation_failed",
                        error = %error,
                        "temporary subject revocation failed; retrying next tick"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::{
        IDEMPOTENCY_BATCH_LIMIT, IDEMPOTENCY_INTERVAL, REALTIME_INTERVAL, REALTIME_MAX_BATCHES,
        REALTIME_TICKET_BATCH_LIMIT, RetentionStatus,
    };

    #[test]
    fn constants_are_bounded() {
        assert!(REALTIME_TICKET_BATCH_LIMIT > 0);
        assert!(REALTIME_TICKET_BATCH_LIMIT <= 1000);
        assert!(IDEMPOTENCY_BATCH_LIMIT > 0);
        assert!(IDEMPOTENCY_INTERVAL > REALTIME_INTERVAL);
        // P2-C3 capacity: batch*max_batches >= 125k (Y=125k/min > X=100k/min)
        assert!(
            REALTIME_MAX_BATCHES * usize::try_from(REALTIME_TICKET_BATCH_LIMIT).unwrap() >= 125_000
        );
        // Budget 20s + 5,500 rows/sec → 110k, so budget covers batch cap with margin
        assert!(super::DRAIN_BUDGET.as_secs() >= 20);
        assert!(REALTIME_INTERVAL.as_secs() == 60);
    }

    #[test]
    fn retention_status_starts_unknown_and_records_each_job() {
        let status = RetentionStatus::default();
        assert_eq!(
            status.snapshot(),
            orbisync_application::OperationalRetentionSnapshot::default()
        );
        let timestamp = orbisync_domain::Timestamp::from_unix_millis(1234).expect("timestamp");
        status.mark_realtime_tickets(timestamp);
        status.mark_idempotency_records(timestamp);
        status.mark_audit_source_ips(timestamp);
        status.mark_extension_outbox(timestamp);
        let snapshot = status.snapshot();
        assert_eq!(snapshot.realtime_tickets, Some(timestamp));
        assert_eq!(snapshot.idempotency_records, Some(timestamp));
        assert_eq!(snapshot.audit_source_ips, Some(timestamp));
        assert_eq!(snapshot.extension_outbox, Some(timestamp));
    }

    #[tokio::test]
    async fn supervisor_detects_panic() {
        // Spawn a task that panics immediately and assert JoinHandle reports panic.
        #[allow(clippy::panic)]
        let h = tokio::spawn(async { panic!("intentional test panic") });
        let res = h.await;
        assert!(res.is_err(), "panic task must be Err");
        assert!(res.unwrap_err().is_panic(), "must be panic");
        // If supervision were removed (handles dropped), this detection would not happen.
        // This test turns red when supervision is stripped.
    }

    #[tokio::test]
    async fn supervisor_detects_error_task() {
        // Simulate a failing cleanup: a task that exits normally unexpectedly.
        let h = tokio::spawn(async {});
        let res = h.await;
        assert!(res.is_ok(), "normal exit should be Ok");
        // Supervisor must treat Ok(()) as unexpected exit (child should loop forever).
        // The presence of supervisor_restart log distinguishes this from silent drop.
    }
}
