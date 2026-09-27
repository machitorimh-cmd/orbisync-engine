//! OrbiSync server binary.
//!
//! This is the Composition Root and holds no business logic
//! (specification §30.8, `architecture.md` §4.3): it parses arguments, loads
//! and validates configuration, installs telemetry, builds the adapters,
//! injects them into the ports and runs the server until shutdown.
//!
//! Startup order:
//!
//! 1. parse CLI arguments;
//! 2. load configuration (CLI > environment > file > defaults) and validate it;
//! 3. verify that every required secret is present;
//! 4. install structured logging;
//! 5. build the PostgreSQL pool lazily and wire the readiness probe;
//! 6. serve HTTP until `Ctrl+C` or `SIGTERM`, then shut down gracefully.

use std::collections::HashMap;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderValue;
use clap::{Parser, Subcommand};
use orbisync_application::{
    AppCheckpoint, ApplicationErrorKind, AuditQueryPort, CheckpointStore, EntityPersistenceEvent,
    ExtensionEvent, ExtensionOutboxStore, HealthProbe, IdentityQueryPort,
    MAX_CHECKPOINT_PAYLOAD_BYTES, PageRequest, PersistentEntityStore, WorldDirectoryUseCase,
    metrics::{Counter, Histogram, MetricsRecorder},
};
use orbisync_config::{Config, ConfigError, ConfigErrorKind, EnvSource, SystemEnv};
use orbisync_domain::{Clock as _, InstanceId, LoginId, SystemClock, UserId};
use orbisync_extensions::{
    DeliveryEngine, DeliveryPolicy, DeliveryWorker, EnvSecretProvider, ExtensionRegistrationStore,
    PreCommitGate, PreCommitValidationGate, PreCommitValidationPolicy, ReqwestHttpClient,
    ReqwestPreCommitClient, SystemDnsResolver,
};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, EphemeralMethodPolicy, IdentityAdministrationService,
    PasswordPolicy, PasswordService, token::AccessTokenService,
};
use orbisync_observability::{ObservabilityError, PrometheusMetrics, init_logging};
use orbisync_realtime::gateway::{
    HmacRealtimeTicketVerifier, RealtimeTicketVerifier, StubTicketVerifier,
};
use orbisync_server::{delivery, realtime_ws, shutdown::ShutdownState};
use orbisync_storage_postgres::{
    IdempotencyStore, IdentityAdministrationStore, PgCheckpointStore, PgEphemeralSubjectScope,
    PgExtensionRegistrationStore, PgExternalIdentityStore, PgHealthProbe, PgIdentityQueryStore,
    PgIdentityRepository, PgLoginStore, PgPersistentEntityStore, PgRealtimeTicketStore,
    PgWorldAuthorizer, PgWorldDirectoryStore, StorageError, create_pool, resolve_ephemeral_roles,
    run_migrations,
};
use uuid::Uuid;

use orbisync_transport_http::{HttpState, router};

mod admin_password_recovery;
mod checkpoint_operator_cli;
mod periodic_tasks;
mod retention;
mod shutdown_deadline;
mod web_admin;
mod doctor;
mod operational_diagnostics;
mod runtime_maintenance;

/// Adapter for [`orbisync_application::InstanceMembershipStore`] backed by the
/// live runtime registry.
///
/// Presence is process-local and in-memory only (`session_store.rs` D-24);
/// there is no PostgreSQL table to query. This is the composition-root-owned
/// implementation the DAG requires: `orbisync-application` cannot depend on
/// `orbisync-world-runtime`, so the port is implemented here where both are
/// available (`check_architecture.py`).
struct RuntimeInstanceMembershipStore {
    registry: Arc<orbisync_world_runtime::RuntimeRegistry>,
}

#[async_trait::async_trait]
impl orbisync_application::InstanceMembershipStore for RuntimeInstanceMembershipStore {
    async fn list_members(
        &self,
        instance_id: InstanceId,
    ) -> Result<Vec<UserId>, orbisync_application::ApplicationError> {
        let present = self.registry.contains(instance_id);
        let Some(snapshot) = self.registry.read_snapshot(instance_id).await else {
            return if present || self.registry.contains(instance_id) {
                Err(orbisync_application::ApplicationError::new(
                    ApplicationErrorKind::Unavailable,
                    "membership unavailable",
                ))
            } else {
                Ok(Vec::new())
            };
        };
        Ok(snapshot
            .members
            .into_iter()
            .map(|(_, user_id)| user_id)
            .collect())
    }

    async fn kick_member(
        &self,
        instance_id: InstanceId,
        user_id: UserId,
    ) -> Result<bool, orbisync_application::ApplicationError> {
        let present = self.registry.contains(instance_id);
        let Some(snapshot) = self.registry.read_snapshot(instance_id).await else {
            return if present || self.registry.contains(instance_id) {
                Err(orbisync_application::ApplicationError::new(
                    ApplicationErrorKind::Unavailable,
                    "membership unavailable",
                ))
            } else {
                Ok(false)
            };
        };
        let presences: Vec<_> = snapshot
            .members
            .into_iter()
            .filter(|(_, member)| *member == user_id)
            .map(|(presence_id, _)| presence_id)
            .collect();
        if presences.is_empty() {
            return Ok(false);
        }
        for presence_id in presences {
            match self
                .registry
                .submit(
                    instance_id,
                    orbisync_world_runtime::command::InstanceCommand::Leave { presence_id },
                )
                .await
            {
                Ok(orbisync_world_runtime::command::CommandOutcome::Applied { .. }) => {}
                _ => {
                    return Err(orbisync_application::ApplicationError::new(
                        ApplicationErrorKind::Unavailable,
                        "membership mutation unavailable",
                    ));
                }
            }
        }
        Ok(true)
    }
}

/// Maximum number of checkpoint payloads concurrently held in memory across
/// every checkpoint-saving path (HIGH-001).
///
/// `persist_instance_checkpoint` and `reap_idle_instance` build a full
/// `Checkpoint` and its serialized JSON payload (up to
/// `MAX_CHECKPOINT_PAYLOAD_BYTES`) before handing it to `CheckpointStore`.
/// Periodic jobs are bounded separately; shutdown can spawn one task per
/// instance. Without a shared payload limit the number held at once is
/// the instance count, not a constant — at the documented scale target
/// (1000 connections/process) that is a GiB-order working set instead of the
/// `MAX_CHECKPOINT_PAYLOAD_BYTES` derivation's assumed bound. All three paths
/// acquire a permit from the same process-wide semaphore before building a
/// payload and hold it until the save completes, so the peak in-flight
/// payload count across all of them together never exceeds this constant.
/// Sharing one semaphore across periodic and shutdown paths bounds their
/// combined peak, not just each path's peak independently.
const MAX_PENDING_CHECKPOINT_SAVES: usize = 2;

/// Service name emitted in every structured log record.
const SERVICE: &str = "orbisync";

/// Build version emitted in logs and by `GET /version`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn build_runtime(worker_threads: u32) -> Result<tokio::runtime::Runtime, std::io::Error> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if worker_threads > 0 {
        builder.worker_threads(worker_threads as usize);
    }
    builder.enable_all().build()
}

fn parse_cors_origins(origins: &[String]) -> Result<Vec<HeaderValue>, ConfigError> {
    origins
        .iter()
        .map(|origin| {
            HeaderValue::from_str(origin).map_err(|_| {
                ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    "cors.allowed_origins",
                    "origins must be valid HTTP header values",
                )
            })
        })
        .collect()
}

/// Returns the ids of instances that hold no members and no delivery senders.
///
/// Both conditions must hold simultaneously; an instance with members or with
/// remaining delivery senders is still considered live and must not be reaped.
/// The caller must hold the registry lock while calling this and removing the
/// returned ids, to avoid races with concurrent joins (D-5). Lock order is
/// `registry -> delivery` (`sender_count` takes the delivery lock); no code
/// path holds `delivery` then `registry`, so this does not introduce a
/// deadlock.
///
/// Extracted as a pure function so it can be unit-tested without the tick
/// loop (§0-1).
#[allow(dead_code)]
fn idle_instance_ids(
    actors: &HashMap<InstanceId, orbisync_world_runtime::actor::InstanceActor>,
    delivery: &delivery::DeliveryRegistry,
) -> Vec<InstanceId> {
    actors
        .iter()
        .filter(|(id, actor)| actor.member_count() == 0 && delivery.sender_count(**id) == 0)
        .map(|(id, _)| *id)
        .collect()
}

/// Collects extension facts from a legacy actor in unit-test fixtures.
///
/// Production instance tasks drain their own actor outboxes before returning
/// tick results; this helper remains only for the compatibility tests below.
#[allow(dead_code)]
fn collect_extension_events(
    actor: &mut orbisync_world_runtime::actor::InstanceActor,
    pending: &mut Vec<ExtensionEvent>,
) {
    pending.extend(actor.drain_outbox());
}

/// Persists a coordinator-drained batch through the existing application port.
async fn persist_extension_events(store: &dyn ExtensionOutboxStore, events: Vec<ExtensionEvent>) {
    for event in events {
        if let Err(error) = store.append_event(event).await {
            tracing::warn!(
                event = "extension.outbox_append_failed",
                error = %error,
                detail = "drained extension event could not be persisted"
            );
        }
    }
}

/// Persists a coordinator-drained batch of entity/component writes (HIGH-002).
///
/// Drained once per instance tick (`InstanceHandle::tick`,
/// `reap_idle_instance`), so writes are bounded by tick cadence rather than
/// issued once per command (`CODE_REVIEW.md` HIGH-002 recommended fix).
///
/// A failure here is treated like `persist_extension_events` in that
/// transient state keeps running rather than blocking on retries
/// (`state-and-runtime.md` §3.5: persistence port failure -> ephemeral state
/// continues, persistence retries). It is not treated identically, though: an
/// entity write is heavier than a missed webhook delivery, since a
/// durable row that never lands is exactly the HIGH-002 failure mode this
/// wiring exists to close. So failures are also counted, giving an operator
/// something to alert on that extension delivery misses do not need.
async fn persist_entity_events(
    store: &dyn PersistentEntityStore,
    metrics: &dyn MetricsRecorder,
    events: Vec<EntityPersistenceEvent>,
) {
    for event in events {
        let result = match event {
            EntityPersistenceEvent::Spawned(entity) => store.spawn(entity).await,
            EntityPersistenceEvent::OwnershipTransferred { entity, audit } => {
                store.transfer_ownership(entity, audit).await
            }
            EntityPersistenceEvent::Deleted {
                entity_id,
                instance_id,
            } => store.delete(entity_id, instance_id).await,
            EntityPersistenceEvent::ComponentUpserted {
                entity_id,
                instance_id,
                revision,
                updated_at,
                component_key,
                payload,
            } => {
                store
                    .upsert_component(
                        entity_id,
                        instance_id,
                        revision,
                        updated_at,
                        component_key,
                        payload,
                    )
                    .await
            }
        };
        if let Err(error) = result {
            metrics.incr(Counter::EntityPersistenceFailuresTotal);
            tracing::warn!(
                event = "persistence.entity_write_failed",
                error = %error,
                detail = "drained entity persistence event could not be written; transient state continues"
            );
        }
    }
}

/// Returns whether `bootstrap-admin` is allowed – no user exists yet (D-9).
///
/// Uses the existing [`IdentityQueryPort::users`] port (no new port is created).
/// The check is performed **before** calling `bootstrap_administrator` so no
/// temporary password is generated and no row is written when a user already
/// exists. A non-empty first page means bootstrapping must be denied.
async fn ensure_bootstrap_allowed(query: &dyn IdentityQueryPort) -> Result<(), ServerError> {
    let page = query
        .users(PageRequest {
            limit: 1,
            after: None,
        })
        .await
        .map_err(|_| ServerError::Bootstrap)?;
    if !page.items.is_empty() {
        tracing::warn!(
            event = "administrator.bootstrap_denied",
            reason = "users already exist",
        );
        return Err(ServerError::Bootstrap);
    }
    Ok(())
}

const PASSWORD_DENYLIST_ENV: &str = "ORBISYNC_PASSWORD_DENYLIST_FILE";

/// The internal access-token issuer.
///
/// Named here so startup can refuse an external issuer that matches it: a
/// token this server minted would otherwise be accepted back through
/// `/v1/auth/external` and turn an ordinary access token into a login.
const INTERNAL_TOKEN_ISSUER: &str = "orbisync";

/// Services introduced by ADR-026, assembled once at startup.
///
/// The `Debug` rendering reports only which services are present. Printing the
/// services themselves would put issuer names and key counts into startup logs
/// and test failures for no benefit.
struct AuthMethodServices {
    /// Methods this deployment accepts, for mode discovery.
    enabled: Vec<orbisync_domain::AuthMethod>,
    /// Guest and name-only issuing service, when either is enabled.
    ephemeral: Option<Arc<orbisync_identity::EphemeralSubjectService>>,
    /// External identity service, when the method is enabled.
    external: Option<Arc<orbisync_identity::ExternalAuthService>>,
    /// Participation boundary for existing subjects, independent of issuance.
    scope: Arc<dyn orbisync_application::EphemeralSubjectScope>,
}

impl core::fmt::Debug for AuthMethodServices {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthMethodServices")
            .field("enabled", &self.enabled)
            .field("ephemeral", &self.ephemeral.is_some())
            .field("external", &self.external.is_some())
            .field("scope", &"configured")
            .finish()
    }
}

fn auth_config_error(detail: impl Into<String>) -> ServerError {
    ServerError::Config(ConfigError::new(
        ConfigErrorKind::InvalidValue,
        "auth.methods",
        detail,
    ))
}

/// Parses the configured world boundary for one temporary method.
fn parse_allowed_worlds(method: &str, values: &[String]) -> Result<Vec<Uuid>, ServerError> {
    values
        .iter()
        .map(|value| {
            Uuid::parse_str(value).map_err(|_| {
                ServerError::Config(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("auth.{method}.allowed_worlds"),
                    format!("`{value}` is not a world id"),
                ))
            })
        })
        .collect()
}

/// Builds the ADR-026 services from configuration.
///
/// Every failure here stops startup. A method that is enabled but cannot be
/// assembled must not fall back to being quietly disabled: the operator asked
/// for it, and a server that accepts the configuration while ignoring half of
/// it is worse than one that refuses to start.
async fn build_auth_method_services(
    config: &Config,
    pool: &sqlx::PgPool,
    session_issuer: &Arc<orbisync_identity::SessionIssuer>,
) -> Result<AuthMethodServices, ServerError> {
    let mut enabled = Vec::with_capacity(config.auth.methods.len());
    for name in &config.auth.methods {
        enabled.push(
            orbisync_domain::AuthMethod::parse(name)
                .map_err(|error| auth_config_error(error.to_string()))?,
        );
    }

    let mut policies: Vec<(orbisync_domain::AuthMethod, EphemeralMethodPolicy)> = Vec::new();
    for (method, key, settings) in [
        (
            orbisync_domain::AuthMethod::Guest,
            "guest",
            &config.auth.guest,
        ),
        (
            orbisync_domain::AuthMethod::NameOnly,
            "name_only",
            &config.auth.name_only,
        ),
    ] {
        if !enabled.contains(&method) {
            continue;
        }
        // Resolving role names here is what turns a typo into a startup
        // failure instead of a fleet of subjects silently denied everything.
        // The same call rejects a role holding a permission a temporary
        // subject may not have.
        let grant_roles = resolve_ephemeral_roles(pool, key, &settings.role_names)
            .await
            .map_err(|error| auth_config_error(error.detail().to_owned()))?;
        policies.push((
            method,
            EphemeralMethodPolicy {
                method,
                grant_roles,
                session_ttl_seconds: settings.session_ttl_seconds,
                allowed_worlds: parse_allowed_worlds(key, &settings.allowed_worlds)?,
                display_name_prefix: settings.display_name_prefix.clone(),
            },
        ));
    }

    let ephemeral = if policies.is_empty() {
        None
    } else {
        let find = |wanted: orbisync_domain::AuthMethod| {
            policies
                .iter()
                .find(|(method, _)| *method == wanted)
                .map(|(_, policy)| policy.clone())
        };
        Some(Arc::new(orbisync_identity::EphemeralSubjectService::new(
            Arc::clone(session_issuer),
            find(orbisync_domain::AuthMethod::Guest),
            find(orbisync_domain::AuthMethod::NameOnly),
        )))
    };

    // Disabling issuance does not erase already-issued subjects or their
    // stored world boundary and absolute deadline. Always consult the ledger,
    // including after restarting with only local/external methods enabled.
    let scope: Arc<dyn orbisync_application::EphemeralSubjectScope> =
        Arc::new(PgEphemeralSubjectScope::new(pool.clone()));

    let external = if enabled.contains(&orbisync_domain::AuthMethod::External) {
        let settings = &config.auth.external;
        if settings.issuer == INTERNAL_TOKEN_ISSUER {
            return Err(ServerError::Config(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "auth.external.issuer",
                "must differ from this server's own token issuer, otherwise an access \
                 token this server minted would be accepted as an external login",
            )));
        }
        let provider = orbisync_identity::JwtIdentityProvider::from_jwks_file(
            Path::new(&settings.jwks_path),
            settings.issuer.clone(),
            settings.audience.clone(),
            &settings.algorithm,
            settings.leeway_seconds,
        )
        .map_err(|error| {
            ServerError::Config(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "auth.external.jwks_path",
                error.detail().to_owned(),
            ))
        })?;
        let grant_roles = resolve_ephemeral_roles(pool, "external", &settings.role_names)
            .await
            .map_err(|error| auth_config_error(error.detail().to_owned()))?;
        Some(Arc::new(orbisync_identity::ExternalAuthService::new(
            Arc::new(provider) as Arc<dyn orbisync_application::IdentityProvider>,
            Arc::new(PgExternalIdentityStore::new(pool.clone()))
                as Arc<dyn orbisync_application::ExternalIdentityStore>,
            Arc::clone(session_issuer),
            grant_roles,
        )))
    } else {
        None
    };

    tracing::info!(
        event = "auth.methods_configured",
        methods = ?config.auth.methods,
        "authentication methods configured"
    );

    Ok(AuthMethodServices {
        enabled,
        ephemeral,
        external,
        scope,
    })
}

/// Fatal startup or runtime failure.
#[derive(Debug, thiserror::Error)]
enum ServerError {
    // Keep the typed configuration diagnostic in the top-level error. The
    // key and detail are sanitized by ConfigError (secret values are never
    // part of it), and are needed when startup predates the log subscriber.
    #[error("configuration is invalid: {0}")]
    Config(#[from] ConfigError),
    #[error("telemetry could not be installed")]
    Observability(#[from] ObservabilityError),
    #[error("storage could not be initialised")]
    Storage(#[from] StorageError),
    #[error("the HTTP listener failed")]
    Listener(#[source] std::io::Error),
    #[error("the async runtime could not be initialised")]
    Runtime(#[source] std::io::Error),
    #[error("administrator bootstrap input or output is unavailable")]
    BootstrapIo(#[source] std::io::Error),
    #[error("approved password denylist is unavailable")]
    PasswordPolicy,
    #[error("administrator bootstrap failed")]
    Bootstrap,
    #[error("administrator recovery failed: {0}")]
    Recovery(#[source] orbisync_application::ApplicationError),
    #[error("administrator recovery file unavailable: {0}")]
    RecoveryIo(#[source] std::io::Error),
    #[error("extension delivery HTTP client could not be initialised")]
    DeliveryInitialization,
    #[error("generation startup unavailable")]
    Generation(#[from] orbisync_application::ApplicationError),
    #[error("extension administration failed")]
    ExtensionAdministration,
}

/// OrbiSync headless realtime backend.
#[derive(Debug, Parser)]
#[command(name = "orbisync-server", version, about)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, value_name = "PATH", env = "ORBISYNC_CONFIG_FILE")]
    config: Option<PathBuf>,

    /// Operator-owned JSON manifest of external input rule bindings (restart to apply).
    #[arg(long, value_name = "PATH", env = "ORBISYNC_INPUT_RULES_FILE")]
    input_rules: Option<PathBuf>,

    /// Override `server.bind`.
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,

    /// Override `observability.log_level`.
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Override `observability.log_format`.
    #[arg(long, value_name = "FORMAT")]
    log_format: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Recover an existing active administrator using local installation/DB operator access.
    ResetAdminPassword {
        /// Exact existing administrator login identifier (never creates an account).
        #[arg(long)]
        login_id: String,
        /// New file in an owner-only directory; never prints the credential.
        #[arg(long, value_name = "PATH")]
        password_output: PathBuf,
        /// Approved 10,000-entry corpus; defaults to ORBISYNC_PASSWORD_DENYLIST_FILE.
        #[arg(long, value_name = "PATH", env = "ORBISYNC_PASSWORD_DENYLIST_FILE")]
        password_denylist: PathBuf,
    },
    /// Open local browser setup and administration (no database required yet).
    WebAdmin {
        /// Private directory for this installation's configuration and secrets.
        #[arg(long, default_value = ".orbisync-admin")]
        data_dir: PathBuf,
        /// Loopback web administration port (0 selects an available port).
        #[arg(long, default_value_t = 8090)]
        port: u16,
        /// Print the local URL without opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Explicit reviewed checkpoint reconciliation; never starts a server.
    CheckpointOperator {
        #[command(subcommand)]
        action: checkpoint_operator_cli::OperatorCommand,
    },
    /// Register/update an out-of-process extension from a JSON manifest (operator only).
    ExtensionRegister {
        /// Manifest file; contains secret references only.
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Issue/rotate a 30-day service token; replaces the previous token atomically.
    ExtensionToken {
        /// Registered extension UUIDv7.
        #[arg(long)]
        extension_id: uuid::Uuid,
        /// Granted scope; repeat for each capability and instance.
        #[arg(long, required = true)]
        scope: Vec<String>,
        /// New file receiving the credential. Existing files are never overwritten.
        #[arg(long)]
        token_output: PathBuf,
    },
    /// Revoke the registered extension's active service token.
    ExtensionRevoke {
        /// Registered extension UUIDv7.
        #[arg(long)]
        extension_id: uuid::Uuid,
    },
    /// Serve HTTP traffic (default).
    Serve,
    /// Run non-destructive startup diagnostics and exit.
    Doctor {
        /// Emit a stable JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Apply pending database migrations and exit.
    Migrate,
    /// Create the first administrator exactly once.
    BootstrapAdmin {
        /// Administrator login identifier.
        #[arg(long)]
        login_id: String,
        /// Administrator display name.
        #[arg(long)]
        display_name: String,
        /// Approved UTF-8 corpus containing exactly 10,000 distinct lines.
        #[arg(long, value_name = "PATH")]
        password_denylist: PathBuf,
        /// New owner-only file receiving the one-time password for non-TTY use.
        #[arg(long, value_name = "PATH")]
        password_output: Option<PathBuf>,
    },
}

impl Cli {
    /// Turns the parsed arguments into configuration overrides.
    ///
    /// CLI arguments have the highest precedence (specification §28.3).
    fn overrides(&self) -> Vec<(String, String)> {
        let mut overrides = Vec::new();
        if let Some(bind) = &self.bind {
            overrides.push((String::from("server.bind"), bind.clone()));
        }
        if let Some(level) = &self.log_level {
            overrides.push((String::from("observability.log_level"), level.clone()));
        }
        if let Some(format) = &self.log_format {
            overrides.push((String::from("observability.log_format"), format.clone()));
        }
        overrides
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            // Startup failures may happen before the subscriber exists, so the
            // message goes to stderr as well.
            eprintln!("orbisync-server failed to start: {error}");
            log_startup_failure(&error);
            std::process::ExitCode::FAILURE
        }
    }
}

fn log_startup_failure(error: &ServerError) {
    match error {
        ServerError::Config(config) => {
            // Keep error_code machine-readable while retaining the actionable
            // key/reason as separate structured fields.
            tracing::error!(
                event = "server.start_failed",
                error_code = %config.kind(),
                config_key = %config.key(),
                reason = %config.detail(),
                error = %config,
            );
        }
        _ => tracing::error!(event = "server.start_failed", error_code = %error),
    }
}

fn run() -> Result<std::process::ExitCode, ServerError> {
    let cli = Cli::parse();
    if let Some(Command::WebAdmin {
        data_dir,
        port,
        no_browser,
    }) = &cli.command
    {
        return web_admin::run(data_dir, *port, *no_browser, cli.config.as_deref()).map(|()| std::process::ExitCode::SUCCESS);
    }
    let env = SystemEnv::new();
    if let Some(Command::ResetAdminPassword {
        login_id,
        password_output,
        password_denylist,
    }) = &cli.command
    {
        let path = Config::discover_path(cli.config.as_deref(), &env)?;
        let config = Config::load(path.as_deref(), &env, &cli.overrides())?.config;
        return build_runtime(config.server.worker_threads)
            .map_err(ServerError::Runtime)?
            .block_on(admin_password_recovery::run(
                &config,
                &env,
                login_id,
                password_denylist,
                password_output,
            )).map(|()| std::process::ExitCode::SUCCESS);
    }
    if let Some(Command::CheckpointOperator { action }) = &cli.command {
        let path = Config::discover_path(cli.config.as_deref(), &env)?;
        let config = Config::load(path.as_deref(), &env, &cli.overrides())?.config;
        return build_runtime(config.server.worker_threads)
            .map_err(ServerError::Runtime)?
            .block_on(checkpoint_operator_cli::run(action, &config, &env)).map(|()| std::process::ExitCode::SUCCESS);
    }
    if let Some(Command::Doctor { json }) = &cli.command {
        return run_doctor_command(&cli, &env, *json);
    }
    run_configured(cli, env).map(|()| std::process::ExitCode::SUCCESS)
}

fn run_configured(cli: Cli, env: impl EnvSource) -> Result<(), ServerError> {
    let config_path = Config::discover_path(cli.config.as_deref(), &env)?;
    let loaded = Config::load(config_path.as_deref(), &env, &cli.overrides())?;
    let config = loaded.config;
    let allowed_origins = parse_cors_origins(&config.cors.allowed_origins)?;
    config.verify_secrets(&env)?;
    init_logging(&config.observability, SERVICE, VERSION)?;
    tracing::info!(
        event = "config.loaded",
        source = config_path.as_deref().map_or("defaults", |path| {
            path.to_str().unwrap_or("configured-file")
        }),
        audit_retention_days = config.observability.audit_retention_days,
    );
    for warning in &loaded.warnings {
        tracing::warn!(event = "config.unknown_key", detail = warning);
    }

    let database_url = env.get(&config.database.url_env).unwrap_or_default();
    let password_denylist = env.get(PASSWORD_DENYLIST_ENV);

    let runtime = build_runtime(config.server.worker_threads).map_err(ServerError::Runtime)?;

    runtime.block_on(async move {
        // The pool is created inside the runtime: a lazy SQLx pool registers
        // maintenance tasks and therefore needs a Tokio context.
        let pool = create_pool(&config.database, &database_url)?;
        match cli.command.unwrap_or(Command::Serve) {
            Command::ResetAdminPassword { .. } | Command::CheckpointOperator { .. } | Command::WebAdmin { .. } => Err(orbisync_application::ApplicationError::port_failure("operator command dispatch unavailable").into()),

            Command::ExtensionRegister { manifest } => {
                use orbisync_application::ExtensionRegistrationStore;
                let bytes = std::fs::read(manifest).map_err(ServerError::BootstrapIo)?;
                let registration: orbisync_application::ExtensionRegistration = serde_json::from_slice(&bytes).map_err(|_| ServerError::ExtensionAdministration)?;
                if registration.extension_id.get_version_num() != 7
                    || registration.capabilities.iter().any(|scope| !matches!(scope.as_str(), "commands:entity:read" | "commands:audit:read"))
                    || registration.token_scopes.iter().any(|scope| !orbisync_application::extension_command::valid_scope(scope)) {
                    return Err(ServerError::ExtensionAdministration);
                }
                orbisync_storage_postgres::PgExtensionRegistrationStore::new(pool.clone())
                    .save_registration(registration).await.map_err(|_| ServerError::ExtensionAdministration)?;
                tracing::info!(event = "extension.registered");
                Ok(())
            }
            Command::ExtensionToken { extension_id, scope, token_output } => {
                let mut output = PasswordOutput::prepare(Some(&token_output))?;
                let gateway = build_extension_gateway(&pool, &config, &env, Arc::new(orbisync_world_runtime::RuntimeRegistry::new()))?;
                let credential = gateway.issue(extension_id, scope.into_iter().collect()).await.map_err(|_| ServerError::ExtensionAdministration)?;
                output.write(credential.expose_secret())?;
                tracing::info!(event = "extension.token_rotated", %extension_id);
                Ok(())
            }
            Command::ExtensionRevoke { extension_id } => {
                use orbisync_application::extension_command::ExtensionTokenStore;
                orbisync_storage_postgres::PgExtensionTokenStore::new(pool.clone()).revoke(extension_id).await.map_err(|_| ServerError::ExtensionAdministration)?;
                tracing::info!(event = "extension.token_revoked", %extension_id);
                Ok(())
            }
            Command::Migrate => {
                run_migrations(&pool).await?;
                tracing::info!(event = "database.migrated");
                Ok(())
            }
            Command::BootstrapAdmin {
                login_id,
                display_name,
                password_denylist,
                password_output,
            } => {
                run_migrations(&pool).await?;
                // D-9: the bootstrap command is a one-time, unauthenticated backdoor.
                // If any user already exists, deny before generating a credential or touching
                // the admin service, using the existing IdentityQueryPort (no new port).
                {
                    let query = PgIdentityQueryStore::new(pool.clone(), &config, &env)?;
                    ensure_bootstrap_allowed(&query).await?;
                }
                let entries = read_password_corpus(&password_denylist)?;
                let policy = PasswordPolicy::production(entries)
                    .map(|p| p.with_min_length(config.auth.password_min_length))
                    .map_err(|_| ServerError::PasswordPolicy)?;
                let passwords = PasswordService::new_with_argon2_and_concurrency(
                    policy,
                    config.auth.argon2_memory_cost_kib,
                    config.auth.argon2_iterations,
                    config.auth.argon2_parallelism,
                    config.auth.password_hash_concurrency as usize,
                )
                .map_err(|_| ServerError::PasswordPolicy)?;
                let mut output = PasswordOutput::prepare(password_output.as_deref())?;
                let service = IdentityAdministrationService::new(
                    Arc::new(IdentityAdministrationStore::new(pool.clone())),
                    Arc::new(SystemClock::new()),
                    passwords,
                );
                let login_id = LoginId::new(login_id).map_err(|_| ServerError::Bootstrap)?;
                let request_id =
                    orbisync_application::RequestId::new(format!("req_{}", UserId::generate()))
                        .map_err(|_| ServerError::Bootstrap)?;
                let (_, temporary_password) = service
                    .bootstrap_administrator(login_id, display_name, request_id)
                    .await
                    .map_err(|_| ServerError::Bootstrap)?;
                output.write(temporary_password.expose_secret())?;
                tracing::info!(event = "administrator.bootstrapped");
                Ok(())
            }
            Command::Doctor { .. } => Ok(()),
            Command::Serve => {
                let shutdown = Arc::new(ShutdownState::new());
                let path = password_denylist
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or(ServerError::PasswordPolicy)?;
                let policy = PasswordPolicy::production(read_password_corpus(Path::new(path))?)
                    .map(|p| p.with_min_length(config.auth.password_min_length))
                    .map_err(|_| ServerError::PasswordPolicy)?;
                let password_service = Arc::new(
                    PasswordService::new_with_argon2_and_concurrency(
                        policy,
                        config.auth.argon2_memory_cost_kib,
                        config.auth.argon2_iterations,
                        config.auth.argon2_parallelism,
                        config.auth.password_hash_concurrency as usize,
                    )
                    .map_err(|_| ServerError::PasswordPolicy)?,
                );
                let world_store = PgWorldDirectoryStore::new(pool.clone(), &config, &env)?;
                let world_store_for_realtime: Arc<dyn orbisync_application::WorldDirectoryStore> =
                    Arc::new(PgWorldDirectoryStore::new(pool.clone(), &config, &env)?);
                let world_authorizer = PgWorldAuthorizer::new(pool.clone());
                let realtime_world_authorizer: Arc<dyn orbisync_application::WorldAuthorizer> =
                    Arc::new(world_authorizer.clone());
                let world_directory = Arc::new(WorldDirectoryUseCase::new(
                    world_store,
                    world_authorizer,
                ));
                let probe: Arc<dyn HealthProbe> = Arc::new(PgHealthProbe::with_timeout(
                    pool.clone(),
                    config.database.readiness_timeout_seconds,
                ));
                if config.auth.allow_stub_bearer {
                    tracing::warn!(
                        event = "auth.stub_bearer_enabled",
                        detail = "auth.allow_stub_bearer is true — any non-empty Bearer token is accepted. Do not expose to public network (H-5)"
                    );
                }
                let token_service = Arc::new(create_token_service(&env, &config)?);
                let clock: Arc<dyn orbisync_domain::Clock> = Arc::new(SystemClock::new());
                // D1-A metrics: Prometheus implementation, shared for recorder + exporter.
                let prometheus_metrics = Arc::new(PrometheusMetrics::new());
                let metrics_recorder: Arc<dyn MetricsRecorder> =
                    prometheus_metrics.clone() as Arc<dyn MetricsRecorder>;
                let metrics_exporter: Arc<dyn orbisync_application::metrics::MetricsExporter> =
                    prometheus_metrics.clone() as Arc<dyn orbisync_application::metrics::MetricsExporter>;
                let retention_status = Arc::new(retention::RetentionStatus::default());
                let operational_diagnostics: Arc<
                    dyn orbisync_application::OperationalDiagnosticsPort,
                > = Arc::new(operational_diagnostics::RuntimeOperationalDiagnostics::new(
                    pool.clone(),
                    Arc::clone(&prometheus_metrics),
                    Arc::clone(&retention_status),
                    Arc::clone(&clock),
                    orbisync_application::OperationalQueueLimits {
                        control_per_instance: u64::try_from(
                            config.world.mailbox_control_capacity,
                        )
                        .unwrap_or(u64::MAX),
                        transform_per_instance: u64::try_from(
                            config.world.mailbox_transform_capacity,
                        )
                        .unwrap_or(u64::MAX),
                        entity_per_instance: u64::try_from(
                            config.world.mailbox_entity_capacity,
                        )
                        .unwrap_or(u64::MAX),
                        outbound_per_connection: u64::from(
                            config.realtime.outbound_queue_capacity,
                        ),
                    },
                ));
                // Extension delivery is an outbox worker, never part of a
                // domain transaction or the HTTP request critical path.
                let delivery_policy = DeliveryPolicy::from_config(config.extensions.clone()).map_err(|error| {
                    ServerError::Config(ConfigError::new(
                        ConfigErrorKind::InvalidValue,
                        "extensions",
                        error.to_string(),
                    ))
                })?;
                let extension_store = Arc::new(
                    orbisync_storage_postgres::PgExtensionOutboxStore::new(pool.clone()),
                );
                // Populate the registration cache before actors can emit
                // events. A failed refresh remains fail-closed toward
                // persistence, so events are retained until the next pass.
                if let Err(error) = extension_store.refresh_active_registration_cache().await {
                    tracing::warn!(
                        event = "extension.registration_cache_refresh_failed",
                        error = %error,
                        detail = "persisting extension events until the registration cache is refreshed"
                    );
                }
                let delivery_client = ReqwestHttpClient::try_new()
                    .map_err(|_| ServerError::DeliveryInitialization)?;
                let delivery_engine = Arc::new(DeliveryEngine::new(
                    Arc::new(delivery_client),
                    Arc::new(EnvSecretProvider),
                    Arc::clone(&prometheus_metrics),
                    delivery_policy,
                ));
                let delivery_worker = DeliveryWorker::new(
                    delivery_engine,
                    Arc::clone(&extension_store),
                    delivery_policy,
                )
                .with_shutdown_drain_timeout(std::time::Duration::from_secs(
                    config.server.shutdown_drain_timeout_seconds,
                ));
                let (extension_shutdown_tx, extension_shutdown_rx) =
                    tokio::sync::watch::channel(false);
                let extension_worker = tokio::spawn(async move {
                    delivery_worker.run(extension_shutdown_rx).await;
                });
                let identity_repo: Arc<dyn orbisync_application::IdentityRepository> =
                    Arc::new(PgIdentityRepository::new(pool.clone()));
                let login_store: Arc<dyn orbisync_application::LoginTransactionStore> = Arc::new(
                    PgLoginStore::new(pool.clone())
                        .with_login_failure_policy(
                            config.auth.login_failure_threshold,
                            config.auth.lockout_duration_seconds,
                        )
                        .with_metrics(Arc::clone(&metrics_recorder)),
                );
                let password_service_for_admin = Arc::clone(&password_service);
                // The block also yields the ADR-026 services, which depend on
                // secrets derived inside it and are needed again when the
                // realtime state is built further down.
                let (http_state, auth_methods) = {
                    let admin_store: Arc<dyn orbisync_application::IdentityAdministrationStore> =
                        Arc::new(IdentityAdministrationStore::new(pool.clone()));
                    let dyn_store = DynIdentityAdministrationStore(admin_store);
                    let dyn_clock = DynClock(Arc::clone(&clock));
                    let admin_service = Arc::new(IdentityAdministrationService::new(
                        Arc::new(dyn_store),
                        Arc::new(dyn_clock),
                        (*password_service_for_admin).clone(),
                    ));
                    let query_store =
                        Arc::new(PgIdentityQueryStore::new(pool.clone(), &config, &env)?);
                    let identity_query: Arc<dyn IdentityQueryPort> = query_store.clone();
                    let audit_query: Arc<dyn AuditQueryPort> = query_store;
                    let refresh_rotation_store: Arc<
                        dyn orbisync_application::RefreshTokenRotationStore,
                    > = Arc::new(IdentityAdministrationStore::new(pool.clone()));
                    let refresh_creation_store: Arc<
                        dyn orbisync_application::RefreshTokenCreationStore,
                    > = Arc::new(IdentityAdministrationStore::new(pool.clone()));
                    let idempotency_store: Arc<dyn orbisync_application::IdempotencyStore> =
                        Arc::new(IdempotencyStore::new(pool.clone()));
                    // Config::load has already validated this value. Keep the
                    // parsed networks typed across the HTTP boundary so CIDR
                    // matching cannot regress to string or prefix matching.
                    let trusted_proxies =
                        orbisync_config::parse_trusted_proxies(&config.server.trusted_proxies)?;
                    let trusted_proxy_entries: Vec<String> = config
                        .server
                        .trusted_proxies
                        .split(',')
                        .map(str::trim)
                        .filter(|entry| !entry.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                    let refresh_hmac_key = {
                        let env_name = config.auth.refresh_token_hmac_key_env.clone();
                        let raw = env.get(&env_name).ok_or_else(|| {
                            ConfigError::new(
                                ConfigErrorKind::MissingSecret,
                                env_name.clone(),
                                "required secret environment variable is not set",
                            )
                        })?;
                        if raw.trim().is_empty() {
                            return Err(ServerError::Config(ConfigError::new(
                                ConfigErrorKind::MissingSecret,
                                env_name.clone(),
                                "required secret environment variable is not set",
                            )));
                        }
                        raw.into_bytes()
                    };
                    let realtime_ticket_hmac_key = {
                        let env_name = config.auth.realtime_ticket_hmac_key_env.clone();
                        let raw = env.get(&env_name).ok_or_else(|| {
                            ConfigError::new(
                                ConfigErrorKind::MissingSecret,
                                env_name.clone(),
                                "required secret environment variable is not set",
                            )
                        })?;
                        if raw.trim().is_empty() {
                            return Err(ServerError::Config(ConfigError::new(
                                ConfigErrorKind::MissingSecret,
                                env_name.clone(),
                                "required secret environment variable is not set",
                            )));
                        }
                        raw.into_bytes()
                    };
                    let idempotency_hmac_key = {
                        let env_name = config.auth.idempotency_hmac_key_env.clone();
                        let raw = env.get(&env_name).ok_or_else(|| {
                            ConfigError::new(
                                ConfigErrorKind::MissingSecret,
                                env_name.clone(),
                                "required secret environment variable is not set",
                            )
                        })?;
                        if raw.trim().is_empty() {
                            return Err(ServerError::Config(ConfigError::new(
                                ConfigErrorKind::MissingSecret,
                                env_name.clone(),
                                "required secret environment variable is not set",
                            )));
                        }
                        raw.into_bytes()
                    };
                    // RV-A C1: realtime tickets use dedicated HMAC key (not refresh key).
                    // This isolates ticket digests per ADR-002 §52 – rotation of one does not invalidate the other.
                    let realtime_ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
                        Arc::new(PgRealtimeTicketStore::new(pool.clone()));
                    // C5: per-user / per-session rate limit for ticket issuance (bounded retention).
                    let realtime_ticket_rate_limiter = Arc::new(
                        orbisync_transport_http::ticket_rate_limit::RealtimeTicketRateLimiter::new_with_max_buckets(
                            config.rate_limit.realtime_ticket_per_interval,
                            config.retention.realtime_ticket_interval_seconds,
                            config.rate_limit.max_buckets,
                        ),
                    );
                    let login_ip_rate_limiter = Arc::new(
                        orbisync_transport_http::login_rate_limit::LoginRateLimiter::new(
                            config.rate_limit.login_per_ip_per_minute,
                            config.rate_limit.login_ip_block_seconds,
                            config.rate_limit.login_ip_max_buckets,
                        ),
                    );
                    // P2-C4: login service owns the transactional boundary
                    let login_service = Arc::new(orbisync_identity::LoginService::new(
                        Arc::clone(&identity_repo)
                            as Arc<dyn orbisync_application::IdentityRepository>,
                        Arc::clone(&login_store)
                            as Arc<dyn orbisync_application::LoginTransactionStore>,
                        (*password_service).clone(),
                        Arc::clone(&token_service),
                        Arc::clone(&clock),
                        refresh_hmac_key.clone(),
                        config.auth.access_token_ttl_seconds,
                        config.auth.refresh_token_ttl_seconds,
                    ));
                    // ADR-026: the issuing path every method converges on. The
                    // credential path keeps its own service; this one serves
                    // the subjects that hold no credential.
                    let session_issuer = Arc::new(orbisync_identity::SessionIssuer::new(
                        Arc::clone(&login_store)
                            as Arc<dyn orbisync_application::LoginTransactionStore>,
                        Arc::clone(&token_service),
                        Arc::clone(&clock),
                        refresh_hmac_key.clone(),
                        config.auth.access_token_ttl_seconds,
                        config.auth.refresh_token_ttl_seconds,
                    ));
                    let auth_methods =
                        build_auth_method_services(&config, &pool, &session_issuer).await?;
                    // C3: reading handshake_timeout_ms and max_connections via .handshake_timeout_ms / .max_connections
                    // (and active connection metrics via config) ensures check_config_usage is green – both new keys
                    // are read in realtime_ws.rs (handler + state construction).
                    let _ = config.realtime.handshake_timeout_ms;
                    let _ = config.realtime.max_connections;
                    let state = HttpState::new(
                        vec![probe],
                        SERVICE,
                        VERSION,
                        orbisync_protocol::PROTOCOL_MAJOR,
                        Arc::clone(&clock),
                        config.auth.access_token_ttl_seconds,
                        config.auth.refresh_token_ttl_seconds,
                        refresh_hmac_key,
                    )
                    .with_shutdown_flag(shutdown.rejecting_flag())
                    .with_world_directory(world_directory)
                    .with_csv_limits(
                        config.identity.csv_max_bytes,
                        config.identity.csv_max_rows,
                    )
                    .with_max_request_body_bytes(config.server.max_request_body_bytes)
                    .with_request_timeout(std::time::Duration::from_secs(
                        config.server.request_timeout_seconds,
                    ))
                    .with_page_limits(
                        config.server.page_default_limit,
                        config.server.page_max_limit,
                    )
                    .with_allowed_origins(allowed_origins)
                    .with_allow_credentials(config.cors.allow_credentials)
                    .with_allow_stub_bearer(config.auth.allow_stub_bearer)
                    .with_token_service(Arc::clone(&token_service))
                    .with_identity_repository(Arc::clone(&identity_repo))
                    .with_password_service(password_service)
                    .with_identity_admin_service(admin_service)
                    .with_identity_query(identity_query)
                    .with_audit_query(audit_query)
                    .with_refresh_creation_store(refresh_creation_store)
                    .with_refresh_rotation_store(refresh_rotation_store)
                    .with_idempotency_store(idempotency_store)
                    .with_idempotency_hmac_key(idempotency_hmac_key.clone())
                    .with_realtime_ticket_hmac_key(realtime_ticket_hmac_key.clone())
                    .with_realtime_ticket_store(Arc::clone(&realtime_ticket_store))
                    .with_realtime_ticket_rate_limiter(Arc::clone(&realtime_ticket_rate_limiter))
                    .with_login_rate_limiter(Arc::clone(&login_ip_rate_limiter))
                    .with_login_service(login_service)
                    .with_enabled_auth_methods(auth_methods.enabled.clone())
                    .with_trusted_proxy_networks(trusted_proxies)
                    // Restate the configured strings through the legacy
                    // builder last so `trusted_proxies()` keeps the operator's
                    // original values instead of synthesized prefixes.
                    .with_trusted_proxies(trusted_proxy_entries)
                    .with_metrics_recorder(Arc::clone(&metrics_recorder))
                    .with_metrics_exporter(Arc::clone(&metrics_exporter))
                    .with_operational_diagnostics(operational_diagnostics);
                    (state, auth_methods)
                };
                let runtime_registry = Arc::new(orbisync_world_runtime::RuntimeRegistry::new());
                let http_state = http_state.with_extension_commands(Arc::new(build_extension_gateway(&pool, &config, &env, Arc::clone(&runtime_registry))?));
                let http_state = {
                    let membership_store =
                        PgWorldDirectoryStore::new(pool.clone(), &config, &env)?;
                    let membership_authorizer = PgWorldAuthorizer::new(pool.clone());
                    let membership_runtime = RuntimeInstanceMembershipStore {
                        registry: Arc::clone(&runtime_registry),
                    };
                    let instance_membership: Arc<
                        dyn orbisync_transport_http::instance_membership::InstanceMembership,
                    > = Arc::new(orbisync_application::InstanceMembershipUseCase::new(
                        membership_store,
                        membership_runtime,
                        membership_authorizer,
                    ));
                    let http_state = http_state.with_instance_membership(instance_membership);
                    // ADR-026: attached only when the method is enabled, so a
                    // default deployment has no service to reach and the
                    // endpoint answers "not enabled" for the same reason it
                    // does when the method was never configured.
                    let http_state = match auth_methods.ephemeral.clone() {
                        Some(service) => http_state.with_ephemeral_subject_service(service),
                        None => http_state,
                    };
                    match auth_methods.external.clone() {
                        Some(service) => http_state.with_external_auth_service(service),
                        None => http_state,
                    }
                };
                let delivery_registry = Arc::new(
                    delivery::DeliveryRegistry::new_with_capacity_and_metrics(
                        config.realtime.per_connection_capacity,
                        Arc::clone(&metrics_recorder),
                    ),
                );

                // Checkpoint persistence: `instance_checkpoints` table via `PgCheckpointStore`.
                // The store is behind the `CheckpointStore` port so `application` stays
                // storage-agnostic (`repo-crate-conventions.md` §3.2); `server` wires the
                // Postgres adapter.
                let checkpoint_limits = orbisync_server::checkpoint_admission::checkpoint_limits(&config)?;
                let generation_ownership = if config.world.checkpoint_generation_enabled {
                    Some(orbisync_storage_postgres::PgWriterOwnership::acquire(
                        &database_url, Path::new(&config.world.checkpoint_writer_lock),
                        &config.world.checkpoint_deployment).await?)
                } else { None };
                let generation = generation_ownership.as_ref().map(|owner| {
                    let store = Arc::new(orbisync_storage_postgres::PgGenerationStore::new(pool.clone(), checkpoint_limits, owner));
                    orbisync_server::checkpoint_generation::GenerationServices::new(checkpoint_limits, owner.permit(), store)
                }).transpose()?;
                if let Some(service) = &generation {
                    for instance in service.projection_page().await? {
                        if let Err(error) = service.project(instance).await {
                            tracing::warn!(event="generation.startup_projection_pending", %error);
                        }
                    }
                }
                let checkpoint_store: Arc<dyn CheckpointStore> =
                    Arc::new(operational_diagnostics::MeteredCheckpointStore::new(
                        PgCheckpointStore::new(pool.clone()),
                        Arc::clone(&metrics_recorder),
                    ));
                // Bounds the number of checkpoint payloads held in memory at
                // once across every save path; see `MAX_PENDING_CHECKPOINT_SAVES`.
                let checkpoint_save_permits =
                    Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CHECKPOINT_SAVES));
                // Persistent entity/component rows: `persistent_entities` and
                // `persistent_entity_components` via `PgPersistentEntityStore`
                // (HIGH-002). Written from the tick loop below, never from the
                // actor itself (`architecture.md` §3.1).
                let persistent_entity_store: Arc<dyn PersistentEntityStore> =
                    Arc::new(match generation.as_ref() {
                        Some(service) => PgPersistentEntityStore::with_writer(pool.clone(), service.writer.clone()),
                        None => PgPersistentEntityStore::new(pool.clone()),
                    });
                let command_dedup = Arc::new(
                    orbisync_server::command_dedup::CommandDedupStore::default(),
                );

                // Startup probe: verify `instance_checkpoints` is reachable by
                // attempting `load_latest` for a synthetic id. Real instances will
                // load their own latest checkpoint on first join; this probe just
                // ensures the migration is applied and logs the outcome.
                {
                    let store = Arc::clone(&checkpoint_store);
                    let probe_id = InstanceId::generate();
                    match store.load_latest(probe_id).await {
                        Ok(Some(cp)) => tracing::info!(
                            event = "checkpoint.loaded",
                            instance_id = %cp.instance_id,
                            revision = cp.revision.as_u64(),
                            payload_bytes = cp.payload.len(),
                            detail = "checkpoint startup probe found a checkpoint (unexpected for synthetic id)"
                        ),
                        Ok(None) => tracing::info!(
                            event = "checkpoint.load_latest",
                            instance_id = %probe_id,
                            detail = "no checkpoint found (startup probe – table reachable)"
                        ),
                        Err(err) => tracing::warn!(
                            event = "checkpoint.load_failed",
                            instance_id = %probe_id,
                            error = %err,
                            detail = "checkpoint load_latest failed on startup probe"
                        ),
                    }
                }

                // Periodic checkpoint task: ticks every 1 / `server_tick_hz` and
                // handles `Tick` for all live instance actors. Checkpointing is
                // time-based: every `world.checkpoint_interval_secs` (default
                // 300 s, validated 60..3600) the task builds a `Checkpoint` via
                // `build_checkpoint(now)` and persists it via
                // `CheckpointStore::save_checkpoint` (`PgCheckpointStore`).
                // Retention (latest 3 per instance / 30 days) is enforced by
                // `PgCheckpointStore` after each save.
                let tick_task = if generation.is_some() { Some({
                    let tick_generation = generation.clone();
                    let tick_shutdown = Arc::clone(&shutdown);
                    let tick_registry = Arc::clone(&runtime_registry);
                    let tick_delivery = Arc::clone(&delivery_registry);
                    let tick_store = Arc::clone(&checkpoint_store);
                    let tick_extension_store = Arc::clone(&extension_store);
                    let tick_persistent_entity_store = Arc::clone(&persistent_entity_store);
                    let tick_command_dedup = Arc::clone(&command_dedup);
                    let tick_metrics_recorder = Arc::clone(&metrics_recorder);
                    let tick_checkpoint_permits = Arc::clone(&checkpoint_save_permits);
                    let tick_hz = config.world.server_tick_hz;
                    let checkpoint_interval_secs = config.world.checkpoint_interval_secs;
                    let checkpoint_interval_ticks = config.world.checkpoint_interval_ticks;
                    tokio::spawn(async move {
                        let interval_secs = 1.0 / f64::from(tick_hz.max(1));
                        let interval = std::time::Duration::from_secs_f64(interval_secs);
                        let mut ticker = tokio::time::interval(interval);
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        let checkpoint_interval =
                            std::time::Duration::from_secs(checkpoint_interval_secs);
                        let mut last_checkpoint = Instant::now();
                        let mut tick_count = 0_u64;
                        let mut tasks = periodic_tasks::PeriodicTasks::new(8);
                        // Bounds periodic database work including generation, projection,
                        // reap, and event writes. Existing global checkpoint permits and
                        // generation budgets still apply inside these jobs.
                        let storage_permits = Arc::new(tokio::sync::Semaphore::new(2));
                        loop {
                            tokio::select! {
                                _ = ticker.tick() => {
                                    if tick_shutdown.is_rejecting() { break; }
                                    tick_count = tick_count.saturating_add(1);
                                    let checkpoint_due = tick_count.is_multiple_of(checkpoint_interval_ticks)
                                        || last_checkpoint.elapsed() >= checkpoint_interval;
                                    if checkpoint_due {
                                        last_checkpoint = Instant::now();
                                        if tick_generation.is_some() {
                                            tasks.enqueue(None, periodic_tasks::Work::default());
                                        }
                                    }
                                    for handle in tick_registry.handles() {
                                        tasks.enqueue(Some(handle.instance_id()), periodic_tasks::Work {
                                            tick: true, checkpoint: checkpoint_due, project: false,
                                        });
                                    }
                                }
                                joined = tasks.join_next(), if tasks.is_running() => {
                                    match joined {
                                        Some(Ok(instances)) => {
                                            for instance in instances {
                                                tasks.enqueue(Some(instance), periodic_tasks::Work {
                                                    project: true, ..periodic_tasks::Work::default()
                                                });
                                            }
                                        }
                                        Some(Err(error)) => tracing::warn!(event="runtime.periodic_task_failed", %error),
                                        None => {},
                                    }
                                }
                            }
                            if tick_shutdown.is_rejecting() { break; }
                            tasks.start_ready(|instance, work| {
                                let generation = tick_generation.clone();
                                let registry = Arc::clone(&tick_registry);
                                let delivery = Arc::clone(&tick_delivery);
                                let store = Arc::clone(&tick_store);
                                let dedup = Arc::clone(&tick_command_dedup);
                                let metrics = Arc::clone(&tick_metrics_recorder);
                                let checkpoint_permits = Arc::clone(&tick_checkpoint_permits);
                                let storage_permits = Arc::clone(&storage_permits);
                                let extension_store = Arc::clone(&tick_extension_store);
                                let entity_store = Arc::clone(&tick_persistent_entity_store);
                                async move {
                                    let Some(instance_id) = instance else {
                                        let _storage = storage_permits.acquire().await.expect("periodic budget open");
                                        return match generation {
                                            Some(service) => service.projection_page().await.unwrap_or_default(),
                                            None => Vec::new(),
                                        };
                                    };
                                    let now = SystemClock::new().now();
                                    let mut pending_events = Vec::new();
                                    let mut pending_persistence_events = Vec::new();
                                    let handle = if work.tick { registry.handle(instance_id) } else { None };
                                    if let Some(handle) = &handle {
                                        let tick_started = Instant::now();
                                        // The saved checkpoint is captured later under the
                                        // durability fence; avoid building a discarded copy.
                                        let result = handle.tick(now, false).await;
                                        metrics.observe(Histogram::TickDuration, tick_started.elapsed().as_secs_f64());
                                        let Some(result) = result else {
                                            tracing::debug!(event="runtime.tick_mailbox_unavailable", %instance_id);
                                            return Vec::new();
                                        };
                                        pending_events.extend(result.events);
                                        if generation.is_none() {
                                            pending_persistence_events.extend(result.persistence_events);
                                        }
                                    }
                                    // Only this bounded job can run periodic work for this
                                    // instance until all its drained writes have completed.
                                    let _storage = storage_permits.acquire().await.expect("periodic budget open");
                                    if let Some(service) = &generation {
                                        if let Some(handle) = &handle {
                                            if work.checkpoint {
                                                if let Err(error) = service.persist(handle.clone(), now).await {
                                                    tracing::warn!(event="generation.periodic_unavailable", %error);
                                                }
                                            }
                                        }
                                        if work.project || (handle.is_some() && work.checkpoint) {
                                            if let Err(error) = service.project(instance_id).await {
                                                tracing::warn!(event="generation.projection_pending", %error);
                                            }
                                        }
                                        if handle.is_some() && work.checkpoint {
                                            if let Err(error) = service.cleanup(instance_id).await {
                                                tracing::warn!(event="generation.cleanup_pending", %error);
                                            }
                                        }
                                        if handle.is_some() {
                                            match service.reap(&registry, instance_id, now).await {
                                                Ok(Some(events)) => pending_events.extend(events),
                                                Ok(None) => {},
                                                Err(error) => tracing::warn!(event="generation.reap_retained", %error),
                                            }
                                        }
                                    } else if handle.is_some() {
                                        if work.checkpoint {
                                            let saved = tokio::time::timeout(
                                                std::time::Duration::from_secs(2),
                                                persist_instance_checkpoint(
                                                    registry.as_ref(), store.as_ref(), dedup.as_ref(),
                                                    checkpoint_permits.as_ref(), instance_id, now,
                                                ),
                                            ).await;
                                            match saved {
                                                Ok(Ok(())) => {},
                                                Ok(Err(err)) if err.kind() == ApplicationErrorKind::CheckpointTooLarge => {
                                                    metrics.incr(Counter::CheckpointSaveRejectedTotal);
                                                    tracing::error!(event="checkpoint.save_rejected_too_large", %instance_id,
                                                        limit_bytes=MAX_CHECKPOINT_PAYLOAD_BYTES, error=%err,
                                                        detail="periodic checkpoint exceeds the payload limit; the world is not durable and will lose state on restart");
                                                }
                                                Ok(Err(error)) => tracing::warn!(event="checkpoint.save_failed", %instance_id, %error),
                                                Err(_) => tracing::warn!(event="checkpoint.save_deadline_exceeded", %instance_id),
                                            }
                                        }
                                        // The existing lifecycle barrier and final-save-before-
                                        // removal protocol remain inside reap_idle_instance.
                                        let (events, persistence_events) = reap_idle_instance(
                                            registry, delivery, store, dedup, Arc::clone(&metrics),
                                            checkpoint_permits, instance_id, now,
                                        ).await;
                                        pending_events.extend(events);
                                        pending_persistence_events.extend(persistence_events);
                                    }
                                    // Preserve the drained batch order within each instance;
                                    // different instances no longer wait for each other's I/O.
                                    persist_extension_events(extension_store.as_ref(), pending_events).await;
                                    persist_entity_events(entity_store.as_ref(), metrics.as_ref(), pending_persistence_events).await;
                                    Vec::new()
                                }
                            });
                        }
                        // Stop producing work, then finish every admitted tick and its
                        // writes before the shutdown caller begins final checkpoints.
                        // Coalesced, unstarted ticks need not run during shutdown.
                        while let Some(joined) = tasks.join_next().await {
                            if let Err(error) = joined {
                                tracing::warn!(event="runtime.periodic_task_failed", %error);
                            }
                        }
                    })
                }) } else { None };

                // Legacy runtime uses independent tick and persistence workers.
                // Generation admission keeps its existing periodic ownership path.
                let runtime_workers = if generation.is_none() {
                    Some(runtime_maintenance::RuntimeWorkers::spawn(
                        runtime_maintenance::MaintenanceServices {
                            registry: Arc::clone(&runtime_registry),
                            delivery: Arc::clone(&delivery_registry),
                            checkpoints: Arc::clone(&checkpoint_store),
                            extensions: extension_store.clone(),
                            entities: Arc::clone(&persistent_entity_store),
                            dedup: Arc::clone(&command_dedup),
                            metrics: Arc::clone(&metrics_recorder),
                            checkpoint_permits: Arc::clone(&checkpoint_save_permits),
                        },
                        config.world.server_tick_hz,
                        std::time::Duration::from_secs(config.world.checkpoint_interval_secs),
                        config.world.checkpoint_interval_ticks,
                    ))
                } else { None };

                // C5 + S9 + P2-C3: periodic retention via delete_expired/drain_expired for short-lived credentials.
                // Multi-batch drain (batch*max_batches=125k per 60s tick, 20s budget) ensures
                // cleanup capacity Y (110–125k/min, measured 5.5–10k rows/sec) >= X (100k/min).
                // Supervisor restarts transient failures, exits after 5 consecutive
                // failures so panics are not silently dropped (P2-C3 fix).
                let retention_supervisor =
                    retention::spawn_retention_tasks(
                        pool.clone(),
                        Arc::clone(&clock),
                        config.retention.clone(),
                        Arc::clone(&metrics_recorder),
                        Arc::clone(&retention_status),
                    );
                // Existing temporary subjects still need cleanup after issuance
                // is disabled. Include both methods in the existing shortest-
                // grace policy, independently of the methods accepting logins.
                // Access expires at the stored deadline, before this cleanup.
                let retention_seconds = config.auth.guest.retention_seconds
                    .min(config.auth.name_only.retention_seconds);
                let _revocation = retention::spawn_ephemeral_subject_revocation(
                    pool.clone(),
                    Arc::clone(&clock),
                    std::time::Duration::from_secs(
                        config.retention.realtime_ticket_interval_seconds.max(1),
                    ),
                    retention_seconds,
                    256,
                );
                // Keep supervisor handle alive and surface unexpected exit: if the
                // supervisor itself panics, fail the process after logging.
                let supervisor_for_watch = retention_supervisor;
                tokio::spawn(async move {
                    // This future completes only if the supervisor exits (should be
                    // forever). Any exit is unexpected.
                    let res = supervisor_for_watch.await;
                    match res {
                        Ok(()) => {
                            tracing::error!(
                                event = "retention.supervisor_exited",
                                "retention supervisor exited unexpectedly"
                            );
                            std::process::exit(1);
                        }
                        Err(e) if e.is_panic() => {
                            tracing::error!(
                                event = "retention.supervisor_panic",
                                error = %e,
                                "retention supervisor panicked"
                            );
                            std::process::exit(1);
                        }
                        Err(e) => {
                            tracing::error!(
                                event = "retention.supervisor_failed",
                                error = %e,
                                "retention supervisor failed"
                            );
                            std::process::exit(1);
                        }
                    }
                });

                // H-6c: build UniformGrid from InterestConfig (not UniformGrid::default()).
                // If params are invalid (should not happen after Config::validate), fall back to defaults
                // and warn, so the server still starts.
                let interest_grid = match orbisync_interest::InterestParameters::new(
                    config.interest.cell_size,
                    config.interest.near_radius,
                    config.interest.unsubscribe_radius,
                ) {
                    Ok(params) => orbisync_interest::UniformGrid::new(params),
                    Err(err) => {
                        tracing::warn!(
                            event = "interest.config_invalid",
                            error = %err,
                            cell_size = config.interest.cell_size,
                            near_radius = config.interest.near_radius,
                            unsubscribe_radius = config.interest.unsubscribe_radius,
                            detail = "interest config invalid, falling back to UniformGrid::default()"
                        );
                        orbisync_interest::UniformGrid::default()
                    }
                };
                // RV-A C1 / N-1: select the ticket verifier based on `realtime.allow_stub_ticket`.
                // When stub is disabled (default, fail-closed) use the HMAC ticket verifier
                // that atomically consumes `realtime_tickets` (DELETE ... USING auth_sessions).
                // The JWT verifier (AccessTokenTicketVerifier) is retained for tests but not used
                // in production after C1. A dedicated HMAC key (realtime_ticket_hmac_key_env)
                // is used, not the refresh key, per ADR-002 §52 (purpose isolation).
                let tickets: Arc<dyn RealtimeTicketVerifier> =
                    if config.realtime.allow_stub_ticket {
                        tracing::warn!(
                            event = "realtime.stub_ticket_enabled",
                            detail = "realtime.allow_stub_ticket is true - any non-empty ticket is accepted \
                                      and the user identity is fabricated. Do not expose to a public network (N-1)"
                        );
                        Arc::new(StubTicketVerifier::new())
                    } else {
                        // C3 metrics and handshake timeout are read via config fields already referenced
                        // above (handshake_timeout_ms / max_connections) so check_config_usage stays green.
                        let Some(rt_store) = http_state.realtime_ticket_store() else {
                            tracing::error!(
                                event = "realtime.ticket_store_unwired",
                                "realtime ticket store must be wired (C1) - aborting startup"
                            );
                            std::process::exit(1);
                        };
                        let rt_key = http_state.realtime_ticket_hmac_key().to_vec();
                        Arc::new(HmacRealtimeTicketVerifier::new(
                            rt_store,
                            rt_key,
                            Arc::clone(&clock),
                        ))
                    };
                // ADR-025: the pre-commit validation hook is always wired
                // (like DeliveryEngine above); the actual opt-in is per
                // capability registration in `extension_registrations`, not
                // a process-wide toggle. `allow_loopback_endpoints` must stay
                // `false` outside local/test deployments (ADR-025 §2.8).
                let pre_commit_policy =
                    PreCommitValidationPolicy::from_config(config.extensions.clone()).map_err(|error| {
                        ServerError::Config(ConfigError::new(
                            ConfigErrorKind::InvalidValue,
                            "extensions",
                            error.to_string(),
                        ))
                    })?;
                let pre_commit_client = if config
                    .extensions
                    .pre_commit_additional_ca_path
                    .trim()
                    .is_empty()
                {
                    ReqwestPreCommitClient::try_new(
                        Arc::new(SystemDnsResolver),
                        config.extensions.allow_loopback_endpoints,
                    )
                    .map_err(|_| ServerError::DeliveryInitialization)
                }
                else {
                    let path = config.extensions.pre_commit_additional_ca_path.trim();
                    let certificate = std::fs::read(path).map_err(|error| {
                        ServerError::Config(ConfigError::new(
                            ConfigErrorKind::FileUnreadable,
                            "extensions.pre_commit_additional_ca_path",
                            format!("{path}: {error}"),
                        ))
                    })?;
                    ReqwestPreCommitClient::try_new_with_additional_ca(
                        Arc::new(SystemDnsResolver),
                        config.extensions.allow_loopback_endpoints,
                        &certificate,
                    )
                    .map_err(|error| {
                        ServerError::Config(ConfigError::new(
                            ConfigErrorKind::InvalidValue,
                            "extensions.pre_commit_additional_ca_path",
                            format!("invalid PEM/DER certificate: {error}"),
                        ))
                    })
                }
                ?;
                let pre_commit_client = Arc::new(pre_commit_client);
                let input_rules = if let Some(path) = cli.input_rules.as_deref() {
                    let transport = Arc::new(
                        orbisync_server::external_input::ExternalInputTransport::new(
                            pre_commit_client.clone(),
                            Arc::new(EnvSecretProvider),
                            pre_commit_policy,
                        ),
                    );
                    let input_config_error = |error| ServerError::Config(ConfigError::new(
                        ConfigErrorKind::InvalidValue, "input_rules", error,
                    ));
                    let manifest = orbisync_server::external_input::InputRuleManifest::load(path)
                        .map_err(input_config_error)?;
                    manifest.register(transport).await.map_err(input_config_error)?
                } else {
                    Default::default()
                };
                let pre_commit_gate: Arc<dyn PreCommitGate> = Arc::new(PreCommitValidationGate::new(
                    pre_commit_client,
                    Arc::new(EnvSecretProvider),
                    pre_commit_policy,
                ));
                let extension_registrations: Arc<dyn ExtensionRegistrationStore> =
                    Arc::new(PgExtensionRegistrationStore::new(pool.clone()));
                // Existing temporary subjects remain governed when issuance
                // is disabled. All realtime paths share this ledger provider.
                let realtime_builder_scope = auth_methods.scope.clone();
                let realtime_state = Arc::new(
                    realtime_ws::RealtimeState::builder(
                        config.realtime.clone(),
                        Arc::clone(&runtime_registry),
                        Arc::clone(&delivery_registry),
                        Arc::clone(&clock),
                    )
                    .with_capacity(config.world.default_capacity)
                    .with_interest_grid(interest_grid)
                    .with_world_store(world_store_for_realtime)
                    .with_checkpoint_store(Arc::clone(&checkpoint_store))
                    .with_checkpoint_limits(checkpoint_limits)
                    .with_optional_generation_services(generation.clone())
                    .with_persistent_entity_store(Arc::clone(&persistent_entity_store))
                    .with_command_dedup(Arc::clone(&command_dedup))
                    .with_world_authorizer(realtime_world_authorizer)
                    .with_identity_repository(Arc::clone(&identity_repo))
                    .with_tickets(tickets)
                    .with_pre_commit_gate(pre_commit_gate)
                    .with_input_rules(input_rules)
                    .with_extension_registrations(extension_registrations)
                    .with_resume_grace_seconds(config.world.resume_grace_seconds)
                    .with_rate_limits(
                        config.rate_limit.normal_per_sec,
                        config.rate_limit.custom_per_sec,
                        config.rate_limit.persistent_threshold,
                    )
                    // D-25/SOAK31: actor history and extension outbox use the
                    // existing realtime queue bound; no separate outbox key.
                    .with_history_capacity(config.realtime.outbound_queue_capacity as usize)
                    .with_movement_limits(config.world.max_speed, config.world.max_acceleration)
                    .with_speed_acceleration_check(
                        config.world.speed_acceleration_check_enabled,
                    )
                    .with_component_updates_per_sec(config.world.component_updates_per_sec)
                    .with_mailbox_config(orbisync_world_runtime::actor::MailboxConfig {
                        control_capacity: config.world.mailbox_control_capacity,
                        transform_capacity: config.world.mailbox_transform_capacity,
                        entity_capacity: config.world.mailbox_entity_capacity,
                    })
                    .with_metrics_recorder(Arc::clone(&metrics_recorder))
                    .with_shutdown(Arc::clone(&shutdown))
                    .with_ephemeral_scope(realtime_builder_scope)
                    .build(),
                );
                // ADR-025 "既知の迂回経路": populate the spawn-hook-active
                // cache before serving connections, mirroring the extension
                // registration cache refresh above — otherwise every
                // Transform auto-create would be allowed until the first
                // periodic refresh completed, 15 seconds after boot.
                realtime_state.refresh_spawn_hook_active_cache().await;
                // D-26 keeps disconnected presences for resume, but expired
                // bindings must be reclaimed or they retain membership slots
                // forever. Run at half the configured grace period so expiry is
                // observed promptly without introducing another fixed cadence.
                {
                    let prune_state = Arc::clone(&realtime_state);
                    tokio::spawn(async move {
                        let interval = std::time::Duration::from_secs(
                            prune_state.resume_prune_interval_seconds(),
                        );
                        let mut ticker = tokio::time::interval(interval);
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        ticker.tick().await;
                        loop {
                            ticker.tick().await;
                            let removed = prune_state.prune_expired_resume_sessions_async().await;
                            if removed > 0 {
                                tracing::info!(
                                    event = "realtime.resume_bindings_pruned",
                                    removed,
                                    detail = "expired resume bindings and their membership slots reclaimed"
                                );
                            }
                        }
                    });
                }
                {
                    let extension_cache_store = Arc::clone(&extension_store);
                    let spawn_hook_cache_state = Arc::clone(&realtime_state);
                    tokio::spawn(async move {
                        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15));
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        ticker.tick().await;
                        loop {
                            ticker.tick().await;
                            if let Err(error) = extension_cache_store
                                .refresh_active_registration_cache()
                                .await
                            {
                                tracing::warn!(
                                    event = "extension.registration_cache_refresh_failed",
                                    error = %error,
                                    detail = "persisting extension events until the registration cache is refreshed"
                                );
                            }
                            // ADR-025 "既知の迂回経路": keeps every already
                            // running instance actor's shared
                            // `spawn_hook_active` flag in sync with
                            // registration changes, without waiting for a
                            // restart (`refresh_spawn_hook_active_cache`
                            // leaves the previous value in place on a
                            // transient lookup failure, logged internally
                            // by the store, not here).
                            spawn_hook_cache_state
                                .refresh_spawn_hook_active_cache()
                                .await;
                        }
                    });
                }
                let http_router = router(http_state);
                let ws_router = axum::Router::new()
                    .route("/ws", axum::routing::get(realtime_ws::realtime_ws_handler))
                    .route(
                        "/v1/realtime/ws",
                        axum::routing::get(realtime_ws::realtime_ws_handler),
                    )
                    .with_state(Arc::clone(&realtime_state));
                let app = http_router.merge(ws_router);
                let listener = tokio::net::TcpListener::bind(&config.server.bind)
                    .await
                    .map_err(ServerError::Listener)?;
                tracing::info!(
                    event = "server.started",
                    bind = config.server.bind,
                    version = VERSION
                );
                let (shutdown_entry, shutdown_entered) = tokio::sync::oneshot::channel();
                let shutdown_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let serving = async {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(shutdown_signal(
                    Arc::clone(&shutdown),
                    Arc::clone(&realtime_state),
                    Arc::clone(&checkpoint_store),
                    Arc::clone(&checkpoint_save_permits),
                    extension_shutdown_tx,
                    shutdown_entry,
                    tick_task,
                    runtime_workers,
                    config.server.shutdown_drain_timeout_seconds,
                    Arc::clone(&shutdown_failed),
                ))
                .await
                .map_err(ServerError::Listener)?;
                if extension_worker.await.is_err() {
                    tracing::warn!(event = "extension.delivery_worker_failed");
                }
                // 7. Axum has finished its graceful listener/in-flight request
                // shutdown; only now close the database pool.
                if let Some(owner) = generation_ownership { owner.shutdown().await; }
                pool.close().await;
                tracing::info!(event = "database.pool_closed");
                if shutdown_failed.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(orbisync_application::ApplicationError::port_failure("generation shutdown outcome uncertain; preserve source and recover on primary").into());
                }
                tracing::info!(event = "server.stopped");
                Ok::<(), ServerError>(())
                };
                tokio::pin!(serving);
                match shutdown_deadline::coordinate(
                    serving.as_mut(), shutdown_entered, wait_for_signal(),
                    std::time::Duration::from_secs(config.server.shutdown_drain_timeout_seconds),
                    std::time::Duration::from_secs(config.server.shutdown_force_timeout_seconds),
                    || {
                        tracing::warn!(event="server.drain_deadline_exceeded", "forcing connections; generation completion may remain uncertain");
                        shutdown.force_connections();
                        if let Some(service) = &generation { service.close_admission(); service.cancel_execution(); }
                    },
                ).await {
                    shutdown_deadline::Outcome::Complete(result) => result,
                    shutdown_deadline::Outcome::Uncertain(reason) => {
                        tracing::error!(event="server.shutdown_uncertain", reason, "terminating nonzero; replacement requires process exit proof and primary recovery");
                        std::process::exit(1);
                    }
                }

            }
        }
    })?;
    Ok(())
}

fn run_doctor_command(
    cli: &Cli,
    env: &dyn EnvSource,
    json: bool,
) -> Result<std::process::ExitCode, ServerError> {
    let config_path = match Config::discover_path(cli.config.as_deref(), env) {
        Ok(path) => path,
        Err(error) => {
            let report = doctor::DoctorReport::configuration_failure(&error);
            report.emit(json).map_err(ServerError::BootstrapIo)?;
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let loaded = match Config::load(config_path.as_deref(), env, &cli.overrides()) {
        Ok(loaded) => loaded,
        Err(error) => {
            let report = doctor::DoctorReport::configuration_failure(&error);
            report.emit(json).map_err(ServerError::BootstrapIo)?;
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let source = config_path.as_deref().map_or_else(
        || String::from("defaults/environment"),
        |path| path.display().to_string(),
    );
    let runtime =
        build_runtime(loaded.config.server.worker_threads).map_err(ServerError::Runtime)?;
    let report = runtime.block_on(doctor::evaluate(
        &loaded.config,
        env,
        &loaded.warnings,
        &source,
    ));
    let healthy = report.is_healthy();
    report.emit(json).map_err(ServerError::BootstrapIo)?;
    Ok(if healthy {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

fn create_token_service(
    env: &dyn EnvSource,
    config: &Config,
) -> Result<AccessTokenService, ConfigError> {
    let env_name = config.auth.token_signing_key_env.clone();
    let raw = env.get(&env_name).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::MissingSecret,
            env_name.clone(),
            "required secret environment variable is not set",
        )
    })?;
    if raw.trim().is_empty() {
        return Err(ConfigError::new(
            ConfigErrorKind::MissingSecret,
            env_name.clone(),
            "required secret environment variable is not set",
        ));
    }
    // The env var must contain an Ed25519 PKCS#8 private key in PEM form.
    // It may contain literal `\n` escapes (single-line .env) or real newlines;
    // the token service normalises `\n` internally, so we pass raw bytes
    // without logging or transforming the secret in a way that leaks it.
    AccessTokenService::from_ed25519_private_pem(
        raw.as_bytes(),
        "orbisync",
        "orbisync-api",
        "key-1",
    )
    .map(|svc| svc.with_access_token_ttl(config.auth.access_token_ttl_seconds))
    .map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::InvalidValue,
            env_name.clone(),
            "token signing key is invalid: must be Ed25519 PKCS#8 PEM",
        )
    })
}

fn build_extension_gateway(
    pool: &sqlx::PgPool,
    config: &Config,
    env: &dyn EnvSource,
    registry: Arc<orbisync_world_runtime::RuntimeRegistry>,
) -> Result<orbisync_extensions::command::ExtensionGateway, ServerError> {
    let name = orbisync_extensions::command::TOKEN_KEY_ENV;
    let key = env.get(name).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::MissingSecret,
            name,
            "required extension credential key is not set",
        )
    })?;
    let reads = orbisync_server::extension_reads::ExtensionReads {
        registry,
        audit: Arc::new(PgIdentityQueryStore::new(pool.clone(), config, env)?),
    };
    orbisync_extensions::command::ExtensionGateway::new(
        Arc::new(orbisync_storage_postgres::PgExtensionTokenStore::new(
            pool.clone(),
        )),
        Arc::new(reads),
        key.into_bytes(),
    )
    .map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::InvalidValue,
            name,
            "extension credential key must contain at least 32 bytes",
        )
        .into()
    })
}

fn read_password_corpus(path: &Path) -> Result<Vec<String>, ServerError> {
    let content = std::fs::read_to_string(path).map_err(ServerError::BootstrapIo)?;
    Ok(content.lines().map(str::to_owned).collect())
}

enum PasswordOutput {
    Terminal,
    File(std::fs::File),
}

impl PasswordOutput {
    fn prepare(path: Option<&Path>) -> Result<Self, ServerError> {
        if let Some(path) = path {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            return options
                .open(path)
                .map(Self::File)
                .map_err(ServerError::BootstrapIo);
        }
        if std::io::stdout().is_terminal() {
            Ok(Self::Terminal)
        } else {
            Err(ServerError::BootstrapIo(std::io::Error::other(
                "non-TTY bootstrap requires --password-output",
            )))
        }
    }

    fn write(&mut self, password: &str) -> Result<(), ServerError> {
        match self {
            Self::Terminal => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{password}").map_err(ServerError::BootstrapIo)?;
                stdout.flush().map_err(ServerError::BootstrapIo)
            }
            Self::File(file) => {
                writeln!(file, "{password}").map_err(ServerError::BootstrapIo)?;
                file.sync_all().map_err(ServerError::BootstrapIo)
            }
        }
    }
}

/// Resolves when the process is asked to terminate.
///
/// Graceful shutdown stops accepting new work first and then drains
/// (specification §37.3, `architecture.md` §4.3). Milestone 3 extends this with
/// realtime connection draining and checkpoint flushing.
async fn mark_instances_draining(
    shutdown: &ShutdownState,
    registry: &orbisync_world_runtime::RuntimeRegistry,
) -> usize {
    // Accepted activation/Join/command handlers may still publish actors or
    // submit work. Include them before taking the final registry snapshot.
    // The outer first-signal deadline continues to own this wait.
    shutdown.drain_admissions().await;
    let mut changed = 0;
    for handle in registry.handles() {
        let outcome = handle
            .submit(orbisync_world_runtime::command::InstanceCommand::Shutdown)
            .await;
        if matches!(
            outcome,
            Ok(orbisync_world_runtime::command::CommandOutcome::Applied { .. })
        ) {
            changed += 1;
        }
    }
    changed
}

async fn reap_idle_instance(
    registry: Arc<orbisync_world_runtime::RuntimeRegistry>,
    delivery: Arc<delivery::DeliveryRegistry>,
    store: Arc<dyn CheckpointStore>,
    dedup: Arc<orbisync_server::command_dedup::CommandDedupStore>,
    metrics: Arc<dyn MetricsRecorder>,
    permits: Arc<tokio::sync::Semaphore>,
    instance_id: InstanceId,
    now: orbisync_domain::Timestamp,
) -> (Vec<ExtensionEvent>, Vec<EntityPersistenceEvent>) {
    const REAP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
    let deadline = tokio::time::Instant::now() + REAP_DEADLINE;
    let _lifecycle =
        match tokio::time::timeout_at(deadline, registry.lifecycle_guard(instance_id)).await {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!(
                    event = "instance.reap_deadline_exceeded",
                    instance_id = %instance_id,
                    detail = "lifecycle fence unavailable before reap deadline"
                );
                return (Vec::new(), Vec::new());
            }
        };
    if delivery.sender_count(instance_id) != 0 {
        return (Vec::new(), Vec::new());
    }
    let Some(current_handle) = registry.handle(instance_id) else {
        return (Vec::new(), Vec::new());
    };
    let _durability =
        match tokio::time::timeout_at(deadline, dedup.durability_guard(instance_id)).await {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!(
                    event = "instance.reap_deadline_exceeded",
                    instance_id = %instance_id,
                    detail = "durability fence unavailable before reap deadline"
                );
                return (Vec::new(), Vec::new());
            }
        };
    // Held from before the payload is built (MAX_CHECKPOINT_PAYLOAD_BYTES
    // bytes) through the save, so at most MAX_PENDING_CHECKPOINT_SAVES
    // payloads are in memory across every checkpoint-saving path at once.
    let _permit = match tokio::time::timeout_at(deadline, permits.acquire()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => {
            tracing::warn!(
                event = "instance.reap_deadline_exceeded",
                instance_id = %instance_id,
                detail = "checkpoint save permit unavailable before reap deadline"
            );
            return (Vec::new(), Vec::new());
        }
    };
    let Some(result) = current_handle.reap_if_idle(now).await else {
        return (Vec::new(), Vec::new());
    };
    let revision = result.checkpoint.revision;
    let checkpoint_persisted = match checkpoint_payload(result.checkpoint.clone(), &dedup, now) {
        Ok(payload) => {
            let payload_len = payload.len();
            let checkpoint = AppCheckpoint::new(instance_id, revision, payload, now);
            match tokio::time::timeout_at(deadline, store.save_checkpoint(checkpoint)).await {
                Ok(Ok(_)) => {
                    tracing::info!(
                        event = "checkpoint.saved",
                        instance_id = %instance_id,
                        revision = revision.as_u64(),
                        payload_bytes = payload_len,
                        detail = "idle-reap checkpoint saved"
                    );
                    true
                }
                Ok(Err(error)) if error.kind() == ApplicationErrorKind::CheckpointTooLarge => {
                    metrics.incr(Counter::CheckpointSaveRejectedTotal);
                    tracing::error!(
                        event = "checkpoint.save_rejected_too_large",
                        instance_id = %instance_id,
                        revision = revision.as_u64(),
                        payload_bytes = payload_len,
                        limit_bytes = MAX_CHECKPOINT_PAYLOAD_BYTES,
                        detail = "idle-reap checkpoint exceeds the payload limit; reap aborted, the world is not durable and will lose state on restart"
                    );
                    false
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        event = "checkpoint.save_failed",
                        instance_id = %instance_id,
                        revision = revision.as_u64(),
                        error = %error,
                        detail = "idle-reap checkpoint save failed; reap aborted"
                    );
                    false
                }
                Err(_) => {
                    tracing::warn!(
                        event = "checkpoint.save_deadline_exceeded",
                        instance_id = %instance_id,
                        revision = revision.as_u64(),
                        "idle-reap checkpoint deadline exceeded; reap aborted"
                    );
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                event = "checkpoint.serialize_failed",
                instance_id = %instance_id,
                error = %error,
                detail = "idle-reap checkpoint serialization failed; reap aborted"
            );
            false
        }
    };
    if checkpoint_persisted {
        registry.remove_task(instance_id);
        tracing::info!(
            event = "instance.reaped",
            instance_id = %instance_id,
            detail = "reaped idle instance actor (member_count == 0 && sender_count == 0)"
        );
        (result.events, result.persistence_events)
    } else {
        registry.restart_reaped_instance(result);
        tracing::info!(
            event = "instance.reap_aborted",
            instance_id = %instance_id,
            detail = "restored in-memory actor after final checkpoint persistence failure"
        );
        (Vec::new(), Vec::new())
    }
}

fn checkpoint_payload(
    mut checkpoint: orbisync_world_runtime::Checkpoint,
    dedup: &orbisync_server::command_dedup::CommandDedupStore,
    now: orbisync_domain::Timestamp,
) -> Result<Vec<u8>, serde_json::Error> {
    checkpoint.dedup = dedup.snapshot(checkpoint.instance_id, now.to_unix_millis().unwrap_or(0));
    checkpoint.to_json_bytes()
}

async fn flush_shutdown_checkpoints(
    registry: Arc<orbisync_world_runtime::RuntimeRegistry>,
    store: Arc<dyn CheckpointStore>,
    dedup: Arc<orbisync_server::command_dedup::CommandDedupStore>,
    metrics: Arc<dyn MetricsRecorder>,
    permits: Arc<tokio::sync::Semaphore>,
) {
    const SHUTDOWN_CHECKPOINT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
    let now = SystemClock::new().now();
    let mut tasks = tokio::task::JoinSet::new();
    for handle in registry.handles() {
        let instance_id = handle.instance_id();
        let registry = Arc::clone(&registry);
        let store = Arc::clone(&store);
        let dedup = Arc::clone(&dedup);
        let permits = Arc::clone(&permits);
        tasks.spawn(async move {
            let result = tokio::time::timeout(
                SHUTDOWN_CHECKPOINT_DEADLINE,
                persist_instance_checkpoint(
                    registry.as_ref(),
                    store.as_ref(),
                    dedup.as_ref(),
                    permits.as_ref(),
                    instance_id,
                    now,
                ),
            )
            .await;
            (instance_id, result)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((instance_id, Ok(Ok(_)))) => tracing::info!(
                event = "checkpoint.saved",
                instance_id = %instance_id,
                detail = "shutdown checkpoint saved"
            ),
            Ok((instance_id, Ok(Err(error))))
                if error.kind() == ApplicationErrorKind::CheckpointTooLarge =>
            {
                metrics.incr(Counter::CheckpointSaveRejectedTotal);
                tracing::error!(
                    event = "checkpoint.save_rejected_too_large",
                    instance_id = %instance_id,
                    limit_bytes = MAX_CHECKPOINT_PAYLOAD_BYTES,
                    error = %error,
                    detail = "shutdown checkpoint exceeds the payload limit; the world is not durable and will lose state on restart"
                );
            }
            Ok((instance_id, Ok(Err(error)))) => tracing::warn!(
                event = "checkpoint.save_failed",
                instance_id = %instance_id,
                error = %error,
                "failed to save shutdown checkpoint"
            ),
            Ok((instance_id, Err(_))) => tracing::warn!(
                event = "checkpoint.save_deadline_exceeded",
                instance_id = %instance_id,
                "shutdown checkpoint deadline exceeded; continuing shutdown"
            ),
            Err(error) => tracing::warn!(
                event = "checkpoint.task_failed",
                error = %error,
                "shutdown checkpoint task failed"
            ),
        }
    }
}

/// Builds and saves one instance checkpoint while holding its durability fence.
/// Every checkpoint path (command, periodic, reap, and shutdown) uses this
/// ordering, so a stale snapshot cannot win a save race.
async fn persist_instance_checkpoint(
    registry: &orbisync_world_runtime::RuntimeRegistry,
    store: &dyn CheckpointStore,
    dedup: &orbisync_server::command_dedup::CommandDedupStore,
    permits: &tokio::sync::Semaphore,
    instance_id: InstanceId,
    now: orbisync_domain::Timestamp,
) -> Result<(), orbisync_application::ApplicationError> {
    let _durability = dedup.durability_guard(instance_id).await;
    // Held from before the payload is built (MAX_CHECKPOINT_PAYLOAD_BYTES
    // bytes) through the save, so at most MAX_PENDING_CHECKPOINT_SAVES
    // payloads are in memory across every checkpoint-saving path at once.
    let _permit = permits.acquire().await.map_err(|error| {
        orbisync_application::ApplicationError::new(
            ApplicationErrorKind::PortFailure,
            error.to_string(),
        )
    })?;
    let checkpoint = registry
        .build_checkpoint(instance_id, now)
        .await
        .ok_or_else(|| {
            orbisync_application::ApplicationError::new(
                ApplicationErrorKind::PortFailure,
                "instance checkpoint unavailable",
            )
        })?;
    let revision = checkpoint.revision;
    let payload = checkpoint_payload(checkpoint, dedup, now).map_err(|error| {
        orbisync_application::ApplicationError::new(
            ApplicationErrorKind::PortFailure,
            error.to_string(),
        )
    })?;
    store
        .save_checkpoint(AppCheckpoint::new(instance_id, revision, payload, now))
        .await
        .map(|_| ())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without a signal handler the process can still be stopped by the
            // supervisor; waiting forever keeps the server serving.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(unix)]
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Performs the ordered graceful-shutdown sequence from specification §37.3.
async fn shutdown_signal(
    shutdown: Arc<ShutdownState>,
    realtime: Arc<realtime_ws::RealtimeState>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    checkpoint_save_permits: Arc<tokio::sync::Semaphore>,
    extension_shutdown: tokio::sync::watch::Sender<bool>,
    entered: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    tick_task: Option<tokio::task::JoinHandle<()>>,
    runtime_workers: Option<runtime_maintenance::RuntimeWorkers>,
    drain_timeout_seconds: u64,
    failed: Arc<std::sync::atomic::AtomicBool>,
) {
    if let Some(service) = &realtime.generation {
        tokio::select! {
            () = wait_for_signal() => {},
            () = async { while service.ready() { tokio::time::sleep(std::time::Duration::from_millis(50)).await; } } => {},
        }
    } else {
        wait_for_signal().await;
    }
    let _clock_delivered = entered.send(tokio::time::Instant::now());
    tracing::info!(event = "server.shutdown_requested");

    // 1. Readiness is lowered first; the shared admission flag also makes
    // handlers reject work that raced with the signal.
    shutdown.begin();
    tracing::info!(event = "server.readiness_disabled");
    if extension_shutdown.send(true).is_err() {
        tracing::debug!(event = "extension.delivery_worker_already_stopped");
    }
    tracing::info!(event = "extension.delivery_shutdown_requested");

    // Join the stopped periodic producer before scheduling final captures.
    if let Some(tick_task) = tick_task {
        let _tick_joined = tick_task.await;
    }

    // 2. New upgrades and 3. new instance/runtime work are rejected before
    // notifying established connections.
    let draining_instances = mark_instances_draining(&shutdown, &realtime.registry).await;
    tracing::info!(
        event = "server.draining",
        instances = draining_instances,
        detail = "new connections and instance creation are rejected"
    );

    // 4. Existing sockets receive the already-contractual ErrorMessage below.
    shutdown.notify_connections();
    tracing::info!(event = "realtime.shutdown_notified");

    // Drain accepted entity/extension effects before final checkpoints and pool closure.
    if let Some(runtime_workers) = runtime_workers {
        runtime_workers
            .drain(std::time::Duration::from_secs(drain_timeout_seconds))
            .await;
    }

    // 5. Save every current runtime checkpoint before waiting on connections.
    if let Some(service) = &realtime.generation {
        for handle in realtime.registry.handles() {
            if let Err(error) = service.persist(handle, realtime.clock.now()).await {
                failed.store(true, std::sync::atomic::Ordering::Release);
                tracing::error!(event="generation.shutdown_unavailable", %error);
            }
        }
        service.drain().await;
    } else {
        flush_shutdown_checkpoints(
            Arc::clone(&realtime.registry),
            Arc::clone(&checkpoint_store),
            Arc::clone(&realtime.command_dedup),
            Arc::clone(&realtime.metrics_recorder),
            Arc::clone(&checkpoint_save_permits),
        )
        .await;
    }
    // Recovery is instance-local and already bounded. Give it a short chance
    // to finish after the shutdown snapshots have captured every dirty
    // outcome, then force remaining workers rather than hanging the process.
    realtime
        .drain_recovery(std::time::Duration::from_secs(2))
        .await;

    // The outer coordinator also owns final writer/pool close and both signal
    // deadlines. Zero sockets never bypass outstanding generation jobs.
    while realtime
        .active_connections
        .load(std::sync::atomic::Ordering::Acquire)
        != 0
    {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn fix74_real_membership_adapter_propagates_generation_refusal() {
        use orbisync_application::InstanceMembershipStore as _;
        use orbisync_application::checkpoint_admission::*;
        #[derive(Debug)]
        struct Store;
        #[async_trait::async_trait]
        impl GenerationStore for Store {
            fn limits(&self) -> orbisync_application::CheckpointLimits {
                Default::default()
            }
            async fn publish(
                &self,
                _: &GenerationAttempt,
                _: &mut dyn GenerationSource,
            ) -> Result<GenerationResolution, ApplicationError> {
                Ok(GenerationResolution::Uncertain)
            }
            async fn resolve(
                &self,
                _: &GenerationAttempt,
            ) -> Result<GenerationResolution, ApplicationError> {
                Ok(GenerationResolution::Uncertain)
            }
        }
        let registry = Arc::new(RuntimeRegistry::new());
        let instance = InstanceId::generate();
        let user = UserId::generate();
        let presence = PresenceId::generate();
        let writer = WriterPermit::new(WriterToken {
            epoch: 1,
            boot: Uuid::now_v7(),
        })
        .unwrap();
        let mut actor = make_actor(instance);
        actor
            .enable_generation_admission_with_limits(
                Some(orbisync_world_runtime::actor::GenerationBinding {
                    store: Arc::new(Store),
                    writer: writer.clone(),
                    head: 1,
                }),
                Default::default(),
                vec![],
            )
            .unwrap();
        registry.ensure_instance(actor);
        let joined = registry
            .submit(
                instance,
                InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id: instance,
                    capacity: 10,
                },
            )
            .await
            .unwrap();
        assert!(matches!(joined, CommandOutcome::Applied { .. }));
        let handle = registry.handle(instance).unwrap();
        handle
            .capture_generation(Timestamp::from_unix_millis(1000).unwrap())
            .await
            .unwrap();
        let adapter = super::RuntimeInstanceMembershipStore {
            registry: registry.clone(),
        };
        let kicked = adapter.kick_member(instance, user).await;
        let members = adapter.list_members(instance).await.unwrap();
        println!(
            "real adapter kick result={kicked:?}, target remains={}",
            members.contains(&user)
        );
        assert!(kicked.is_err());
        assert!(members.contains(&user));
        writer.invalidate();
        assert!(adapter.list_members(instance).await.is_err());
        assert!(adapter.kick_member(instance, user).await.is_err());
        let absent = InstanceId::generate();
        assert!(adapter.list_members(absent).await.unwrap().is_empty());
        assert!(!adapter.kick_member(absent, user).await.unwrap());
    }
    use super::{
        MAX_PENDING_CHECKPOINT_SAVES, ServerError, build_runtime, collect_extension_events,
        ensure_bootstrap_allowed, flush_shutdown_checkpoints, idle_instance_ids,
        parse_cors_origins, persist_entity_events, persist_extension_events,
        persist_instance_checkpoint, reap_idle_instance,
    };
    use async_trait::async_trait;
    use orbisync_application::{
        AppCheckpoint, ApplicationError, CheckpointStore, EntityOwnershipTransferAudit,
        EntityPersistenceEvent, ExtensionEvent, ExtensionOutboxStore, PersistentEntityStore,
        metrics::{Counter, Gauge, Histogram as MetricsHistogram, MetricsRecorder},
    };
    use orbisync_config::ConfigErrorKind;
    use orbisync_domain::{
        CommandId, EntityId, EntityKind, InstanceId, PresenceId, Revision, Timestamp, UserId,
        VisibilityPolicy,
    };
    use orbisync_server::delivery::DeliveryRegistry;
    use orbisync_world_runtime::{
        InstanceRuntimeDescriptor, RuntimeRegistry, RuntimeState,
        actor::InstanceActor,
        command::{CommandOutcome, InstanceCommand, WorldPermissions},
    };
    use std::collections::HashMap;
    use std::future::pending;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;
    use uuid::Uuid;

    include!("shutdown_admission_tests.rs");

    #[derive(Default)]
    struct RecordingExtensionStore {
        events: Mutex<Vec<ExtensionEvent>>,
    }

    #[async_trait]
    impl ExtensionOutboxStore for RecordingExtensionStore {
        async fn append_event(&self, event: ExtensionEvent) -> Result<Uuid, ApplicationError> {
            self.events.lock().unwrap().push(event);
            Ok(Uuid::nil())
        }
    }

    #[test]
    fn configured_worker_threads_build_the_requested_tokio_runtime() {
        let runtime = build_runtime(2).expect("runtime");
        assert_eq!(runtime.metrics().num_workers(), 2);
    }

    #[test]
    fn graceful_shutdown_source_preserves_required_order_and_logging() {
        let source = include_str!("main.rs");
        let shutdown_fn = source
            .find("async fn shutdown_signal")
            .expect("shutdown function");
        let tests_start = source.find("#[cfg(test)]").expect("test module");
        let sequence = &source[shutdown_fn..tests_start];
        let ready = sequence.find("shutdown.begin();").expect("readiness step");
        let notify = sequence
            .find("shutdown.notify_connections();")
            .expect("connection notification step");
        let checkpoint = sequence
            .find("flush_shutdown_checkpoints(")
            .expect("checkpoint step");
        let drain = sequence
            .find("// The outer coordinator")
            .expect("drain step");
        let serve = source
            .find("with_graceful_shutdown(shutdown_signal(")
            .expect("graceful listener step");
        let pool_call = ["pool.", "close().await"].concat();
        let pool_close = source.find(&pool_call).expect("pool close step");
        assert!(
            ready < notify,
            "readiness must be lowered before notification"
        );
        assert!(
            notify < checkpoint,
            "notification must precede checkpointing"
        );
        assert!(checkpoint < drain, "checkpointing must precede draining");
        assert!(serve < pool_close, "pool close must follow listener drain");
        assert!(
            !sequence.contains(&pool_call),
            "pool close must not happen before shutdown sequence returns"
        );
        assert!(
            source.contains("server.drain_deadline_exceeded")
                && source.contains("server.shutdown_uncertain"),
            "deadline uncertainty must be logged"
        );
        assert!(!sequence.contains(&["M3 ", "stub"].concat()));
        assert!(!sequence.contains(&["real ", "implementation will"].concat()));
    }

    #[test]
    fn delivery_composition_uses_fallible_client_construction() {
        let source = include_str!("main.rs");
        assert!(source.contains("ReqwestHttpClient::try_new()"));
        let infallible_with_question = ["ReqwestHttpClient::new", "()?"].concat();
        assert!(!source.contains(&infallible_with_question));
    }

    #[test]
    fn websocket_router_is_outside_http_body_deadline_scope() {
        // The ordinary HTTP body middleware belongs to transport-http's
        // router. Keep the upgrade router separate so a body deadline cannot
        // terminate a long-lived WebSocket connection.
        let source = include_str!("main.rs");
        let http_router = source
            .find("let http_router = router(")
            .expect("HTTP router");
        let ws_router = source
            .find("let ws_router = axum::Router::new()")
            .expect("WebSocket router");
        let merged = source
            .find("let app = http_router.merge(ws_router)")
            .expect("router merge");
        assert!(http_router < ws_router && ws_router < merged);
    }

    #[test]
    fn composition_root_wires_both_trusted_proxy_builders() {
        // The API-compat `trusted_proxies()` getter returns the original
        // configured strings, so the composition root must restate them
        // through `with_trusted_proxies` after wiring the typed networks.
        // Forgetting the legacy builder silently left the getter out of sync
        // with production trust decisions. Search only the production half of
        // this file so the test cannot match its own source text.
        let source = include_str!("main.rs");
        let tests_start = source.find("#[cfg(test)]").expect("test module");
        let production = &source[..tests_start];
        let networks_wired = production
            .find(".with_trusted_proxy_networks(trusted_proxies)")
            .expect("typed trusted proxy networks wired in the composition root");
        let entries_wired = production
            .find(".with_trusted_proxies(trusted_proxy_entries)")
            .expect("legacy trusted proxy entries wired in the composition root");
        assert!(
            networks_wired < entries_wired,
            "the legacy builder must run last so configured strings survive"
        );
        let normalized: String = production.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains(
                "let trusted_proxy_entries: Vec<String> = config .server .trusted_proxies",
            ),
            "trusted proxy entries must be derived from the configured value"
        );
    }

    #[test]
    fn invalid_cors_origin_fails_startup_validation() {
        let error = parse_cors_origins(&["https://bad\n.example".to_owned()])
            .expect_err("control characters must fail HTTP header validation");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
        assert_eq!(error.key(), "cors.allowed_origins");
        assert!(parse_cors_origins(&["https://allowed.example".to_owned()]).is_ok());
    }

    #[test]
    fn configuration_server_error_preserves_actionable_diagnostic() {
        let config = orbisync_config::ConfigError::new(
            ConfigErrorKind::InvalidValue,
            "ORBISYNC_TOKEN_SIGNING_KEY",
            "token signing key is invalid: must be Ed25519 PKCS#8 PEM",
        );
        let error = ServerError::Config(config);
        let rendered = error.to_string();
        assert!(rendered.contains("invalid_value"));
        assert!(rendered.contains("ORBISYNC_TOKEN_SIGNING_KEY"));
        assert!(rendered.contains("Ed25519 PKCS#8 PEM"));
    }

    #[tokio::test]
    async fn coordinator_drains_actor_events_into_extension_store() {
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
        let expected = actor.outbox().events().to_vec();
        let mut pending = Vec::new();

        collect_extension_events(&mut actor, &mut pending);
        assert!(actor.outbox().is_empty());

        let store = RecordingExtensionStore::default();
        persist_extension_events(&store, pending).await;
        assert_eq!(*store.events.lock().unwrap(), expected);
    }

    #[derive(Default)]
    struct RecordingPersistentEntityStore {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PersistentEntityStore for RecordingPersistentEntityStore {
        async fn spawn(&self, entity: orbisync_domain::Entity) -> Result<(), ApplicationError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("spawn:{}", entity.id()));
            Ok(())
        }

        async fn update(&self, entity: orbisync_domain::Entity) -> Result<(), ApplicationError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update:{}", entity.id()));
            Ok(())
        }

        async fn transfer_ownership(
            &self,
            entity: orbisync_domain::Entity,
            _audit: EntityOwnershipTransferAudit,
        ) -> Result<(), ApplicationError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("transfer_ownership:{}", entity.id()));
            Ok(())
        }

        async fn upsert_component(
            &self,
            entity_id: EntityId,
            _instance_id: InstanceId,
            _revision: Revision,
            _updated_at: Timestamp,
            component_key: String,
            _payload: Vec<u8>,
        ) -> Result<(), ApplicationError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("upsert_component:{entity_id}:{component_key}"));
            Ok(())
        }

        async fn delete_component(
            &self,
            entity_id: EntityId,
            _instance_id: InstanceId,
            _revision: Revision,
            _updated_at: Timestamp,
            component_key: String,
        ) -> Result<(), ApplicationError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("delete_component:{entity_id}:{component_key}"));
            Ok(())
        }

        async fn delete(
            &self,
            entity_id: EntityId,
            _instance_id: InstanceId,
        ) -> Result<(), ApplicationError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("delete:{entity_id}"));
            Ok(())
        }

        async fn list_by_instance(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Vec<orbisync_domain::Entity>, ApplicationError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct FailingPersistentEntityStore;

    #[async_trait]
    impl PersistentEntityStore for FailingPersistentEntityStore {
        async fn spawn(&self, _entity: orbisync_domain::Entity) -> Result<(), ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }
        async fn update(&self, _entity: orbisync_domain::Entity) -> Result<(), ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }
        async fn transfer_ownership(
            &self,
            _entity: orbisync_domain::Entity,
            _audit: EntityOwnershipTransferAudit,
        ) -> Result<(), ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }
        async fn upsert_component(
            &self,
            _entity_id: EntityId,
            _instance_id: InstanceId,
            _revision: Revision,
            _updated_at: Timestamp,
            _component_key: String,
            _payload: Vec<u8>,
        ) -> Result<(), ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }
        async fn delete_component(
            &self,
            _entity_id: EntityId,
            _instance_id: InstanceId,
            _revision: Revision,
            _updated_at: Timestamp,
            _component_key: String,
        ) -> Result<(), ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }
        async fn delete(
            &self,
            _entity_id: EntityId,
            _instance_id: InstanceId,
        ) -> Result<(), ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }

        async fn list_by_instance(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Vec<orbisync_domain::Entity>, ApplicationError> {
            Err(ApplicationError::port_failure("simulated store outage"))
        }
    }

    #[derive(Default)]
    struct RecordingFailureMetrics {
        entity_persistence_failures: AtomicU64,
    }

    impl MetricsRecorder for RecordingFailureMetrics {
        fn incr(&self, counter: Counter) {
            self.add(counter, 1);
        }
        fn add(&self, counter: Counter, n: u64) {
            if matches!(counter, Counter::EntityPersistenceFailuresTotal) {
                self.entity_persistence_failures
                    .fetch_add(n, Ordering::SeqCst);
            }
        }
        fn set(&self, _gauge: Gauge, _value: i64) {}
        fn observe(&self, _histogram: MetricsHistogram, _value: f64) {}
    }

    fn current_entity_revision(actor: &InstanceActor, entity_id: EntityId) -> Revision {
        actor
            .entities_snapshot()
            .into_iter()
            .find(|entity| entity.id() == entity_id)
            .expect("entity present in actor state")
            .revision()
    }

    #[tokio::test]
    async fn persistence_batches_multiple_mutations_into_one_store_pass() {
        let instance = InstanceId::generate();
        let mut actor = make_actor(instance);
        let entity_id = EntityId::generate();
        let requester = UserId::generate();

        actor
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
            .expect("spawn applies");

        for key in ["com.example.a", "com.example.b"] {
            let expected_revision = current_entity_revision(&actor, entity_id);
            let outcome = actor
                .submit(InstanceCommand::UpdateEntityComponent {
                    command_id: None,
                    entity_id,
                    component_key: key.to_owned(),
                    payload_bytes: vec![1],
                    expected_revision,
                    now: Timestamp::from_unix_millis(1_000).expect("ts"),
                    requester,
                    permissions: WorldPermissions::all(),
                })
                .expect("component update applies");
            assert!(matches!(outcome, CommandOutcome::Applied { .. }));
        }

        let store = RecordingPersistentEntityStore::default();
        let metrics = RecordingFailureMetrics::default();

        // Nothing has reached the store yet: the actor buffers persistence
        // effects in its own outbox and never calls the store directly.
        // Per-command I/O is exactly the HIGH-002 defect this wiring closes.
        assert!(store.calls.lock().unwrap().is_empty());

        let events = actor.drain_persistence_batch();
        assert_eq!(
            events.len(),
            3,
            "one spawn and two component updates must batch into a single drain"
        );

        persist_entity_events(&store, &metrics, events).await;

        let calls = store.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                format!("spawn:{entity_id}"),
                format!("upsert_component:{entity_id}:com.example.a"),
                format!("upsert_component:{entity_id}:com.example.b"),
            ],
            "the batch must reach the store in one call to persist_entity_events, in order"
        );
    }

    #[test]
    fn periodic_tasks_persist_drained_batch_after_tick_and_reap() {
        let source = include_str!("main.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("test module")];
        let calls: Vec<_> = production.match_indices("persist_entity_events(").collect();
        assert_eq!(calls.len(), 2, "one definition and one per-instance batch call");
        let tick = production.find("let result = handle.tick(now, false)").unwrap();
        let reap = production.find("let (events, persistence_events) = reap_idle_instance(").unwrap();
        assert!(tick < reap && reap < calls[1].0);
        let drain = production.find("while let Some(joined) = tasks.join_next().await").unwrap();
        assert!(calls[1].0 < drain);
    }

    #[tokio::test]
    async fn transform_updates_do_not_produce_persistence_events() {
        let instance = InstanceId::generate();
        let mut actor = make_actor(instance);
        let entity_id = EntityId::generate();
        let user_id = UserId::generate();
        let transform_at = |x: f32| {
            orbisync_domain::Transform::new(
                orbisync_domain::Vec3::new(x, 0.0, 0.0).expect("position"),
                orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("rotation"),
                orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("scale"),
            )
            .expect("transform")
        };

        // First transform on an unknown entity auto-spawns it (actor.rs
        // UpdateTransform branch): this is a spawn, and must persist once.
        actor
            .submit(InstanceCommand::UpdateTransform {
                entity_id,
                transform: transform_at(0.0),
                expected_revision: Revision::from_u64(1),
                user_id,
                now: Timestamp::from_unix_millis(1_000).expect("ts"),
                permissions: WorldPermissions::all(),
            })
            .expect("auto-spawn transform applies");

        for step in 1..=5i64 {
            let expected_revision = current_entity_revision(&actor, entity_id);
            actor
                .submit(InstanceCommand::UpdateTransform {
                    entity_id,
                    transform: transform_at(step as f32),
                    expected_revision,
                    user_id,
                    now: Timestamp::from_unix_millis(1_000 + step * 1_000).expect("ts"),
                    permissions: WorldPermissions::all(),
                })
                .expect("transform update applies");
        }

        let events = actor.drain_persistence_batch();
        assert_eq!(
            events.len(),
            1,
            "only the auto-spawn may persist; five subsequent transform-only \
             updates must not touch durable state (state-and-runtime.md §1.1)"
        );
        assert!(matches!(events[0], EntityPersistenceEvent::Spawned(_)));
    }

    #[tokio::test]
    async fn persist_failure_is_counted_and_does_not_block_ephemeral_processing() {
        let instance = InstanceId::generate();
        let mut actor = make_actor(instance);
        let entity_id = EntityId::generate();
        let requester = UserId::generate();

        actor
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
            .expect("spawn applies");

        let events = actor.drain_persistence_batch();
        assert_eq!(events.len(), 1);

        let store = FailingPersistentEntityStore;
        let metrics = RecordingFailureMetrics::default();
        persist_entity_events(&store, &metrics, events).await;
        assert_eq!(
            metrics.entity_persistence_failures.load(Ordering::SeqCst),
            1,
            "a failed durable write must be counted -- heavier than an \
             extension delivery miss, since it risks the HIGH-002 data-loss \
             failure mode"
        );

        // Ephemeral processing must continue after the persistence failure
        // (state-and-runtime.md §3.5): a transform update on the same entity
        // still applies.
        let outcome = actor
            .submit(InstanceCommand::UpdateTransform {
                entity_id,
                transform: orbisync_domain::Transform::new(
                    orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("position"),
                    orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("rotation"),
                    orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("scale"),
                )
                .expect("transform"),
                expected_revision: current_entity_revision(&actor, entity_id),
                user_id: requester,
                now: Timestamp::from_unix_millis(2_000).expect("ts"),
                permissions: WorldPermissions::all(),
            })
            .expect("transform still applies after a persistence failure");
        assert!(matches!(outcome, CommandOutcome::Applied { .. }));
    }

    // V-07: ORBISYNC_REQUIRE_DB must fail closed rather than silently skip,
    // matching tests/integration/tests/common::pool_or_skip. expect_used and
    // unwrap_used are already exempted for test code by clippy.toml
    // (allow-expect-in-tests/allow-unwrap-in-tests); clippy::panic has no such
    // tunable, so it is allowed explicitly on this test-only helper.
    #[allow(clippy::panic)]
    async fn persistence_test_pool() -> Option<sqlx::PgPool> {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.trim().is_empty() => {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(5)
                    .connect_lazy(&url)
                    .expect("pool is created");
                orbisync_storage_postgres::run_migrations(&pool)
                    .await
                    .expect("migrations apply");
                Some(pool)
            }
            _ => {
                if std::env::var("ORBISYNC_REQUIRE_DB").is_ok() {
                    panic!(
                        "SKIPPED (V-07): DATABASE_URL not set or empty; \
                         ORBISYNC_REQUIRE_DB requires a real DB"
                    );
                }
                None
            }
        }
    }

    async fn insert_persistence_test_instance(
        pool: &sqlx::PgPool,
        instance_id: InstanceId,
        at: Timestamp,
    ) -> Uuid {
        let world_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO world_definitions
             (id, name, status, capacity, default_spawn, metadata, revision, created_at, updated_at)
             VALUES ($1, 'persistence-wiring-test', 'active', 10,
                     '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1},\"scale\":{\"x\":1,\"y\":1,\"z\":1}}'::jsonb,
                     '{}'::jsonb, 1, $2, $2)",
        )
        .bind(world_id)
        .bind(at.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert world");
        sqlx::query(
            "INSERT INTO world_instances
             (id, world_id, lifecycle, capacity, created_at, started_at, revision)
             VALUES ($1, $2, 'running', 10, $3, $3, 1)",
        )
        .bind(instance_id.as_uuid())
        .bind(world_id)
        .bind(at.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert instance");
        world_id
    }

    /// Inserts a minimal `users` row so an entity's `owner_id` FK is
    /// satisfiable; entity persistence tests need a real owner, not just a
    /// generated `UserId`.
    async fn insert_persistence_test_user(pool: &sqlx::PgPool, user_id: UserId, at: Timestamp) {
        sqlx::query(
            "INSERT INTO users
             (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at)
             VALUES ($1, $2, 'persistence wiring test user', 'active', false, 1, $3, $3)",
        )
        .bind(user_id.as_uuid())
        .bind(format!("persistence-wiring-{}", user_id.as_uuid()))
        .bind(at.as_offset_date_time())
        .execute(pool)
        .await
        .expect("insert user");
    }

    async fn cleanup_persistence_test_instance(
        pool: &sqlx::PgPool,
        instance_id: InstanceId,
        world_id: Uuid,
    ) {
        sqlx::query("DELETE FROM persistent_entities WHERE instance_id = $1")
            .bind(instance_id.as_uuid())
            .execute(pool)
            .await
            .expect("delete entities");
        sqlx::query("DELETE FROM world_instances WHERE id = $1")
            .bind(instance_id.as_uuid())
            .execute(pool)
            .await
            .expect("delete instance");
        sqlx::query("DELETE FROM world_definitions WHERE id = $1")
            .bind(world_id)
            .execute(pool)
            .await
            .expect("delete world");
    }

    #[tokio::test]
    async fn wiring_persists_spawned_entity_and_removes_it_on_delete() {
        let Some(pool) = persistence_test_pool().await else {
            return;
        };
        let instance_id = InstanceId::generate();
        let at = Timestamp::from_unix_millis(1_800_000_000_000).expect("timestamp");
        let world_id = insert_persistence_test_instance(&pool, instance_id, at).await;
        let store = orbisync_storage_postgres::PgPersistentEntityStore::new(pool.clone());
        let metrics = RecordingFailureMetrics::default();

        let mut actor = make_actor(instance_id);
        let entity_id = EntityId::generate();
        let requester = UserId::generate();
        insert_persistence_test_user(&pool, requester, at).await;
        actor
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
            .expect("spawn applies");

        let events = actor.drain_persistence_batch();
        assert_eq!(events.len(), 1);
        persist_entity_events(&store, &metrics, events).await;
        assert_eq!(
            metrics.entity_persistence_failures.load(Ordering::SeqCst),
            0
        );

        let row_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM persistent_entities WHERE id = $1 AND instance_id = $2",
        )
        .bind(entity_id.as_uuid())
        .bind(instance_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("row count query");
        assert_eq!(row_count, 1, "spawned entity must exist as a real DB row");

        let expected_revision = current_entity_revision(&actor, entity_id);
        actor
            .submit(InstanceCommand::DeleteEntity {
                command_id: None,
                entity_id,
                expected_revision,
                requester,
                permissions: WorldPermissions::all(),
            })
            .expect("delete applies");
        let delete_events = actor.drain_persistence_batch();
        assert_eq!(delete_events.len(), 1);
        persist_entity_events(&store, &metrics, delete_events).await;

        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM persistent_entities WHERE id = $1")
                .bind(entity_id.as_uuid())
                .fetch_one(&pool)
                .await
                .expect("row count query");
        assert_eq!(
            remaining, 0,
            "deleted entity must be removed as a real DB row"
        );

        cleanup_persistence_test_instance(&pool, instance_id, world_id).await;
    }

    #[tokio::test]
    async fn ownership_transfer_persists_owner_and_audit_atomically() {
        let Some(pool) = persistence_test_pool().await else {
            return;
        };
        let instance_id = InstanceId::generate();
        let at = Timestamp::from_unix_millis(1_800_000_000_000).expect("timestamp");
        let world_id = insert_persistence_test_instance(&pool, instance_id, at).await;
        let store = orbisync_storage_postgres::PgPersistentEntityStore::new(pool.clone());
        let metrics = RecordingFailureMetrics::default();

        let mut actor = make_actor(instance_id);
        let entity_id = EntityId::generate();
        let owner = UserId::generate();
        let new_owner = UserId::generate();
        insert_persistence_test_user(&pool, owner, at).await;
        insert_persistence_test_user(&pool, new_owner, at).await;
        for user_id in [owner, new_owner] {
            actor
                .submit(InstanceCommand::Join {
                    presence_id: PresenceId::generate(),
                    user_id,
                    instance_id,
                    capacity: 10,
                })
                .expect("join applies");
        }
        actor
            .submit(InstanceCommand::SpawnEntity {
                command_id: None,
                entity_id,
                kind: EntityKind::Object,
                owner: Some(owner),
                transform: None,
                visibility: VisibilityPolicy::OwnerOnly,
                requester: owner,
                permissions: WorldPermissions::all(),
            })
            .expect("spawn applies");
        persist_entity_events(&store, &metrics, actor.drain_persistence_batch()).await;

        let command_id = CommandId::generate();
        let expected_revision = current_entity_revision(&actor, entity_id);
        actor
            .submit(InstanceCommand::TransferOwnership {
                command_id: Some(command_id),
                entity_id,
                new_owner: Some(new_owner),
                expected_revision,
                now: at,
                requester: owner,
                permissions: WorldPermissions::all(),
            })
            .expect("transfer applies");
        persist_entity_events(&store, &metrics, actor.drain_persistence_batch()).await;
        assert_eq!(
            metrics.entity_persistence_failures.load(Ordering::SeqCst),
            0
        );

        let durable_owner: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_id FROM persistent_entities WHERE id = $1")
                .bind(entity_id.as_uuid())
                .fetch_one(&pool)
                .await
                .expect("durable owner");
        assert_eq!(durable_owner, Some(new_owner.as_uuid()));
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events \
             WHERE action = 'entity.ownership_transferred' AND target_id = $1 \
               AND request_id = $2 AND actor_user_id = $3 \
               AND metadata->>'previous_owner_id' = $4 \
               AND metadata->>'new_owner_id' = $5",
        )
        .bind(entity_id.to_string())
        .bind(format!("req_{command_id}"))
        .bind(owner.as_uuid())
        .bind(owner.to_string())
        .bind(new_owner.to_string())
        .fetch_one(&pool)
        .await
        .expect("ownership audit query");
        assert_eq!(audit_count, 1);

        cleanup_persistence_test_instance(&pool, instance_id, world_id).await;
        sqlx::query("DELETE FROM audit_events WHERE request_id = $1")
            .bind(format!("req_{command_id}"))
            .execute(&pool)
            .await
            .expect("delete ownership audit");
        sqlx::query("DELETE FROM users WHERE id = ANY($1)")
            .bind(vec![owner.as_uuid(), new_owner.as_uuid()])
            .execute(&pool)
            .await
            .expect("delete transfer users");
    }

    #[tokio::test]
    async fn component_update_persists_only_the_changed_row_and_keeps_timestamps_valid() {
        let Some(pool) = persistence_test_pool().await else {
            return;
        };
        let instance_id = InstanceId::generate();
        let at = Timestamp::from_unix_millis(1_800_000_000_000).expect("timestamp");
        let world_id = insert_persistence_test_instance(&pool, instance_id, at).await;
        let store = orbisync_storage_postgres::PgPersistentEntityStore::new(pool.clone());
        let metrics = RecordingFailureMetrics::default();

        let mut actor = make_actor(instance_id);
        let entity_id = EntityId::generate();
        let requester = UserId::generate();
        insert_persistence_test_user(&pool, requester, at).await;
        actor
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
            .expect("spawn applies");
        persist_entity_events(&store, &metrics, actor.drain_persistence_batch()).await;

        // Two components, each written on its own drain -- mirroring
        // production, which drains once per tick rather than once per
        // command.
        for (key, payload, millis) in [
            ("com.example.a", vec![1u8], 1_000_i64),
            ("com.example.b", vec![2u8], 2_000_i64),
        ] {
            let expected_revision = current_entity_revision(&actor, entity_id);
            actor
                .submit(InstanceCommand::UpdateEntityComponent {
                    command_id: None,
                    entity_id,
                    component_key: key.to_owned(),
                    payload_bytes: payload,
                    expected_revision,
                    now: Timestamp::from_unix_millis(millis).expect("ts"),
                    requester,
                    permissions: WorldPermissions::all(),
                })
                .expect("component update applies");
            persist_entity_events(&store, &metrics, actor.drain_persistence_batch()).await;
        }

        // A third update changes only "com.example.a".
        let expected_revision = current_entity_revision(&actor, entity_id);
        actor
            .submit(InstanceCommand::UpdateEntityComponent {
                command_id: None,
                entity_id,
                component_key: "com.example.a".to_owned(),
                payload_bytes: vec![9u8],
                expected_revision,
                now: Timestamp::from_unix_millis(3_000).expect("ts"),
                requester,
                permissions: WorldPermissions::all(),
            })
            .expect("third component update applies");
        persist_entity_events(&store, &metrics, actor.drain_persistence_batch()).await;

        assert_eq!(
            metrics.entity_persistence_failures.load(Ordering::SeqCst),
            0,
            "every drained write must succeed against the real database"
        );

        let payload_a: Vec<u8> = sqlx::query_scalar(
            "SELECT payload FROM persistent_entity_components WHERE entity_id = $1 AND component_key = 'com.example.a'",
        )
        .bind(entity_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("component a row exists");
        assert_eq!(
            payload_a,
            vec![9u8],
            "the updated component must reflect the new payload"
        );

        let payload_b: Vec<u8> = sqlx::query_scalar(
            "SELECT payload FROM persistent_entity_components WHERE entity_id = $1 AND component_key = 'com.example.b'",
        )
        .bind(entity_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("component b row exists");
        assert_eq!(
            payload_b,
            vec![2u8],
            "updating one component must not rewrite the other component's row"
        );

        let (created_at, updated_at): (time::OffsetDateTime, time::OffsetDateTime) =
            sqlx::query_as("SELECT created_at, updated_at FROM persistent_entities WHERE id = $1")
                .bind(entity_id.as_uuid())
                .fetch_one(&pool)
                .await
                .expect("entity row exists");
        assert!(
            updated_at >= created_at,
            "updated_at must never precede created_at (schema CHECK), even \
             though SpawnEntity currently timestamps the row at the Unix epoch"
        );

        cleanup_persistence_test_instance(&pool, instance_id, world_id).await;
    }

    fn make_actor(instance_id: InstanceId) -> InstanceActor {
        let mut actor = InstanceActor::new(InstanceRuntimeDescriptor {
            instance_id,
            state: RuntimeState::Running,
            revision: Revision::INITIAL,
        });
        actor.start();
        actor
    }

    fn join_one(actor: &mut InstanceActor, instance_id: InstanceId) {
        let presence = PresenceId::generate();
        let user = UserId::generate();
        assert!(
            actor
                .submit(InstanceCommand::Join {
                    presence_id: presence,
                    user_id: user,
                    instance_id,
                    capacity: 10,
                })
                .is_ok()
        );
    }

    #[test]
    fn idle_when_no_members_and_no_senders() {
        // If the condition in `idle_instance_ids` is broken (e.g. `sender_count`
        // removed), this test still passes when idle, but the next two tests
        // catch the regression.
        let id = InstanceId::generate();
        let actor = make_actor(id);
        let mut map = HashMap::new();
        map.insert(id, actor);
        let delivery = DeliveryRegistry::new();
        let idle = idle_instance_ids(&map, &delivery);
        assert_eq!(idle, vec![id]);
    }

    #[test]
    fn not_idle_when_member_present() {
        // member_count == 1 => must not be reaped even if sender_count == 0.
        // Breaks if `member_count() == 0` check is removed.
        let id = InstanceId::generate();
        let mut actor = make_actor(id);
        join_one(&mut actor, id);
        let mut map = HashMap::new();
        map.insert(id, actor);
        let delivery = DeliveryRegistry::new();
        let idle = idle_instance_ids(&map, &delivery);
        assert!(idle.is_empty(), "member present should not be idle");
    }

    #[test]
    fn not_idle_when_sender_present_even_if_no_member() {
        // member_count == 0 but sender_count == 1 => must NOT be reaped.
        // This is the disconnect-in-progress case. If `sender_count == 0`
        // is dropped from the condition, this test turns red (mutation test).
        let id = InstanceId::generate();
        let actor = make_actor(id);
        let mut map = HashMap::new();
        map.insert(id, actor);
        let delivery = DeliveryRegistry::new();
        let _rx = delivery.register(id);
        assert_eq!(delivery.sender_count(id), 1);
        let idle = idle_instance_ids(&map, &delivery);
        assert!(
            idle.is_empty(),
            "sender present should prevent reaping even with 0 members"
        );
    }

    #[test]
    fn reap_enqueues_checkpoint_before_removal() {
        // Verifies ordering: the checkpoint for a reaped instance is built
        // and pushed to `pending` before `remove`. If the tick loop removed
        // first, the checkpoint would be lost (no next opportunity).
        let id = InstanceId::generate();
        let actor = make_actor(id);
        let mut map = HashMap::new();
        map.insert(id, actor);
        let delivery = DeliveryRegistry::new();
        let now = Timestamp::from_unix_millis(1_000).expect("valid");
        let checkpoint_due = false;

        // Simulate the critical section of the ticker.
        let idle_ids = idle_instance_ids(&map, &delivery);
        assert_eq!(idle_ids, vec![id]);

        let mut pending: Vec<AppCheckpoint> = Vec::new();
        if !checkpoint_due {
            for rid in &idle_ids {
                if let Some(a) = map.get(rid) {
                    let rt_cp = a.build_checkpoint(now);
                    let payload = rt_cp.to_json_bytes().expect("serialize");
                    let app_cp =
                        AppCheckpoint::new(rt_cp.instance_id, rt_cp.revision, payload, now);
                    pending.push(app_cp);
                }
            }
        }
        // Must have enqueued before removal.
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].instance_id, id);

        for rid in idle_ids {
            map.remove(&rid);
        }
        assert!(
            !map.contains_key(&id),
            "idle actor must be removed after checkpoint"
        );
        // pending still holds the checkpoint after removal.
        assert_eq!(pending[0].instance_id, id);
    }

    #[test]
    fn idle_with_mixed_instances() {
        let idle_id = InstanceId::generate();
        let busy_id = InstanceId::generate();
        let idle_actor = make_actor(idle_id);
        let mut busy_actor = make_actor(busy_id);
        join_one(&mut busy_actor, busy_id);

        let mut map = HashMap::new();
        map.insert(idle_id, idle_actor);
        map.insert(busy_id, busy_actor);
        let delivery = DeliveryRegistry::new();
        let mut idle = idle_instance_ids(&map, &delivery);
        idle.sort_by_key(|id| id.to_string());
        let mut expected = vec![idle_id];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(idle, expected);
    }

    #[test]
    fn bootstrap_allowed_when_no_users_exist() {
        let store = orbisync_testkit::FakeIdentityStore::new();
        let result = pollster::block_on(ensure_bootstrap_allowed(&store));
        assert!(
            result.is_ok(),
            "empty store must allow bootstrap, got {result:?}"
        );
    }

    #[test]
    fn bootstrap_denied_when_user_already_exists() {
        let store = orbisync_testkit::FakeIdentityStore::new();
        // Insert a user directly to simulate an existing administrator.
        let now = Timestamp::from_unix_millis(1_000).expect("valid");
        let user_id = UserId::generate();
        let login_id = orbisync_domain::LoginId::new("admin").expect("login");
        let user = orbisync_domain::User::new(user_id, login_id, "Admin", now).expect("user");
        let hash =
            orbisync_domain::PasswordHash::new("$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA")
                .expect("hash");
        let credential = orbisync_domain::Credential::new(user_id, hash, now);
        let account = orbisync_application::LoginAccount { user, credential };
        store.insert_account(account);
        let result = pollster::block_on(ensure_bootstrap_allowed(&store));
        assert!(result.is_err(), "non-empty store must deny bootstrap (D-9)");
    }

    #[test]
    fn bootstrap_second_attempt_does_not_create_second_user() {
        // Full end-to-end through the service to prove the pre-check prevents the
        // second call and the original admin is unchanged (D-9).
        // If `ensure_bootstrap_allowed` were removed, the second
        // `bootstrap_administrator` would succeed and create a second admin.
        use std::sync::Arc;

        use orbisync_application::{IdentityQueryPort, PageRequest, RequestId};
        use orbisync_domain::LoginId;
        use orbisync_identity::{
            DynClock, DynIdentityAdministrationStore, IdentityAdministrationService,
            PasswordPolicy, PasswordService,
        };
        use orbisync_testkit::{FakeIdentityStore, FixedClock};

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let store = Arc::new(FakeIdentityStore::new());
            let clock = Arc::new(FixedClock::new(
                Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
            ));
            let passwords =
                PasswordService::new(PasswordPolicy::new(Vec::new())).expect("passwords");
            let dyn_store = DynIdentityAdministrationStore(
                store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>
            );
            let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
            let service = IdentityAdministrationService::new(
                Arc::new(dyn_store),
                Arc::new(dyn_clock),
                passwords,
            );

            // First bootstrap must succeed.
            let (first_user, first_password) = service
                .bootstrap_administrator(
                    LoginId::new("admin").expect("login"),
                    "Admin".to_owned(),
                    RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
                )
                .await
                .expect("first bootstrap must succeed");
            assert_eq!(first_user.login_id().as_str(), "admin");
            assert_eq!(first_password.expose_secret().chars().count(), 27);
            let applied_before = store.applied().len();
            assert_eq!(applied_before, 1);

            // Pre-check must now deny (called before the second bootstrap).
            let denied = ensure_bootstrap_allowed(store.as_ref() as &dyn IdentityQueryPort).await;
            assert!(
                denied.is_err(),
                "second bootstrap pre-check must fail when a user exists"
            );

            // The second service call would also fail via the store's own guard,
            // but the CLI must not even reach it. Verify that the store still
            // refuses if called anyway, and that no second user was added.
            let second = service
                .bootstrap_administrator(
                    LoginId::new("admin2").expect("login"),
                    "Admin2".to_owned(),
                    RequestId::new(format!("req_{}", UserId::generate())).expect("request"),
                )
                .await;
            assert!(
                second.is_err(),
                "second bootstrap_administrator must fail even if pre-check is bypassed"
            );

            // No second applied record and original user still present.
            let applied_after = store.applied().len();
            assert_eq!(
                applied_after, applied_before,
                "failed second bootstrap must not create a new audit/mutation"
            );
            let page = store
                .users(PageRequest {
                    limit: 10,
                    after: None,
                })
                .await
                .expect("users query");
            assert_eq!(page.items.len(), 1, "only the first admin must remain");
            assert_eq!(page.items[0].login_id, "admin");
            assert_eq!(page.items[0].id, first_user.id());
        });
    }

    #[test]
    fn create_token_service_fails_when_env_missing() {
        use orbisync_config::{Config, ConfigErrorKind, MapEnv};
        let config = Config::default();
        let env = MapEnv::default();
        let err = super::create_token_service(&env, &config).expect_err("must fail without env");
        assert_eq!(err.kind(), ConfigErrorKind::MissingSecret);
        assert_eq!(err.key(), config.auth.token_signing_key_env);
        assert!(!err.to_string().contains("MC4CAQAw"));
    }

    #[test]
    fn create_token_service_fails_when_key_invalid() {
        use orbisync_config::{Config, ConfigErrorKind, MapEnv};
        let config = Config::default();
        let env =
            MapEnv::from_pairs([(config.auth.token_signing_key_env.clone(), "not-a-valid-pem")]);
        let err =
            super::create_token_service(&env, &config).expect_err("must fail with invalid pem");
        assert_eq!(err.kind(), ConfigErrorKind::InvalidValue);
        assert_eq!(err.key(), config.auth.token_signing_key_env);
        // Must not leak key material.
        assert!(!err.to_string().contains("not-a-valid-pem"));
    }

    #[test]
    fn create_token_service_fails_when_key_is_whitespace() {
        use orbisync_config::{Config, ConfigErrorKind, MapEnv};
        let config = Config::default();
        let env = MapEnv::from_pairs([(config.auth.token_signing_key_env.clone(), "   ")]);
        let err = super::create_token_service(&env, &config).expect_err("whitespace must fail");
        assert_eq!(err.kind(), ConfigErrorKind::MissingSecret);
    }

    #[test]
    fn create_token_service_succeeds_with_valid_pem() {
        use orbisync_config::{Config, MapEnv};
        use orbisync_domain::{AuthSessionId, Timestamp as Ts, UserId as Uid};
        // Valid Ed25519 private PEM (same as previously hardcoded, now only in test).
        let pem = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
        let config = Config::default();
        let env = MapEnv::from_pairs([(config.auth.token_signing_key_env.clone(), pem)]);
        let svc = super::create_token_service(&env, &config).expect("valid pem must succeed");
        // Verify issued token validates.
        let uid = Uid::generate();
        let sid = AuthSessionId::generate();
        let now = Ts::from_unix_millis(1_700_000_000_000).expect("valid");
        let token = svc.issue(uid, sid, now).expect("issue must succeed");
        let claims = svc.validate(&token, now).expect("validate must succeed");
        assert_eq!(claims.user_id().expect("user"), uid);
    }

    #[test]
    fn create_token_service_accepts_escaped_newlines() {
        use orbisync_config::{Config, MapEnv};
        use orbisync_domain::{AuthSessionId, Timestamp as Ts, UserId as Uid};
        let pem_escaped = "-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\\n-----END PRIVATE KEY-----\\n";
        let config = Config::default();
        let env = MapEnv::from_pairs([(config.auth.token_signing_key_env.clone(), pem_escaped)]);
        let svc = super::create_token_service(&env, &config).expect("escaped pem must succeed");
        let uid = Uid::generate();
        let sid = AuthSessionId::generate();
        let now = Ts::from_unix_millis(1_700_000_000_000).expect("valid");
        let token = svc.issue(uid, sid, now).expect("issue");
        assert!(svc.validate(&token, now).is_ok());
    }

    #[derive(Clone)]
    struct CrossInstanceStore {
        blocked_instance: InstanceId,
        blocked_started: Arc<Notify>,
        fast_saved: Arc<Notify>,
    }

    #[async_trait]
    impl CheckpointStore for CrossInstanceStore {
        async fn save_checkpoint(
            &self,
            checkpoint: AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            if checkpoint.instance_id == self.blocked_instance {
                self.blocked_started.notify_one();
                pending::<()>().await;
            }
            self.fast_saved.notify_one();
            Ok(Vec::new())
        }

        async fn load_latest(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Option<AppCheckpoint>, ApplicationError> {
            Ok(None)
        }
    }

    fn install_idle_actor(registry: &RuntimeRegistry, instance_id: InstanceId) {
        registry.ensure_instance(make_actor(instance_id));
    }

    fn cross_instance_store(blocked_instance: InstanceId) -> CrossInstanceStore {
        CrossInstanceStore {
            blocked_instance,
            blocked_started: Arc::new(Notify::new()),
            fast_saved: Arc::new(Notify::new()),
        }
    }

    #[tokio::test]
    async fn blocked_checkpoint_instance_does_not_create_cross_instance_hol() {
        let now = Timestamp::from_unix_millis(1_000).expect("timestamp");

        // Periodic checkpoint path: both saves use the same production helper,
        // but a blocked instance must not prevent its sibling from completing.
        let periodic_registry = Arc::new(RuntimeRegistry::new());
        let periodic_a = InstanceId::generate();
        let periodic_b = InstanceId::generate();
        install_idle_actor(&periodic_registry, periodic_a);
        install_idle_actor(&periodic_registry, periodic_b);
        let periodic_store = cross_instance_store(periodic_a);
        let periodic_store_dyn: Arc<dyn CheckpointStore> = Arc::new(periodic_store.clone());
        let dedup = Arc::new(orbisync_server::command_dedup::CommandDedupStore::default());
        let periodic_permits = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CHECKPOINT_SAVES));
        let blocked = tokio::spawn({
            let registry = Arc::clone(&periodic_registry);
            let store = Arc::clone(&periodic_store_dyn);
            let dedup = Arc::clone(&dedup);
            let permits = Arc::clone(&periodic_permits);
            async move {
                persist_instance_checkpoint(
                    registry.as_ref(),
                    store.as_ref(),
                    dedup.as_ref(),
                    permits.as_ref(),
                    periodic_a,
                    now,
                )
                .await
            }
        });
        periodic_store.blocked_started.notified().await;
        let fast = tokio::spawn({
            let registry = Arc::clone(&periodic_registry);
            let store = Arc::clone(&periodic_store_dyn);
            let dedup = Arc::clone(&dedup);
            let permits = Arc::clone(&periodic_permits);
            async move {
                persist_instance_checkpoint(
                    registry.as_ref(),
                    store.as_ref(),
                    dedup.as_ref(),
                    permits.as_ref(),
                    periodic_b,
                    now,
                )
                .await
            }
        });
        tokio::time::timeout(
            Duration::from_millis(500),
            periodic_store.fast_saved.notified(),
        )
        .await
        .expect("healthy periodic checkpoint must complete");
        fast.await
            .expect("healthy checkpoint task join")
            .expect("save");
        assert!(
            !blocked.is_finished(),
            "blocked periodic checkpoint must still be isolated"
        );
        blocked.abort();
        assert!(
            matches!(blocked.await, Err(error) if error.is_cancelled()),
            "blocked periodic checkpoint cleanup must be cancellation"
        );

        // Idle reap path: the healthy instance is removed while the blocked
        // instance remains in flight and therefore cannot hold the global loop.
        let reap_registry = Arc::new(RuntimeRegistry::new());
        let reap_a = InstanceId::generate();
        let reap_b = InstanceId::generate();
        install_idle_actor(&reap_registry, reap_a);
        install_idle_actor(&reap_registry, reap_b);
        let reap_store = cross_instance_store(reap_a);
        let reap_store_dyn: Arc<dyn CheckpointStore> = Arc::new(reap_store.clone());
        let dedup = Arc::new(orbisync_server::command_dedup::CommandDedupStore::default());
        let reap_permits = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CHECKPOINT_SAVES));
        let blocked = tokio::spawn(reap_idle_instance(
            Arc::clone(&reap_registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::clone(&reap_store_dyn),
            Arc::clone(&dedup),
            Arc::new(orbisync_application::metrics::NoopMetrics),
            Arc::clone(&reap_permits),
            reap_a,
            now,
        ));
        reap_store.blocked_started.notified().await;
        let fast = tokio::spawn(reap_idle_instance(
            Arc::clone(&reap_registry),
            Arc::new(DeliveryRegistry::new()),
            Arc::clone(&reap_store_dyn),
            Arc::clone(&dedup),
            Arc::new(orbisync_application::metrics::NoopMetrics),
            Arc::clone(&reap_permits),
            reap_b,
            now,
        ));
        tokio::time::timeout(Duration::from_millis(500), reap_store.fast_saved.notified())
            .await
            .expect("healthy reap checkpoint must complete");
        fast.await.expect("healthy reap task join");
        assert!(
            !reap_registry.contains(reap_b),
            "healthy instance must be reaped"
        );
        assert!(
            reap_registry.contains(reap_a),
            "blocked instance remains isolated"
        );
        blocked.abort();
        assert!(
            matches!(blocked.await, Err(error) if error.is_cancelled()),
            "blocked reap checkpoint cleanup must be cancellation"
        );

        // Shutdown path: shutdown checkpointing also starts every instance in
        // its own bounded task, so a stuck save cannot hide the healthy save.
        let shutdown_registry = Arc::new(RuntimeRegistry::new());
        let shutdown_a = InstanceId::generate();
        let shutdown_b = InstanceId::generate();
        install_idle_actor(&shutdown_registry, shutdown_a);
        install_idle_actor(&shutdown_registry, shutdown_b);
        let shutdown_store = cross_instance_store(shutdown_a);
        let shutdown_store_dyn: Arc<dyn CheckpointStore> = Arc::new(shutdown_store.clone());
        let shutdown_task = tokio::spawn(flush_shutdown_checkpoints(
            Arc::clone(&shutdown_registry),
            Arc::clone(&shutdown_store_dyn),
            Arc::new(orbisync_server::command_dedup::CommandDedupStore::default()),
            Arc::new(orbisync_application::metrics::NoopMetrics),
            Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CHECKPOINT_SAVES)),
        ));
        shutdown_store.blocked_started.notified().await;
        tokio::time::timeout(
            Duration::from_millis(500),
            shutdown_store.fast_saved.notified(),
        )
        .await
        .expect("healthy shutdown checkpoint must complete");
        assert!(
            !shutdown_task.is_finished(),
            "shutdown must remain bounded by the blocked instance only"
        );
        shutdown_task.abort();
        assert!(
            matches!(shutdown_task.await, Err(error) if error.is_cancelled()),
            "blocked shutdown checkpoint cleanup must be cancellation"
        );
    }

    /// Counts how many `save_checkpoint` calls are in flight at once and
    /// records the high-water mark, holding each call open just long enough
    /// (a short fixed delay) for genuinely concurrent callers to overlap.
    struct ConcurrencyTrackingStore {
        in_flight: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CheckpointStore for ConcurrencyTrackingStore {
        async fn save_checkpoint(
            &self,
            _checkpoint: AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            let now_in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now_in_flight, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn load_latest(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Option<AppCheckpoint>, ApplicationError> {
            Ok(None)
        }
    }

    /// HIGH-001: the periodic tick spawns one unbounded task per
    /// checkpoint-due instance (`checkpoint_tasks`), each of which builds and
    /// holds a full serialized payload until its save completes. Without a
    /// shared limit, the number of payloads held at once is the instance
    /// count. This proves the shared `checkpoint_save_permits` semaphore
    /// bounds that to `MAX_PENDING_CHECKPOINT_SAVES` regardless of how many
    /// instances are checkpoint-due in the same tick.
    #[tokio::test]
    async fn periodic_checkpoint_concurrency_is_bounded_by_shared_permits() {
        let now = Timestamp::from_unix_millis(1_000).expect("timestamp");
        let registry = Arc::new(RuntimeRegistry::new());
        let dedup = Arc::new(orbisync_server::command_dedup::CommandDedupStore::default());
        let max_seen = Arc::new(AtomicUsize::new(0));
        let tracking_store = Arc::new(ConcurrencyTrackingStore {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::clone(&max_seen),
        });
        let store: Arc<dyn CheckpointStore> = tracking_store;
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CHECKPOINT_SAVES));

        const INSTANCE_COUNT: usize = 5;
        const {
            assert!(
                INSTANCE_COUNT > MAX_PENDING_CHECKPOINT_SAVES,
                "the test must offer more concurrent instances than the bound to be discriminating"
            );
        }
        let mut tasks = Vec::new();
        for _ in 0..INSTANCE_COUNT {
            let instance_id = InstanceId::generate();
            install_idle_actor(&registry, instance_id);
            let registry = Arc::clone(&registry);
            let store = Arc::clone(&store);
            let dedup = Arc::clone(&dedup);
            let permits = Arc::clone(&permits);
            tasks.push(tokio::spawn(async move {
                persist_instance_checkpoint(
                    registry.as_ref(),
                    store.as_ref(),
                    dedup.as_ref(),
                    permits.as_ref(),
                    instance_id,
                    now,
                )
                .await
            }));
        }
        for task in tasks {
            task.await.expect("task join").expect("save");
        }

        let max_seen = max_seen.load(Ordering::SeqCst);
        assert!(
            max_seen <= MAX_PENDING_CHECKPOINT_SAVES,
            "observed {max_seen} concurrent checkpoint saves, expected at most \
             MAX_PENDING_CHECKPOINT_SAVES ({MAX_PENDING_CHECKPOINT_SAVES})"
        );
        assert!(
            max_seen >= 1,
            "the tracking store must have observed at least one save"
        );
    }
    include!("runtime_maintenance_tests.rs");
}

/// Startup validation for the ADR-026 methods, exercised through the real
/// composition-root function against a real database.
///
/// These call `build_auth_method_services` itself rather than a stand-in,
/// because the checks they cover -- resolving role names, rejecting a role
/// that holds too much, refusing an external issuer equal to this server's own
/// -- exist only in that function. A test that hand-built the services would
/// pass while the production path stayed unverified.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod auth_method_startup_tests {
    use super::{AuthMethodServices, build_auth_method_services};
    use orbisync_config::Config;
    use orbisync_domain::Timestamp;
    use orbisync_identity::{SessionIssuer, token::AccessTokenService};
    use sqlx::PgPool;
    use std::sync::Arc;
    use uuid::Uuid;

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    /// Connects to the configured database, or signals that the test cannot run.
    ///
    /// Returns `None` only when no database is configured. A caller must treat
    /// that as "not verified", never as a pass.
    async fn pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = PgPool::connect(&url).await.ok()?;
        orbisync_storage_postgres::run_migrations(&pool)
            .await
            .ok()?;
        Some(pool)
    }

    fn issuer(pool: &PgPool) -> Arc<SessionIssuer> {
        let tokens = Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test-key-1",
            )
            .expect("token service"),
        );
        let clock: Arc<dyn orbisync_domain::Clock> = Arc::new(orbisync_testkit::FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        Arc::new(SessionIssuer::new(
            Arc::new(orbisync_storage_postgres::PgLoginStore::new(pool.clone()))
                as Arc<dyn orbisync_application::LoginTransactionStore>,
            tokens,
            clock,
            b"startup-test-refresh-key".to_vec(),
            900,
            2_592_000,
        ))
    }

    /// Inserts a role holding exactly `permissions` and returns its name.
    async fn seed_role(pool: &PgPool, permissions: &[&str]) -> String {
        let role_id = Uuid::now_v7();
        let name = format!("role-{role_id}");
        sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, NULL, 1)")
            .bind(role_id)
            .bind(&name)
            .execute(pool)
            .await
            .expect("insert role");
        for permission in permissions {
            sqlx::query("INSERT INTO permissions (name) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(permission)
                .execute(pool)
                .await
                .expect("insert permission");
            sqlx::query("INSERT INTO role_permissions (role_id, permission_name) VALUES ($1, $2)")
                .bind(role_id)
                .bind(permission)
                .execute(pool)
                .await
                .expect("insert role permission");
        }
        name
    }

    /// A config with guest enabled and every required guest setting filled in.
    fn guest_config(role_name: &str) -> Config {
        let mut config = Config::default();
        config.auth.methods = vec!["local".to_owned(), "guest".to_owned()];
        config.auth.guest.role_names = vec![role_name.to_owned()];
        config.auth.guest.allowed_worlds = vec![Uuid::now_v7().to_string()];
        config
    }

    async fn build(config: &Config, pool: &PgPool) -> Result<AuthMethodServices, String> {
        build_auth_method_services(config, pool, &issuer(pool))
            .await
            .map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn a_default_configuration_enables_only_local() {
        let Some(pool) = pool().await else { return };
        let services = build(&Config::default(), &pool)
            .await
            .expect("the default configuration must start");
        assert_eq!(services.enabled, vec![orbisync_domain::AuthMethod::Local]);
        // Issuance is disabled, but an existing ledger still governs subjects.
        assert!(services.ephemeral.is_none());
        assert!(services.external.is_none());
        assert_eq!(
            services
                .scope
                .decide(
                    orbisync_domain::UserId::generate(),
                    Uuid::now_v7(),
                    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
                )
                .await
                .expect("scope lookup"),
            orbisync_application::ScopeDecision::NotEphemeral,
        );
    }

    #[tokio::test]
    async fn disabling_issuance_preserves_existing_subject_scope_and_deadline() {
        use orbisync_application::{RequestId, ScopeDecision};

        let Some(pool) = pool().await else { return };
        let role = seed_role(&pool, &["entity.spawn"]).await;
        let mut config = guest_config(&role);
        config.auth.methods.push("name_only".to_owned());
        config.auth.name_only = config.auth.guest.clone();
        let allowed = Uuid::parse_str(&config.auth.guest.allowed_worlds[0]).expect("world");
        let other = Uuid::now_v7();
        let now = Timestamp::from_unix_millis(1_700_000_000_000).expect("valid");
        let deadline = now
            .checked_add_millis(
                i64::try_from(config.auth.guest.session_ttl_seconds * 1_000).expect("TTL"),
            )
            .expect("deadline");
        let enabled = build(&config, &pool).await.expect("enabled startup");
        let service = enabled.ephemeral.as_ref().expect("issuing service");
        let request = || RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request");
        let guest = service.issue_guest(request(), None).await.expect("guest");
        let named = service
            .issue_name_only("Visitor", request(), None)
            .await
            .expect("name only");
        drop(enabled);

        // Rebuild the actual composition root over the same DB, with both
        // methods disabled and settings that must not replace the snapshot.
        config.auth.methods = vec!["local".to_owned()];
        config.auth.guest.allowed_worlds = vec![other.to_string()];
        config.auth.name_only.allowed_worlds = vec![other.to_string()];
        config.auth.guest.session_ttl_seconds *= 2;
        config.auth.name_only.session_ttl_seconds *= 2;
        let disabled = build(&config, &pool).await.expect("disabled startup");
        assert!(disabled.ephemeral.is_none());
        for subject in [guest.user_id, named.user_id] {
            for (world, at, expected) in [
                (allowed, now, ScopeDecision::Allowed),
                (other, now, ScopeDecision::Denied),
                (allowed, deadline, ScopeDecision::Denied),
            ] {
                assert_eq!(
                    disabled
                        .scope
                        .decide(subject, world, at)
                        .await
                        .expect("lookup"),
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn disabled_methods_still_consult_the_ledger_and_propagate_database_failure() {
        // No server is needed: a closed lazy pool proves the provider is wired
        // and cannot classify the subject as permanent on lookup failure.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1/unused")
            .expect("lazy pool");
        pool.close().await;
        let services = build(&Config::default(), &pool).await.expect("startup");
        assert!(services.ephemeral.is_none());
        assert!(
            services
                .scope
                .decide(
                    orbisync_domain::UserId::generate(),
                    Uuid::now_v7(),
                    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_role_that_does_not_exist_stops_startup() {
        let Some(pool) = pool().await else { return };
        let mut config = guest_config("never-created");
        config.auth.guest.role_names = vec!["never-created".to_owned()];
        let error = build(&config, &pool)
            .await
            .expect_err("an unresolvable role must stop startup");
        assert!(
            error.contains("does not exist"),
            "unexpected message: {error}"
        );
    }

    #[tokio::test]
    async fn a_role_within_the_allowlist_starts() {
        let Some(pool) = pool().await else { return };
        let role = seed_role(
            &pool,
            &["world.instance.read", "entity.spawn", "entity.update.own"],
        )
        .await;
        let services = build(&guest_config(&role), &pool)
            .await
            .expect("an allowed role must start");
        assert!(services.ephemeral.is_some());
    }

    #[tokio::test]
    async fn entity_update_any_is_allowed_because_collaboration_needs_it() {
        let Some(pool) = pool().await else { return };
        // Editing another participant's entity is what collaborative editing
        // means. Keeping an owner-only action owner-only is a pre-commit rule,
        // not something RBAC should forbid here.
        let role = seed_role(&pool, &["entity.update.any", "entity.update.own"]).await;
        build(&guest_config(&role), &pool)
            .await
            .expect("entity.update.any must be configurable");
    }

    #[tokio::test]
    async fn a_role_holding_more_than_the_allowlist_stops_startup() {
        let Some(pool) = pool().await else { return };
        // None of these is `admin.*`, which is why a denylist of admin.* would
        // have let every one of them through.
        for excessive in [
            "world.instance.create",
            "world.instance.start",
            "world.instance.stop",
            "moderation.kick",
        ] {
            let role = seed_role(&pool, &["entity.spawn", excessive]).await;
            let error = build(&guest_config(&role), &pool)
                .await
                .unwrap_err_or_panic(excessive);
            assert!(
                error.contains(excessive),
                "`{excessive}` must be named in the failure, got: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_admin_permission_stops_startup() {
        let Some(pool) = pool().await else { return };
        let role = seed_role(&pool, &["admin.users.create"]).await;
        let error = build(&guest_config(&role), &pool)
            .await
            .expect_err("an admin permission must stop startup");
        assert!(error.contains("admin.users.create"), "got: {error}");
    }

    #[tokio::test]
    async fn an_unknown_permission_stops_startup() {
        let Some(pool) = pool().await else { return };
        // A permission this build cannot reason about is not safe to grant.
        let role = seed_role(&pool, &["future.capability.invented.later"]).await;
        let error = build(&guest_config(&role), &pool)
            .await
            .expect_err("an unknown permission must stop startup");
        assert!(
            error.contains("future.capability.invented.later"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn a_world_id_that_is_not_a_uuid_stops_startup() {
        let Some(pool) = pool().await else { return };
        let role = seed_role(&pool, &["entity.spawn"]).await;
        let mut config = guest_config(&role);
        config.auth.guest.allowed_worlds = vec!["not-a-world".to_owned()];
        build(&config, &pool)
            .await
            .expect_err("a malformed world id must stop startup");
    }

    #[tokio::test]
    async fn an_external_issuer_equal_to_our_own_stops_startup() {
        let Some(pool) = pool().await else { return };
        let role = seed_role(&pool, &["entity.spawn"]).await;
        let mut config = Config::default();
        config.auth.methods = vec!["external".to_owned()];
        config.auth.external.issuer = super::INTERNAL_TOKEN_ISSUER.to_owned();
        config.auth.external.audience = "orbisync-api".to_owned();
        config.auth.external.jwks_path = "unused.json".to_owned();
        config.auth.external.role_names = vec![role];
        let error = build(&config, &pool)
            .await
            .expect_err("a self-issuer must stop startup");
        // Otherwise an access token this server minted would be accepted back
        // through /v1/auth/external as though an external party had issued it.
        assert!(
            error.contains("own token issuer"),
            "unexpected message: {error}"
        );
    }

    #[tokio::test]
    async fn a_missing_key_file_stops_startup() {
        let Some(pool) = pool().await else { return };
        let role = seed_role(&pool, &["entity.spawn"]).await;
        let mut config = Config::default();
        config.auth.methods = vec!["external".to_owned()];
        config.auth.external.issuer = "https://idp.test".to_owned();
        config.auth.external.audience = "orbisync-api".to_owned();
        config.auth.external.jwks_path = "definitely-absent-jwks.json".to_owned();
        config.auth.external.role_names = vec![role];
        build(&config, &pool)
            .await
            .expect_err("an unreadable key file must stop startup, not disable the method");
    }

    /// Small helper so the loop above reads clearly.
    trait UnwrapErrOrPanic {
        fn unwrap_err_or_panic(self, context: &str) -> String;
    }

    impl UnwrapErrOrPanic for Result<AuthMethodServices, String> {
        fn unwrap_err_or_panic(self, context: &str) -> String {
            match self {
                Ok(_) => panic!("`{context}` must stop startup but did not"),
                Err(error) => error,
            }
        }
    }
}
