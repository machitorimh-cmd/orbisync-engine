//! Explicitly retained, non-reusable transaction connections. No SQLx
//! Transaction drop/return-to-pool cleanup is hidden behind job completion.
use super::*;
use sqlx::{PgConnection, pool::PoolConnection};
use std::{
    ops::{Deref, DerefMut},
    sync::Mutex as StdMutex,
};

type Task = Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>;

#[derive(Debug, Default)]
pub(super) struct Cleanup {
    tasks: StdMutex<Vec<Task>>,
}
impl Cleanup {
    fn retain(&self, work: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|task| task.try_lock().map_or(true, |task| task.is_some()));
        tasks.push(Arc::new(Mutex::new(Some(tokio::spawn(work)))));
    }
    pub(super) async fn drain(&self) {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for task in tasks {
            let mut task = task.lock().await;
            if let Some(handle) = task.as_mut() {
                let _joined = handle.await;
            }
            task.take();
        }
    }
}

pub(super) struct OwnedTransaction {
    connection: Option<PoolConnection<Postgres>>,
    cleanup: Arc<Cleanup>,
    deadline: tokio::time::Instant,
    execution: Option<crate::generation_execution::Execution>,
}
impl OwnedTransaction {
    pub(super) async fn begin(
        pool: &PgPool,
        cleanup: Arc<Cleanup>,
    ) -> Result<Self, ApplicationError> {
        let execution = crate::generation_execution::current();
        let deadline = execution.as_ref().map_or_else(
            || tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            |e| e.deadline(),
        );
        crate::generation_execution::check()?;
        // Acquisition remains in the retained job. Once acquired, even BEGIN
        // errors transfer the connection into our explicit close owner.
        let mut connection = pool.acquire().await.map_err(db)?;
        connection.close_on_drop();
        let mut tx = Self {
            connection: Some(connection),
            cleanup,
            deadline,
            execution,
        };
        tx.check()?;
        sqlx::query("BEGIN").execute(&mut *tx).await.map_err(db)?;
        tx.check()?;
        Ok(tx)
    }
    pub(super) fn check(&self) -> Result<(), ApplicationError> {
        if let Some(execution) = &self.execution {
            execution.check()?;
        }
        if tokio::time::Instant::now() >= self.deadline {
            Err(invalid())
        } else {
            Ok(())
        }
    }
    pub(super) async fn commit(self) -> Result<(), ApplicationError> {
        self.check()?;
        self.terminal("COMMIT").await
    }
    pub(super) async fn rollback(self) -> Result<(), ApplicationError> {
        self.terminal("ROLLBACK").await
    }
    async fn terminal(mut self, statement: &'static str) -> Result<(), ApplicationError> {
        let mut connection = self.connection.take().ok_or_else(invalid)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let deadline = self.deadline;
        let execution = self.execution.clone();
        self.cleanup.retain(async move {
            let expired = tokio::time::Instant::now() >= deadline
                || execution.is_some_and(|e| e.check().is_err());
            let result = if statement == "COMMIT" && expired {
                Err(invalid())
            } else {
                sqlx::query(statement)
                    .execute(&mut *connection)
                    .await
                    .map_err(db)
            };
            // Always retire the connection, including a lost COMMIT ack. Close
            // completion proves local retirement, never primary absence.
            let closed = connection.close().await.map_err(db);
            let _delivered = send.send(result.and(closed).map(|_| ()));
        });
        receive.await.map_err(|_| invalid())?
    }
}
impl Deref for OwnedTransaction {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        // Connection is taken only by consuming terminal methods and Drop.
        #[allow(clippy::expect_used)]
        self.connection.as_ref().expect("live owned transaction")
    }
}
impl DerefMut for OwnedTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        #[allow(clippy::expect_used)]
        self.connection.as_mut().expect("live owned transaction")
    }
}
impl Drop for OwnedTransaction {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.cleanup.retain(async move {
                let _closed = connection.close().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn deadline78_terminal_cleanup_owner_survives_observer() {
        let cleanup = Arc::new(Cleanup::default());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let witness = Arc::new(());
        let weak = Arc::downgrade(&witness);
        let held = release.clone();
        cleanup.retain(async move {
            let _witness = witness;
            if let Ok(permit) = held.acquire().await {
                permit.forget();
            }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), cleanup.drain())
                .await
                .is_err()
        );
        assert!(weak.upgrade().is_some());
        release.add_permits(1);
        cleanup.drain().await;
        cleanup.drain().await;
        assert!(weak.upgrade().is_none());
    }
}
