//! Stop-before-start ownership. Never reconnect or replace a live boot token.
use super::*;
use orbisync_application::checkpoint_admission::WriterToken;
use sqlx::{Connection, PgConnection};
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::Arc,
    time::Duration,
};

/// Lifetime owner of the host lock and dedicated PostgreSQL session lock.
/// The designated-host supervisor must prove prior process exit before calling
/// acquire. This is deliberately not a distributed lease or failover mechanism.
pub struct PgWriterOwnership {
    permit: WriterPermit,
    task: tokio::task::JoinHandle<()>,
    _host: Arc<File>,
    pub(super) coordinators: Coordinators,
}
impl PgWriterOwnership {
    /// Acquire before readiness/listeners. `lock_path` must be the supervisor's
    /// stable deployment lock on the designated host, never a per-boot temp path.
    /// The operator configures deployment identity/role after old-writer fencing.
    pub async fn acquire(
        url: &str,
        lock_path: &Path,
        deployment: &str,
    ) -> Result<Self, ApplicationError> {
        let host = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| invalid())?;
        host.try_lock()
            .map_err(|_| ApplicationError::port_failure("deployment host lock unavailable"))?;
        let mut connection = PgConnection::connect(url).await.map_err(db)?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(19419,4)")
            .fetch_one(&mut connection)
            .await
            .map_err(db)?;
        if !acquired {
            return Err(ApplicationError::port_failure(
                "deployment PostgreSQL lock unavailable",
            ));
        }
        let boot = Uuid::now_v7();
        let epoch: i64 = sqlx::query_scalar("SELECT checkpoint_start_writer($1,$2,$3)")
            .bind(deployment)
            .bind(boot)
            .bind(orbisync_application::checkpoint_record::canonical::VERSION as i32)
            .fetch_one(&mut connection)
            .await
            .map_err(db)?;
        let permit = WriterPermit::new(WriterToken { epoch, boot })?;
        let live = permit.clone();
        let host = Arc::new(host);
        let held = Arc::clone(&host);
        let task = tokio::spawn(async move {
            let _held = held;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                match tokio::time::timeout(
                    Duration::from_secs(3),
                    sqlx::query("SELECT 1").execute(&mut connection),
                )
                .await
                {
                    Ok(Ok(_)) if live.is_live() => {}
                    _ => {
                        live.invalidate();
                        break;
                    }
                }
            }
            let _closed = connection.close().await;
        });
        Ok(Self {
            permit,
            task,
            _host: host,
            coordinators: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    /// Clone the permanently revocable capability for actors and adapters.
    pub fn permit(&self) -> WriterPermit {
        self.permit.clone()
    }
    /// Stop admission immediately and join the dedicated connection shutdown.
    pub async fn shutdown(mut self) {
        self.permit.invalidate();
        // The shutdown coordinator retains this consuming future through close.
        let _joined = (&mut self.task).await;
    }
}
impl Drop for PgWriterOwnership {
    fn drop(&mut self) {
        self.permit.invalidate();
        self.task.abort();
    }
}
