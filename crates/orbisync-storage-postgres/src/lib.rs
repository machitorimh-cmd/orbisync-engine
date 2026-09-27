//! PostgreSQL adapter.
//!
//! Owns repository implementations, SQL, transactions and the conversion
//! between database rows and domain types (specification §30.7,
//! `architecture.md` §3). SQLx and PostgreSQL stay inside this crate
//! (`architecture.md` §2.1).
//!
//! # Migrations
//!
//! `migrations/` holds the forward-only SQL migrations applied with
//! `sqlx migrate` (ADR-005). Milestone 0 provides the migration runner and the
//! CI job and Milestone 1 provides the identity, idempotency, audit, and outbox
//! schema.
//!
//! # Dependency rule
//!
//! `storage-postgres` depends on `domain`, `config` and the `application` ports
//! it implements (`repo-crate-conventions.md` §3.2).

use std::time::Duration;

use orbisync_application::HealthProbe;
use orbisync_config::DatabaseConfig;
use sqlx::postgres::{PgPool, PgPoolOptions};

mod checkpoint;
mod ephemeral_subject;
mod extension;
mod extension_token;
mod generation;
pub mod generation_execution;
pub use extension_token::PgExtensionTokenStore;
mod idempotency;
mod identity;
mod identity_repository;
mod login;
mod persistent_entity;
mod query;
mod realtime_ticket;
mod rows;
mod source_ip_retention;
mod world;

pub use checkpoint::PgCheckpointStore;
pub use ephemeral_subject::{
    PgEphemeralSubjectScope, PgExternalIdentityStore, RevocationOutcome, resolve_ephemeral_roles,
    revoke_expired_ephemeral_subjects,
};
pub use extension::{
    ExtensionTerminalStateCounts, ExtensionTerminalStateReconciliation, PgExtensionOutboxStore,
    PgExtensionRegistrationStore,
};
pub use generation::{
    LegacyInventory, PgGenerationSource, PgGenerationStore, PgReconciliationOperator,
    PgWriterOwnership, ReconciledAuthority, ReconciliationDecision,
};
pub use idempotency::{IdempotencyClaim, IdempotencyCompletion, IdempotencyStore};
pub use identity::{
    AuditEvent, IdentityAdministrationStore, NewRefreshToken, NewUser, RefreshRotation,
};
pub use identity_repository::PgIdentityRepository;
pub use login::PgLoginStore;
pub use persistent_entity::PgPersistentEntityStore;
pub use query::PgIdentityQueryStore;
pub use realtime_ticket::PgRealtimeTicketStore;
pub use rows::{AuthSessionRow, IdempotencyRecordRow, RefreshTokenRow, UserRow};
pub use source_ip_retention::PgAuditSourceIpRetentionStore;
pub use world::{PgWorldAuthorizer, PgWorldDirectoryStore};

/// Embedded forward-only migrations (ADR-005).
///
/// The migrator is embedded at compile time, so the binary can apply
/// migrations without shipping the SQL files separately.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Failure while setting up the PostgreSQL adapter.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The connection pool could not be created.
    #[error("cannot create the PostgreSQL connection pool")]
    Pool(#[source] sqlx::Error),
    /// A migration failed to apply.
    #[error("cannot apply PostgreSQL migrations")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// A database operation failed.
    #[error("PostgreSQL persistence operation failed")]
    Database(#[source] sqlx::Error),
    /// A domain revision cannot be represented by PostgreSQL `BIGINT`.
    #[error("identity revision is outside the PostgreSQL range")]
    InvalidRevision,
    /// A login failure counter cannot be represented by PostgreSQL `INTEGER`.
    #[error("login failure count is outside the PostgreSQL range")]
    InvalidLoginFailureCount,
    /// Requested role does not exist.
    #[error("role not found")]
    RoleNotFound,
    /// Expected revision does not match stored revision.
    #[error("revision mismatch")]
    RevisionMismatch,
    /// Extension registration JSON or status is invalid.
    #[error("extension registration is invalid")]
    InvalidExtensionRegistration,
    /// Another active registration already holds one of the same
    /// pre-commit-hook capabilities (ADR-025 §2.7): at most one Active
    /// extension may hold a given `hooks:*` capability at a time.
    #[error("another active registration already holds this pre-commit capability")]
    DuplicatePrecommitCapability,
}

/// Creates a lazily connecting pool.
///
/// The pool does not connect until the first query, so the process starts even
/// while PostgreSQL is down; readiness then reports `false` until the database
/// answers (specification §26.1).
///
/// # Errors
///
/// Returns [`StorageError::Pool`] when the connection string is malformed.
pub fn create_pool(config: &DatabaseConfig, database_url: &str) -> Result<PgPool, StorageError> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_seconds))
        .connect_lazy(database_url)
        .map_err(StorageError::Pool)
}

/// Applies every pending migration.
///
/// # Errors
///
/// Returns [`StorageError::Migrate`] when a migration cannot be applied or when
/// an already applied migration was modified.
pub async fn run_migrations(pool: &PgPool) -> Result<(), StorageError> {
    MIGRATOR.run(pool).await.map_err(StorageError::Migrate)
}

/// Readiness probe backed by the connection pool.
#[derive(Debug, Clone)]
pub struct PgHealthProbe {
    pool: PgPool,
    readiness_timeout: Duration,
}

impl PgHealthProbe {
    /// Wraps a pool as a readiness probe.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            readiness_timeout: Duration::from_secs(3),
        }
    }

    /// Wraps a pool with a configured readiness timeout.
    #[must_use]
    pub const fn with_timeout(pool: PgPool, timeout_seconds: u64) -> Self {
        Self {
            pool,
            readiness_timeout: Duration::from_secs(timeout_seconds),
        }
    }
}

#[async_trait::async_trait]
impl HealthProbe for PgHealthProbe {
    fn name(&self) -> &'static str {
        "database"
    }

    async fn check(&self) -> Result<(), String> {
        // The reason is deliberately generic: a driver message can contain the
        // connection string (`observability-and-config.md` §2.3).
        let query = sqlx::query("SELECT 1").execute(&self.pool);
        match tokio::time::timeout(self.readiness_timeout, query).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(String::from("database is not reachable")),
            Err(_) => Err(String::from(
                "database did not answer within the readiness timeout",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MIGRATOR, create_pool};
    use orbisync_config::Config;

    #[tokio::test]
    async fn test_pool_creation_rejects_a_malformed_url() {
        let config = Config::default();
        assert!(create_pool(&config.database, "not-a-postgres-url").is_err());
    }

    #[tokio::test]
    async fn test_pool_creation_does_not_connect_eagerly() {
        let config = Config::default();
        // Nothing listens on this address; a lazy pool must still be created.
        let pool = create_pool(
            &config.database,
            "postgres://orbisync:orbisync@127.0.0.1:1/orbisync",
        )
        .expect("lazy pool creation must not connect");
        assert_eq!(pool.size(), 0);
    }

    #[test]
    fn test_migrator_embeds_all_schema_versions() {
        // Includes checkpoint generation/codec/projection and service tokens.
        let versions: Vec<_> = MIGRATOR.iter().map(|migration| migration.version).collect();
        assert_eq!(versions, (1_i64..=22).collect::<Vec<_>>());
    }
}
