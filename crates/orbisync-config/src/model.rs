//! The configuration model, its defaults and its validation rules.
//!
//! Default values follow `observability-and-config.md` §6.5 and the safety
//! principles of §6.6: secrets have no default, and the recommended initial
//! values come from the design documents that own them.

use core::fmt;
use core::str::FromStr;
use std::net::IpAddr;

use ipnet::IpNet;

use crate::error::{ConfigError, ConfigErrorKind};

/// Parses the comma-separated trusted proxy allowlist into typed networks.
///
/// A plain IP address remains supported for compatibility and is represented
/// as a host network (`/32` for IPv4 or `/128` for IPv6). CIDR parsing is
/// delegated to `ipnet`, which rejects prefixes outside the address family
/// (`0..=32` for IPv4 and `0..=128` for IPv6).
///
/// # Errors
///
/// Returns an invalid-value configuration error for any non-empty malformed
/// entry. Empty entries are ignored so a blank setting remains equivalent to
/// no trusted proxies.
pub fn parse_trusted_proxies(raw: &str) -> Result<Vec<IpNet>, ConfigError> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            if entry.contains('/') {
                entry.parse::<IpNet>().map_err(|_| {
                    invalid(
                        "server.trusted_proxies",
                        "must contain only valid IP addresses or CIDRs (IPv4 prefixes 0-32, IPv6 prefixes 0-128)",
                    )
                })
            } else {
                let ip = entry.parse::<IpAddr>().map_err(|_| {
                    invalid(
                        "server.trusted_proxies",
                        "must contain only valid IP addresses or CIDRs (IPv4 prefixes 0-32, IPv6 prefixes 0-128)",
                    )
                })?;
                let prefix = match ip {
                    IpAddr::V4(_) => 32,
                    IpAddr::V6(_) => 128,
                };
                IpNet::new(ip, prefix).map_err(|_| {
                    invalid(
                        "server.trusted_proxies",
                        "must contain only valid IP addresses or CIDRs (IPv4 prefixes 0-32, IPv6 prefixes 0-128)",
                    )
                })
            }
        })
        .collect()
}

/// Structured log rendering (`observability-and-config.md` §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Single line JSON. Required in production.
    #[default]
    Json,
    /// Human readable rendering. Development only.
    Pretty,
}

impl LogFormat {
    /// Returns the configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Pretty => "pretty",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogFormat {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "json" => Ok(Self::Json),
            "pretty" => Ok(Self::Pretty),
            _ => Err(()),
        }
    }
}

/// Minimum severity written to the log (`observability-and-config.md` §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// Most detailed tracing of individual messages.
    Trace,
    /// Development detail.
    Debug,
    /// Normal business events. Production default.
    #[default]
    Info,
    /// Abnormal but recoverable conditions.
    Warn,
    /// Failures requiring operator attention.
    Error,
}

impl LogLevel {
    /// Returns the configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogLevel {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            _ => Err(()),
        }
    }
}

/// HTTP listener settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address the server binds to.
    pub bind: String,
    /// Comma-separated list of trusted proxy IPs/CIDRs for `X-Forwarded-For` handling.
    /// Entries are parsed as typed networks during startup validation; plain
    /// IPs remain supported as exact host entries.
    /// Empty means no proxy is trusted and the peer address is always used
    /// (ADR-009, P2-C4: source IP must be the trusted-proxy-filtered value).
    pub trusted_proxies: String,
    /// Tokio worker threads; zero uses Tokio's default.
    pub worker_threads: u32,
    /// Maximum HTTP request body in bytes. The default is the axum default;
    /// CSV imports are limited to 1 MiB, so this is twice that size.
    pub max_request_body_bytes: usize,
    /// Maximum total processing time for an ordinary HTTP request.
    pub request_timeout_seconds: u64,
    /// Default number of records returned by a paginated request.
    pub page_default_limit: u16,
    /// Maximum number of records returned by a paginated request.
    pub page_max_limit: u16,
    /// Initial graceful-shutdown drain deadline in seconds.
    pub shutdown_drain_timeout_seconds: u64,
    /// Additional forced-shutdown grace period in seconds.
    pub shutdown_force_timeout_seconds: u64,
}

/// Explicit CORS policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsConfig {
    /// Origins allowed to make browser cross-origin requests.
    pub allowed_origins: Vec<String>,
    /// Whether browser requests may include credentials.
    pub allow_credentials: bool,
}

/// Database settings. The connection string itself is a secret and is read
/// from the environment variable named by [`DatabaseConfig::url_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    /// Name of the environment variable holding the connection string.
    pub url_env: String,
    /// Upper bound of the connection pool.
    pub max_connections: u32,
    /// Pool acquisition timeout in seconds.
    pub acquire_timeout_seconds: u64,
    /// Readiness query timeout in seconds.
    pub readiness_timeout_seconds: u64,
}

/// Authentication settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// Access token lifetime in seconds.
    pub access_token_ttl_seconds: u64,
    /// Refresh token lifetime in seconds.
    pub refresh_token_ttl_seconds: u64,
    /// Minimum accepted password length.
    pub password_min_length: u32,
    /// Name of the environment variable holding the token signing key.
    pub token_signing_key_env: String,
    /// Name of the environment variable holding the pagination cursor HMAC key.
    /// Separate from the JWT signing key so rotation of one does not invalidate
    /// all in-flight pagination cursors of the other (V-04/CR-07 decision).
    pub pagination_hmac_key_env: String,
    /// Name of the environment variable holding the refresh token HMAC key.
    /// Separate from pagination HMAC so rotation of one does not invalidate the
    /// other (V-10 / ADR-002). Must be set; empty value fails startup validation
    /// and secret verification (fail-closed, same as token signing key).
    pub refresh_token_hmac_key_env: String,
    /// Name of the environment variable holding the realtime ticket HMAC key.
    /// Separate from refresh/pagination HMACs so rotation of one does not
    /// invalidate the other (RV-A C1 decision, ADR-002 §52). Must be set;
    /// empty value fails startup validation and secret verification
    /// (fail-closed, same as token signing key). Purpose isolation limits
    /// blast radius: a leaked pagination key does not allow forging tickets.
    pub realtime_ticket_hmac_key_env: String,
    /// Name of the environment variable holding the idempotency HMAC key.
    /// Separate from refresh/pagination/realtime keys so that a DB backup
    /// containing `request_hash` does not allow offline brute-force without
    /// the dedicated idempotency secret (P2-C1, ADR-002 §52). Must be set;
    /// empty value fails startup validation and secret verification
    /// (fail-closed). Purpose isolation limits blast radius and key reuse
    /// is forbidden by `check_auth_secrets`.
    pub idempotency_hmac_key_env: String,
    /// Whether the stub Bearer auth (any non-empty token) is allowed.
    /// Default `false` — production must use real token verification.
    pub allow_stub_bearer: bool,
    /// Argon2id memory cost in KiB.
    pub argon2_memory_cost_kib: u32,
    /// Argon2id iteration count.
    pub argon2_iterations: u32,
    /// Argon2id parallelism.
    pub argon2_parallelism: u32,
    /// Maximum number of concurrent password-hash workers.
    pub password_hash_concurrency: u32,
    /// Failed login attempts before lockout.
    pub login_failure_threshold: u32,
    /// Lockout duration in seconds.
    pub lockout_duration_seconds: u64,
    /// Enabled authentication methods (ADR-026).
    ///
    /// Defaults to `local` alone, so an existing deployment keeps exactly the
    /// behaviour it had before ADR-026. Every other method is opt-in.
    pub methods: Vec<String>,
    /// Guest participation settings (ADR-026 §3).
    pub guest: EphemeralMethodConfig,
    /// Name-only participation settings (ADR-026 §3).
    pub name_only: EphemeralMethodConfig,
    /// External identity provider settings (ADR-026 §8).
    pub external: ExternalAuthConfig,
}

/// Settings shared by the two temporary-subject methods (guest, name-only).
///
/// Both methods issue a subject that has a `users` row but no credential row,
/// so they need the same lifetime cap, role grant and participation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralMethodConfig {
    /// Role names granted to a new subject, resolved to role ids at startup.
    ///
    /// Empty is rejected when the method is enabled: a subject with no role is
    /// denied every permission by `PgWorldAuthorizer`, which would look like a
    /// working configuration while silently refusing all work.
    pub role_names: Vec<String>,
    /// Absolute lifetime of a temporary subject, in seconds.
    ///
    /// Stored as `ephemeral_subjects.expires_at` at issue time and never
    /// extended, so repeated refresh cannot outlive it (ADR-026 §4).
    pub session_ttl_seconds: u64,
    /// Grace period between expiry and the revocation job, in seconds.
    ///
    /// This does not extend access by a single second: expiry is enforced by
    /// comparing `now` against the stored deadline, not by the job having run.
    pub retention_seconds: u64,
    /// World ids a subject issued under this method may join.
    ///
    /// Snapshotted into `ephemeral_subjects.allowed_worlds` at issue time.
    /// Empty is rejected when the method is enabled.
    pub allowed_worlds: Vec<String>,
    /// Prefix used to build the server-generated anonymous display name.
    pub display_name_prefix: String,
}

impl EphemeralMethodConfig {
    /// Returns the disabled-method defaults, carrying only the display prefix.
    ///
    /// `role_names` and `allowed_worlds` stay empty on purpose: the method is
    /// off by default, and enabling it without filling both in is rejected at
    /// startup rather than silently granting or denying everything.
    #[must_use]
    pub fn default_for(display_name_prefix: &str) -> Self {
        Self {
            role_names: Vec::new(),
            session_ttl_seconds: 3_600,
            retention_seconds: 86_400,
            allowed_worlds: Vec::new(),
            display_name_prefix: display_name_prefix.to_owned(),
        }
    }
}

impl ExternalAuthConfig {
    /// Returns the defaults for a deployment with external auth turned off.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            issuer: String::new(),
            audience: String::new(),
            algorithm: "EdDSA".to_owned(),
            jwks_path: String::new(),
            leeway_seconds: 60,
            role_names: Vec::new(),
        }
    }
}

/// External signed-JWT identity provider settings (ADR-026 §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAuthConfig {
    /// Accepted `iss` claim. Must differ from the internal token issuer.
    pub issuer: String,
    /// Accepted `aud` claim.
    pub audience: String,
    /// Accepted signature algorithm. `none` is never accepted.
    pub algorithm: String,
    /// Path to the static JWKS file mapping `kid` to a public key.
    ///
    /// Read from disk, never fetched over HTTP, so the whole path can be
    /// exercised offline against a local signing issuer.
    pub jwks_path: String,
    /// Accepted clock skew for `exp` and `nbf`, in seconds.
    pub leeway_seconds: u64,
    /// Role names granted to a newly seen external subject.
    pub role_names: Vec<String>,
}

/// Realtime message and login rate limits and their bucket bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Normal message rate per second.
    pub normal_per_sec: u32,
    /// Custom message rate per second.
    pub custom_per_sec: u32,
    /// Consecutive denials before persistent classification.
    pub persistent_threshold: u32,
    /// Maximum number of retained ticket limiter buckets.
    pub max_buckets: usize,
    /// Login attempts allowed per source IP in one minute.
    pub login_per_ip_per_minute: u32,
    /// Block duration after a source IP exceeds the login limit.
    pub login_ip_block_seconds: u64,
    /// Maximum number of retained login source-IP buckets.
    pub login_ip_max_buckets: usize,
    /// Realtime tickets allowed per user and interval.
    pub realtime_ticket_per_interval: u32,
}

/// Realtime connection settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeConfig {
    /// Interval between server heartbeats, in seconds.
    pub heartbeat_interval_seconds: u64,
    /// Idle timeout before a connection is considered lost, in seconds.
    pub connection_timeout_seconds: u64,
    /// Maximum size of a normal realtime message in bytes.
    pub max_normal_message_bytes: u64,
    /// Maximum accepted message size in bytes.
    ///
    /// This is the custom-event/message ceiling; normal messages use
    /// `max_normal_message_bytes` instead (transport-boundaries.md §5).
    pub max_message_bytes: u64,
    /// Capacity of the per connection outbound queue.
    pub outbound_queue_capacity: u32,
    /// Whether the stub realtime ticket verifier is allowed.
    ///
    /// When `true` any non-empty `realtime_ticket` is accepted and the
    /// connection is given a fabricated identity. Never enable this on a public
    /// network (N-1). Default `false`, which refuses every realtime connection
    /// until a real ticket verifier exists.
    pub allow_stub_ticket: bool,
    /// Timeout for the client to send `ClientHello` after upgrade, in milliseconds (C3).
    /// Short deadline prevents unauthenticated clients from holding a Tokio task
    /// and socket indefinitely (resource exhaustion).
    pub handshake_timeout_ms: u64,
    /// Maximum number of concurrent WebSocket connections (C3 semaphore).
    /// When full, new upgrades are rejected with 503 before allocating a task.
    pub max_connections: u32,
    /// Capacity of the per-connection delivery channel.
    pub per_connection_capacity: u32,
    /// Interval between expired resume-binding prune passes, in seconds.
    pub resume_prune_interval_seconds: u64,
}

/// World and instance runtime settings.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldConfig {
    /// Default member capacity of a new instance.
    pub default_capacity: u32,
    /// Server tick rate in hertz.
    pub server_tick_hz: u32,
    /// Grace period during which a disconnected member may resume, in seconds.
    pub resume_grace_seconds: u64,
    /// Interval between instance checkpoints, in seconds.
    pub checkpoint_interval_secs: u64,
    /// Checkpoint cadence in simulation ticks.
    pub checkpoint_interval_ticks: u64,
    /// Generation chunk size, 16 KiB through 1 MiB.
    pub checkpoint_chunk_bytes: u64,
    /// Explicit generation-mode opt-in; requires independent rollout gates.
    pub checkpoint_generation_enabled: bool,
    /// Stable deployment lock on the designated singleton host.
    pub checkpoint_writer_lock: String,
    /// Operator-approved deployment identity.
    pub checkpoint_deployment: String,
    /// Generation total policy, 2 through 64 MiB. Legacy remains 8 MiB.
    pub checkpoint_max_serialized_bytes: u64,
    /// Maximum retained revision history entries.
    pub history_capacity: usize,
    /// Maximum movement speed in units per second.
    pub max_speed: f64,
    /// Maximum movement acceleration in units per second squared.
    pub max_acceleration: f64,
    /// Whether the speed/acceleration kinematics check is enforced (ADR-024).
    ///
    /// This is a use-case policy toggle distinct from `max_speed` /
    /// `max_acceleration`: setting either threshold to a very large value is
    /// not an equivalent, verified way to disable the check (their finite
    /// range and interaction with `delta_seconds` clamping are untested at
    /// extreme values). When `false`, Core still enforces ownership,
    /// `expected_revision`, the per-tick teleport distance limit, and numeric
    /// finiteness; only the speed/acceleration comparison is skipped.
    pub speed_acceleration_check_enabled: bool,
    /// Maximum custom component updates per instance per second.
    pub component_updates_per_sec: u32,
    /// Capacity of the per-instance control mailbox.
    pub mailbox_control_capacity: usize,
    /// Capacity of the per-instance transform mailbox.
    pub mailbox_transform_capacity: usize,
    /// Capacity of the per-instance entity mailbox.
    pub mailbox_entity_capacity: usize,
}

/// CSV identity import limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityConfig {
    /// Maximum CSV payload size in bytes.
    pub csv_max_bytes: usize,
    /// Maximum CSV data rows.
    pub csv_max_rows: usize,
}

/// Retention worker intervals and drain budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    /// Realtime ticket cleanup interval.
    pub realtime_ticket_interval_seconds: u64,
    /// Idempotency cleanup interval.
    pub idempotency_interval_seconds: u64,
    /// Cleanup drain budget.
    pub drain_budget_seconds: u64,
    /// Backoff after a failed cleanup attempt.
    pub restart_backoff_seconds: u64,
    /// Number of days delivered extension outbox rows are retained.
    pub extension_outbox_days: u32,
    /// Number of days audit source IP rows are retained.
    pub source_ip_days: u32,
}

/// Outbound extension webhook delivery policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionsConfig {
    /// Maximum number of webhook attempts in flight process-wide.
    pub max_concurrency: u32,
    /// Per-request delivery timeout in seconds.
    pub delivery_timeout_seconds: u64,
    /// Maximum number of attempts, including the first delivery.
    pub max_attempts: u32,
    /// Minimum full-jitter backoff in seconds.
    pub backoff_min_seconds: u64,
    /// Maximum full-jitter backoff in seconds.
    pub backoff_max_seconds: u64,
    /// Consecutive failures required to open a destination circuit.
    pub circuit_failure_threshold: u32,
    /// Duration for which an open destination circuit rejects delivery.
    pub circuit_open_seconds: u64,
    /// Number of days dead-letter records are retained.
    pub dlq_retention_days: u32,
    /// Per-request timeout for the synchronous pre-commit validation hook, in
    /// milliseconds (ADR-025). Distinct from `delivery_timeout_seconds`
    /// (webhook delivery, 5s default): the hook sits on a user-facing
    /// request path and must fail closed quickly.
    pub pre_commit_validation_timeout_ms: u64,
    /// Process-wide maximum number of concurrent pre-commit validation
    /// requests in flight (ADR-025).
    pub pre_commit_validation_max_concurrency: u32,
    /// Optional PEM or DER CA file used only by the pre-commit HTTPS hook.
    /// Empty means the platform trust store is used unchanged.
    pub pre_commit_additional_ca_path: String,
    /// Test-only escape hatch that allows the pre-commit validation hook to
    /// call a loopback/private endpoint, bypassing the egress policy that
    /// otherwise applies to both webhook delivery and this hook (ADR-007,
    /// ADR-025). Must default to `false`; production deployments must not
    /// set this to `true`.
    pub allow_loopback_endpoints: bool,
}

/// Interest management settings.
#[derive(Debug, Clone, PartialEq)]
pub struct InterestConfig {
    /// Spatial grid cell size.
    pub cell_size: f64,
    /// Radius at which an entity becomes visible.
    pub near_radius: f64,
    /// Radius at which an entity is unsubscribed. Must exceed `near_radius`.
    pub unsubscribe_radius: f64,
}

/// Observability settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservabilityConfig {
    /// Log rendering.
    pub log_format: LogFormat,
    /// Minimum log severity.
    pub log_level: LogLevel,
    /// Declared audit-event retention period for operational enforcement.
    pub audit_retention_days: u32,
}

/// The validated server configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// HTTP listener settings.
    pub server: ServerConfig,
    /// Browser cross-origin request policy.
    pub cors: CorsConfig,
    /// Database settings.
    pub database: DatabaseConfig,
    /// Rate limit settings.
    pub rate_limit: RateLimitConfig,
    /// Authentication settings.
    pub auth: AuthConfig,
    /// Realtime connection settings.
    pub realtime: RealtimeConfig,
    /// Identity import limits.
    pub identity: IdentityConfig,
    /// Retention timing.
    pub retention: RetentionConfig,
    /// Extension webhook delivery settings.
    pub extensions: ExtensionsConfig,
    /// World runtime settings.
    pub world: WorldConfig,
    /// Interest management settings.
    pub interest: InterestConfig,
    /// Observability settings.
    pub observability: ObservabilityConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "0.0.0.0:8080".to_owned(),
                trusted_proxies: String::new(),
                worker_threads: 0,
                // The largest body is the 1 MiB CSV import; 2 MiB matches axum's
                // current default while leaving room for request framing.
                max_request_body_bytes: 2_097_152,
                // Keep the overall request deadline aligned with the legacy
                // 30-second body-read deadline unless operators override it.
                request_timeout_seconds: 30,
                page_default_limit: 50,
                page_max_limit: 200,
                shutdown_drain_timeout_seconds: 30,
                shutdown_force_timeout_seconds: 10,
            },
            cors: CorsConfig {
                allowed_origins: Vec::new(),
                allow_credentials: false,
            },
            database: DatabaseConfig {
                url_env: "DATABASE_URL".to_owned(),
                max_connections: 20,
                acquire_timeout_seconds: 2,
                readiness_timeout_seconds: 3,
            },
            auth: AuthConfig {
                access_token_ttl_seconds: 900,
                refresh_token_ttl_seconds: 2_592_000,
                password_min_length: 12,
                token_signing_key_env: "ORBISYNC_TOKEN_SIGNING_KEY".to_owned(),
                pagination_hmac_key_env: "ORBISYNC_PAGINATION_HMAC_KEY".to_owned(),
                refresh_token_hmac_key_env: "ORBISYNC_REFRESH_TOKEN_HMAC_KEY".to_owned(),
                realtime_ticket_hmac_key_env: "ORBISYNC_REALTIME_TICKET_HMAC_KEY".to_owned(),
                idempotency_hmac_key_env: "ORBISYNC_IDEMPOTENCY_HMAC_KEY".to_owned(),
                allow_stub_bearer: false,
                argon2_memory_cost_kib: 65_536,
                argon2_iterations: 3,
                argon2_parallelism: 1,
                password_hash_concurrency: 4,
                login_failure_threshold: 5,
                lockout_duration_seconds: 900,
                // ADR-026: local only, so an existing deployment is unaffected.
                methods: vec!["local".to_owned()],
                guest: EphemeralMethodConfig::default_for("Guest"),
                name_only: EphemeralMethodConfig::default_for("Guest"),
                external: ExternalAuthConfig::disabled(),
            },
            rate_limit: RateLimitConfig {
                normal_per_sec: 100,
                custom_per_sec: 10,
                persistent_threshold: 5,
                max_buckets: 10_000,
                login_per_ip_per_minute: 10,
                login_ip_block_seconds: 300,
                login_ip_max_buckets: 10_000,
                realtime_ticket_per_interval: 10,
            },
            realtime: RealtimeConfig {
                heartbeat_interval_seconds: 20,
                connection_timeout_seconds: 60,
                max_normal_message_bytes: 16_384,
                max_message_bytes: 65_536,
                outbound_queue_capacity: 256,
                allow_stub_ticket: false,
                handshake_timeout_ms: 5_000,
                max_connections: 1_024,
                per_connection_capacity: 256,
                resume_prune_interval_seconds: 15,
            },
            world: WorldConfig {
                default_capacity: 100,
                server_tick_hz: 10,
                resume_grace_seconds: 60,
                checkpoint_interval_secs: 300,
                checkpoint_interval_ticks: 300,
                checkpoint_chunk_bytes: 262_144,
                checkpoint_generation_enabled: false,
                checkpoint_writer_lock: String::new(),
                checkpoint_deployment: String::new(),
                checkpoint_max_serialized_bytes: 8_388_608,
                history_capacity: 256,
                max_speed: 50.0,
                max_acceleration: 10.0,
                speed_acceleration_check_enabled: true,
                // Match rate_limit.custom_per_sec: both are high-cost,
                // user-defined updates and should not outpace the 10 Hz tick.
                component_updates_per_sec: 10,
                // Control is only used during joins/leaves; transform and entity
                // match the existing realtime queue capacity for burst headroom.
                mailbox_control_capacity: 64,
                mailbox_transform_capacity: 256,
                mailbox_entity_capacity: 256,
            },
            identity: IdentityConfig {
                csv_max_bytes: 1_048_576,
                csv_max_rows: 1_000,
            },
            retention: RetentionConfig {
                realtime_ticket_interval_seconds: 60,
                idempotency_interval_seconds: 3_600,
                drain_budget_seconds: 20,
                restart_backoff_seconds: 5,
                extension_outbox_days: 1,
                source_ip_days: 365,
            },
            extensions: ExtensionsConfig {
                max_concurrency: 16,
                delivery_timeout_seconds: 5,
                max_attempts: 5,
                backoff_min_seconds: 1,
                backoff_max_seconds: 30,
                circuit_failure_threshold: 5,
                circuit_open_seconds: 30,
                dlq_retention_days: 7,
                pre_commit_validation_timeout_ms: 500,
                pre_commit_validation_max_concurrency: 16,
                pre_commit_additional_ca_path: String::new(),
                allow_loopback_endpoints: false,
            },
            interest: InterestConfig {
                cell_size: 20.0,
                near_radius: 30.0,
                unsubscribe_radius: 35.0,
            },
            observability: ObservabilityConfig {
                log_format: LogFormat::Json,
                log_level: LogLevel::Info,
                audit_retention_days: 365,
            },
        }
    }
}

impl Config {
    /// Applies one dotted key with its raw textual value.
    ///
    /// File values, environment variables and CLI arguments all funnel through
    /// this function, so the schema and the type checks are defined once.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigErrorKind::UnknownKey`] for keys outside
    /// [`CONFIG_KEYS`](crate::CONFIG_KEYS) and
    /// [`ConfigErrorKind::InvalidValue`] when the value cannot be parsed.
    pub fn apply(&mut self, key: &str, raw: &str) -> Result<(), ConfigError> {
        match key {
            "server.bind" => self.server.bind = raw.to_owned(),
            "server.trusted_proxies" => self.server.trusted_proxies = raw.to_owned(),
            "server.worker_threads" => self.server.worker_threads = parse(key, raw)?,
            "server.max_request_body_bytes" => {
                self.server.max_request_body_bytes = parse(key, raw)?
            }
            "server.request_timeout_seconds" => {
                self.server.request_timeout_seconds = parse(key, raw)?
            }
            "server.page_default_limit" => self.server.page_default_limit = parse(key, raw)?,
            "server.page_max_limit" => self.server.page_max_limit = parse(key, raw)?,
            "server.shutdown_drain_timeout_seconds" => {
                self.server.shutdown_drain_timeout_seconds = parse(key, raw)?
            }
            "server.shutdown_force_timeout_seconds" => {
                self.server.shutdown_force_timeout_seconds = parse(key, raw)?
            }
            "cors.allowed_origins" => self.cors.allowed_origins = parse_list(key, raw)?,
            "cors.allow_credentials" => self.cors.allow_credentials = parse(key, raw)?,
            "database.url_env" => self.database.url_env = raw.to_owned(),
            "database.max_connections" => self.database.max_connections = parse(key, raw)?,
            "database.acquire_timeout_seconds" => {
                self.database.acquire_timeout_seconds = parse(key, raw)?
            }
            "database.readiness_timeout_seconds" => {
                self.database.readiness_timeout_seconds = parse(key, raw)?
            }
            "auth.access_token_ttl_seconds" => {
                self.auth.access_token_ttl_seconds = parse(key, raw)?
            }
            "auth.refresh_token_ttl_seconds" => {
                self.auth.refresh_token_ttl_seconds = parse(key, raw)?;
            }
            "auth.password_min_length" => self.auth.password_min_length = parse(key, raw)?,
            "auth.token_signing_key_env" => self.auth.token_signing_key_env = raw.to_owned(),
            "auth.pagination_hmac_key_env" => {
                self.auth.pagination_hmac_key_env = raw.to_owned();
            }
            "auth.refresh_token_hmac_key_env" => {
                self.auth.refresh_token_hmac_key_env = raw.to_owned();
            }
            "auth.realtime_ticket_hmac_key_env" => {
                self.auth.realtime_ticket_hmac_key_env = raw.to_owned();
            }
            "auth.idempotency_hmac_key_env" => {
                self.auth.idempotency_hmac_key_env = raw.to_owned();
            }
            "auth.allow_stub_bearer" => self.auth.allow_stub_bearer = parse(key, raw)?,
            "auth.argon2_memory_cost_kib" => self.auth.argon2_memory_cost_kib = parse(key, raw)?,
            "auth.argon2_iterations" => self.auth.argon2_iterations = parse(key, raw)?,
            "auth.argon2_parallelism" => self.auth.argon2_parallelism = parse(key, raw)?,
            "auth.password_hash_concurrency" => {
                self.auth.password_hash_concurrency = parse(key, raw)?
            }
            "auth.login_failure_threshold" => self.auth.login_failure_threshold = parse(key, raw)?,
            "auth.lockout_duration_seconds" => {
                self.auth.lockout_duration_seconds = parse(key, raw)?
            }
            "auth.methods" => self.auth.methods = parse_list(key, raw)?,
            "auth.guest.role_names" => self.auth.guest.role_names = parse_list(key, raw)?,
            "auth.guest.session_ttl_seconds" => {
                self.auth.guest.session_ttl_seconds = parse(key, raw)?
            }
            "auth.guest.retention_seconds" => self.auth.guest.retention_seconds = parse(key, raw)?,
            "auth.guest.allowed_worlds" => self.auth.guest.allowed_worlds = parse_list(key, raw)?,
            "auth.guest.display_name_prefix" => {
                self.auth.guest.display_name_prefix = raw.to_owned();
            }
            "auth.name_only.role_names" => self.auth.name_only.role_names = parse_list(key, raw)?,
            "auth.name_only.session_ttl_seconds" => {
                self.auth.name_only.session_ttl_seconds = parse(key, raw)?
            }
            "auth.name_only.retention_seconds" => {
                self.auth.name_only.retention_seconds = parse(key, raw)?
            }
            "auth.name_only.allowed_worlds" => {
                self.auth.name_only.allowed_worlds = parse_list(key, raw)?;
            }
            "auth.name_only.display_name_prefix" => {
                self.auth.name_only.display_name_prefix = raw.to_owned();
            }
            "auth.external.issuer" => self.auth.external.issuer = raw.to_owned(),
            "auth.external.audience" => self.auth.external.audience = raw.to_owned(),
            "auth.external.algorithm" => self.auth.external.algorithm = raw.to_owned(),
            "auth.external.jwks_path" => self.auth.external.jwks_path = raw.to_owned(),
            "auth.external.leeway_seconds" => self.auth.external.leeway_seconds = parse(key, raw)?,
            "auth.external.role_names" => self.auth.external.role_names = parse_list(key, raw)?,
            "rate_limit.normal_per_sec" => self.rate_limit.normal_per_sec = parse(key, raw)?,
            "rate_limit.custom_per_sec" => self.rate_limit.custom_per_sec = parse(key, raw)?,
            "rate_limit.persistent_threshold" => {
                self.rate_limit.persistent_threshold = parse(key, raw)?
            }
            "rate_limit.max_buckets" => self.rate_limit.max_buckets = parse(key, raw)?,
            "rate_limit.login_per_ip_per_minute" => {
                self.rate_limit.login_per_ip_per_minute = parse(key, raw)?
            }
            "rate_limit.login_ip_block_seconds" => {
                self.rate_limit.login_ip_block_seconds = parse(key, raw)?
            }
            "rate_limit.login_ip_max_buckets" => {
                self.rate_limit.login_ip_max_buckets = parse(key, raw)?
            }
            "rate_limit.realtime_ticket_per_interval" => {
                self.rate_limit.realtime_ticket_per_interval = parse(key, raw)?
            }
            "realtime.heartbeat_interval_seconds" => {
                self.realtime.heartbeat_interval_seconds = parse(key, raw)?;
            }
            "realtime.connection_timeout_seconds" => {
                self.realtime.connection_timeout_seconds = parse(key, raw)?;
            }
            "realtime.max_normal_message_bytes" => {
                self.realtime.max_normal_message_bytes = parse(key, raw)?
            }
            "realtime.max_message_bytes" => self.realtime.max_message_bytes = parse(key, raw)?,
            "realtime.outbound_queue_capacity" => {
                self.realtime.outbound_queue_capacity = parse(key, raw)?;
            }
            "realtime.allow_stub_ticket" => self.realtime.allow_stub_ticket = parse(key, raw)?,
            "realtime.handshake_timeout_ms" => {
                self.realtime.handshake_timeout_ms = parse(key, raw)?;
            }
            "realtime.max_connections" => self.realtime.max_connections = parse(key, raw)?,
            "realtime.per_connection_capacity" => {
                self.realtime.per_connection_capacity = parse(key, raw)?
            }
            "realtime.resume_prune_interval_seconds" => {
                self.realtime.resume_prune_interval_seconds = parse(key, raw)?
            }
            "world.default_capacity" => self.world.default_capacity = parse(key, raw)?,
            "world.server_tick_hz" => self.world.server_tick_hz = parse(key, raw)?,
            "world.resume_grace_seconds" => self.world.resume_grace_seconds = parse(key, raw)?,
            "world.checkpoint_interval_secs" => {
                self.world.checkpoint_interval_secs = parse(key, raw)?
            }
            "world.checkpoint_generation_enabled" => {
                self.world.checkpoint_generation_enabled = parse(key, raw)?;
            }
            "world.checkpoint_writer_lock" => {
                self.world.checkpoint_writer_lock = raw.to_owned();
            }
            "world.checkpoint_deployment" => {
                self.world.checkpoint_deployment = raw.to_owned();
            }
            "world.checkpoint_chunk_bytes" => {
                self.world.checkpoint_chunk_bytes = parse(key, raw)?;
            }
            "world.checkpoint_max_serialized_bytes" => {
                self.world.checkpoint_max_serialized_bytes = parse(key, raw)?;
            }
            "world.checkpoint_interval_ticks" => {
                self.world.checkpoint_interval_ticks = parse(key, raw)?
            }
            "world.history_capacity" => self.world.history_capacity = parse(key, raw)?,
            "world.max_speed" => self.world.max_speed = parse(key, raw)?,
            "world.max_acceleration" => self.world.max_acceleration = parse(key, raw)?,
            "world.speed_acceleration_check_enabled" => {
                self.world.speed_acceleration_check_enabled = parse(key, raw)?
            }
            "world.component_updates_per_sec" => {
                self.world.component_updates_per_sec = parse(key, raw)?
            }
            "world.mailbox_control_capacity" => {
                self.world.mailbox_control_capacity = parse(key, raw)?
            }
            "world.mailbox_transform_capacity" => {
                self.world.mailbox_transform_capacity = parse(key, raw)?
            }
            "world.mailbox_entity_capacity" => {
                self.world.mailbox_entity_capacity = parse(key, raw)?
            }
            "identity.csv_max_bytes" => self.identity.csv_max_bytes = parse(key, raw)?,
            "identity.csv_max_rows" => self.identity.csv_max_rows = parse(key, raw)?,
            "retention.realtime_ticket_interval_seconds" => {
                self.retention.realtime_ticket_interval_seconds = parse(key, raw)?
            }
            "retention.idempotency_interval_seconds" => {
                self.retention.idempotency_interval_seconds = parse(key, raw)?
            }
            "retention.drain_budget_seconds" => {
                self.retention.drain_budget_seconds = parse(key, raw)?
            }
            "retention.restart_backoff_seconds" => {
                self.retention.restart_backoff_seconds = parse(key, raw)?
            }
            "retention.extension_outbox_days" => {
                self.retention.extension_outbox_days = parse(key, raw)?
            }
            "retention.source_ip_days" => self.retention.source_ip_days = parse(key, raw)?,
            "extensions.delivery_timeout_seconds" => {
                self.extensions.delivery_timeout_seconds = parse(key, raw)?
            }
            "extensions.max_concurrency" => self.extensions.max_concurrency = parse(key, raw)?,
            "extensions.max_attempts" => self.extensions.max_attempts = parse(key, raw)?,
            "extensions.backoff_min_seconds" => {
                self.extensions.backoff_min_seconds = parse(key, raw)?
            }
            "extensions.backoff_max_seconds" => {
                self.extensions.backoff_max_seconds = parse(key, raw)?
            }
            "extensions.circuit_failure_threshold" => {
                self.extensions.circuit_failure_threshold = parse(key, raw)?
            }
            "extensions.circuit_open_seconds" => {
                self.extensions.circuit_open_seconds = parse(key, raw)?
            }
            "extensions.dlq_retention_days" => {
                self.extensions.dlq_retention_days = parse(key, raw)?
            }
            "extensions.pre_commit_validation_timeout_ms" => {
                self.extensions.pre_commit_validation_timeout_ms = parse(key, raw)?
            }
            "extensions.pre_commit_validation_max_concurrency" => {
                self.extensions.pre_commit_validation_max_concurrency = parse(key, raw)?
            }
            "extensions.pre_commit_additional_ca_path" => {
                self.extensions.pre_commit_additional_ca_path = parse(key, raw)?
            }
            "extensions.allow_loopback_endpoints" => {
                self.extensions.allow_loopback_endpoints = parse(key, raw)?
            }
            "interest.cell_size" => self.interest.cell_size = parse(key, raw)?,
            "interest.near_radius" => self.interest.near_radius = parse(key, raw)?,
            "interest.unsubscribe_radius" => self.interest.unsubscribe_radius = parse(key, raw)?,
            "observability.log_format" => self.observability.log_format = parse(key, raw)?,
            "observability.log_level" => self.observability.log_level = parse(key, raw)?,
            "observability.audit_retention_days" => {
                self.observability.audit_retention_days = parse(key, raw)?
            }
            _ => {
                return Err(ConfigError::new(
                    ConfigErrorKind::UnknownKey,
                    key,
                    "key is not part of the configuration schema",
                ));
            }
        }
        Ok(())
    }

    /// Validates ranges and cross field consistency
    /// (`observability-and-config.md` §6.2).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigErrorKind::InvalidValue`] or
    /// [`ConfigErrorKind::Inconsistent`] describing the first failing check.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.bind.parse::<std::net::SocketAddr>().is_err() {
            return Err(invalid(
                "server.bind",
                "must be a socket address such as 0.0.0.0:8080",
            ));
        }
        // P2-C4 / ADR-009: parse the allowlist as typed networks at startup.
        // This also strictly validates the family-specific prefix width.
        parse_trusted_proxies(&self.server.trusted_proxies)?;
        if self.database.url_env.is_empty() {
            return Err(invalid(
                "database.url_env",
                "must name the environment variable holding the connection string",
            ));
        }
        if self.database.max_connections < 1 {
            return Err(invalid("database.max_connections", "must be at least 1"));
        }
        if self.server.worker_threads > 256 {
            return Err(invalid(
                "server.worker_threads",
                "must be 0 or between 1 and 256",
            ));
        }
        if self.server.max_request_body_bytes == 0 {
            return Err(invalid(
                "server.max_request_body_bytes",
                "must be at least 1",
            ));
        }
        if self.server.request_timeout_seconds == 0 {
            return Err(invalid(
                "server.request_timeout_seconds",
                "must be at least 1",
            ));
        }
        if self.server.page_default_limit == 0 {
            return Err(invalid("server.page_default_limit", "must be at least 1"));
        }
        if self.server.page_max_limit == 0 || self.server.page_max_limit > 200 {
            return Err(invalid(
                "server.page_max_limit",
                "must be between 1 and 200",
            ));
        }
        if self.server.page_default_limit > self.server.page_max_limit {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "server.page_default_limit",
                "must not exceed server.page_max_limit",
            ));
        }
        if self.server.shutdown_drain_timeout_seconds == 0 {
            return Err(invalid(
                "server.shutdown_drain_timeout_seconds",
                "must be at least 1",
            ));
        }
        if self.server.shutdown_force_timeout_seconds == 0 {
            return Err(invalid(
                "server.shutdown_force_timeout_seconds",
                "must be at least 1",
            ));
        }
        for origin in &self.cors.allowed_origins {
            if origin.trim().is_empty() {
                return Err(invalid("cors.allowed_origins", "origins must not be empty"));
            }
            if origin.trim() == "*" {
                return Err(invalid(
                    "cors.allowed_origins",
                    "wildcard origin is forbidden; enumerate origins explicitly",
                ));
            }
            if origin.chars().any(char::is_control) {
                return Err(invalid(
                    "cors.allowed_origins",
                    "origins must be valid HTTP header values",
                ));
            }
        }
        if self.cors.allow_credentials && self.cors.allowed_origins.is_empty() {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "cors.allow_credentials",
                "requires at least one explicitly allowed origin",
            ));
        }
        if self.database.acquire_timeout_seconds == 0 {
            return Err(invalid(
                "database.acquire_timeout_seconds",
                "must be at least 1",
            ));
        }
        if self.database.readiness_timeout_seconds == 0 {
            return Err(invalid(
                "database.readiness_timeout_seconds",
                "must be at least 1",
            ));
        }
        if !(60..=86_400).contains(&self.auth.access_token_ttl_seconds) {
            return Err(invalid(
                "auth.access_token_ttl_seconds",
                "must be between 60 and 86400",
            ));
        }
        if self.auth.refresh_token_ttl_seconds <= self.auth.access_token_ttl_seconds {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "auth.refresh_token_ttl_seconds",
                "must exceed auth.access_token_ttl_seconds",
            ));
        }
        if self.auth.password_min_length < 8 {
            return Err(invalid("auth.password_min_length", "must be at least 8"));
        }
        if self.auth.token_signing_key_env.is_empty() {
            return Err(invalid(
                "auth.token_signing_key_env",
                "must name the environment variable holding the signing key",
            ));
        }
        if self.auth.pagination_hmac_key_env.is_empty() {
            return Err(invalid(
                "auth.pagination_hmac_key_env",
                "must name the environment variable holding the pagination HMAC key",
            ));
        }
        if self.auth.refresh_token_hmac_key_env.is_empty() {
            return Err(invalid(
                "auth.refresh_token_hmac_key_env",
                "must name the environment variable holding the refresh token HMAC key",
            ));
        }
        if self.auth.realtime_ticket_hmac_key_env.is_empty() {
            return Err(invalid(
                "auth.realtime_ticket_hmac_key_env",
                "must name the environment variable holding the realtime ticket HMAC key",
            ));
        }
        if self.auth.idempotency_hmac_key_env.is_empty() {
            return Err(invalid(
                "auth.idempotency_hmac_key_env",
                "must name the environment variable holding the idempotency HMAC key",
            ));
        }
        if self.auth.argon2_memory_cost_kib == 0 {
            return Err(invalid("auth.argon2_memory_cost_kib", "must be at least 1"));
        }
        if self.auth.argon2_iterations == 0 {
            return Err(invalid("auth.argon2_iterations", "must be at least 1"));
        }
        if self.auth.argon2_parallelism == 0 {
            return Err(invalid("auth.argon2_parallelism", "must be at least 1"));
        }
        if self.auth.password_hash_concurrency == 0 {
            return Err(invalid(
                "auth.password_hash_concurrency",
                "must be at least 1",
            ));
        }
        if self.auth.login_failure_threshold == 0 {
            return Err(invalid(
                "auth.login_failure_threshold",
                "must be at least 1",
            ));
        }
        if self.auth.lockout_duration_seconds == 0 {
            return Err(invalid(
                "auth.lockout_duration_seconds",
                "must be at least 1",
            ));
        }
        self.validate_auth_methods()?;
        if self.rate_limit.normal_per_sec == 0 {
            return Err(invalid("rate_limit.normal_per_sec", "must be at least 1"));
        }
        if self.rate_limit.custom_per_sec == 0 {
            return Err(invalid("rate_limit.custom_per_sec", "must be at least 1"));
        }
        if self.world.component_updates_per_sec == 0 {
            return Err(invalid(
                "world.component_updates_per_sec",
                "must be at least 1",
            ));
        }
        if self.rate_limit.persistent_threshold == 0 {
            return Err(invalid(
                "rate_limit.persistent_threshold",
                "must be at least 1",
            ));
        }
        if self.rate_limit.max_buckets == 0 {
            return Err(invalid("rate_limit.max_buckets", "must be at least 1"));
        }
        if self.rate_limit.login_per_ip_per_minute == 0 {
            return Err(invalid(
                "rate_limit.login_per_ip_per_minute",
                "must be at least 1",
            ));
        }
        if self.rate_limit.login_ip_block_seconds == 0 {
            return Err(invalid(
                "rate_limit.login_ip_block_seconds",
                "must be at least 1",
            ));
        }
        if self.rate_limit.login_ip_max_buckets == 0 {
            return Err(invalid(
                "rate_limit.login_ip_max_buckets",
                "must be at least 1",
            ));
        }
        if self.rate_limit.realtime_ticket_per_interval == 0 {
            return Err(invalid(
                "rate_limit.realtime_ticket_per_interval",
                "must be at least 1",
            ));
        }
        if !(5..=120).contains(&self.realtime.heartbeat_interval_seconds) {
            return Err(invalid(
                "realtime.heartbeat_interval_seconds",
                "must be between 5 and 120",
            ));
        }
        if self.realtime.connection_timeout_seconds <= self.realtime.heartbeat_interval_seconds {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "realtime.connection_timeout_seconds",
                "must exceed realtime.heartbeat_interval_seconds",
            ));
        }
        if !(1_024..=1_048_576).contains(&self.realtime.max_message_bytes) {
            return Err(invalid(
                "realtime.max_message_bytes",
                "must be between 1024 and 1048576",
            ));
        }
        if !(1_024..=1_048_576).contains(&self.realtime.max_normal_message_bytes) {
            return Err(invalid(
                "realtime.max_normal_message_bytes",
                "must be between 1024 and 1048576",
            ));
        }
        if self.realtime.max_normal_message_bytes > self.realtime.max_message_bytes {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "realtime.max_normal_message_bytes",
                "must not exceed realtime.max_message_bytes",
            ));
        }
        if self.realtime.outbound_queue_capacity < 1 {
            return Err(invalid(
                "realtime.outbound_queue_capacity",
                "must be at least 1; unbounded queues are forbidden (TD-02)",
            ));
        }
        if !(500..=30_000).contains(&self.realtime.handshake_timeout_ms) {
            return Err(invalid(
                "realtime.handshake_timeout_ms",
                "must be between 500 and 30000",
            ));
        }
        if self.realtime.max_connections < 1 {
            return Err(invalid("realtime.max_connections", "must be at least 1"));
        }
        if self.realtime.per_connection_capacity == 0 {
            return Err(invalid(
                "realtime.per_connection_capacity",
                "must be at least 1",
            ));
        }
        if self.realtime.resume_prune_interval_seconds == 0 {
            return Err(invalid(
                "realtime.resume_prune_interval_seconds",
                "must be greater than 0",
            ));
        }
        if self.realtime.resume_prune_interval_seconds >= self.world.resume_grace_seconds {
            return Err(invalid(
                "realtime.resume_prune_interval_seconds",
                "must be less than world.resume_grace_seconds",
            ));
        }
        if self.world.default_capacity < 1 {
            return Err(invalid("world.default_capacity", "must be at least 1"));
        }
        if !(1..=60).contains(&self.world.server_tick_hz) {
            return Err(invalid("world.server_tick_hz", "must be between 1 and 60"));
        }
        if !(60..=3600).contains(&self.world.checkpoint_interval_secs) {
            return Err(invalid(
                "world.checkpoint_interval_secs",
                "must be between 60 and 3600",
            ));
        }
        if self.world.checkpoint_generation_enabled
            && (self.world.checkpoint_writer_lock.is_empty()
                || self.world.checkpoint_deployment.is_empty())
        {
            return Err(invalid(
                "world.checkpoint_generation_enabled",
                "requires stable writer lock and approved deployment identity",
            ));
        }
        if !(16_384..=1_048_576).contains(&self.world.checkpoint_chunk_bytes) {
            return Err(invalid(
                "world.checkpoint_chunk_bytes",
                "must be 16384 through 1048576",
            ));
        }
        if !(2_097_152..=67_108_864).contains(&self.world.checkpoint_max_serialized_bytes) {
            return Err(invalid(
                "world.checkpoint_max_serialized_bytes",
                "must be 2097152 through 67108864",
            ));
        }
        if self.world.checkpoint_chunk_bytes > self.world.checkpoint_max_serialized_bytes {
            return Err(invalid(
                "world.checkpoint_chunk_bytes",
                "must not exceed total",
            ));
        }
        if self.world.checkpoint_interval_ticks == 0 {
            return Err(invalid(
                "world.checkpoint_interval_ticks",
                "must be at least 1",
            ));
        }
        if self.world.history_capacity == 0 {
            return Err(invalid("world.history_capacity", "must be at least 1"));
        }
        if !self.world.max_speed.is_finite() || self.world.max_speed <= 0.0 {
            return Err(invalid(
                "world.max_speed",
                "must be finite and greater than 0",
            ));
        }
        if !self.world.max_acceleration.is_finite() || self.world.max_acceleration <= 0.0 {
            return Err(invalid(
                "world.max_acceleration",
                "must be finite and greater than 0",
            ));
        }
        if self.world.mailbox_control_capacity == 0 {
            return Err(invalid(
                "world.mailbox_control_capacity",
                "must be at least 1",
            ));
        }
        if self.world.mailbox_transform_capacity == 0 {
            return Err(invalid(
                "world.mailbox_transform_capacity",
                "must be at least 1",
            ));
        }
        if self.world.mailbox_entity_capacity == 0 {
            return Err(invalid(
                "world.mailbox_entity_capacity",
                "must be at least 1",
            ));
        }
        if self.identity.csv_max_bytes == 0 {
            return Err(invalid("identity.csv_max_bytes", "must be at least 1"));
        }
        if self.identity.csv_max_rows == 0 {
            return Err(invalid("identity.csv_max_rows", "must be at least 1"));
        }
        if self.retention.realtime_ticket_interval_seconds == 0 {
            return Err(invalid(
                "retention.realtime_ticket_interval_seconds",
                "must be at least 1",
            ));
        }
        if self.retention.idempotency_interval_seconds == 0 {
            return Err(invalid(
                "retention.idempotency_interval_seconds",
                "must be at least 1",
            ));
        }
        if self.retention.drain_budget_seconds == 0 {
            return Err(invalid(
                "retention.drain_budget_seconds",
                "must be at least 1",
            ));
        }
        if self.retention.restart_backoff_seconds == 0 {
            return Err(invalid(
                "retention.restart_backoff_seconds",
                "must be at least 1",
            ));
        }
        if self.retention.extension_outbox_days == 0 {
            return Err(invalid(
                "retention.extension_outbox_days",
                "must be at least 1",
            ));
        }
        if self.retention.source_ip_days == 0 {
            return Err(invalid("retention.source_ip_days", "must be at least 1"));
        }
        if self.extensions.max_concurrency == 0 {
            return Err(invalid("extensions.max_concurrency", "must be at least 1"));
        }
        if self.extensions.max_concurrency > 1024 {
            return Err(invalid(
                "extensions.max_concurrency",
                "must be at most 1024",
            ));
        }
        if self.extensions.delivery_timeout_seconds == 0 {
            return Err(invalid(
                "extensions.delivery_timeout_seconds",
                "must be at least 1",
            ));
        }
        if self.extensions.max_attempts == 0 {
            return Err(invalid("extensions.max_attempts", "must be at least 1"));
        }
        if self.extensions.max_attempts > i32::MAX as u32 {
            return Err(invalid(
                "extensions.max_attempts",
                "must fit the delivery persistence counter",
            ));
        }
        if self.extensions.backoff_min_seconds == 0
            || self.extensions.backoff_max_seconds < self.extensions.backoff_min_seconds
        {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "extensions.backoff_max_seconds",
                "must be at least extensions.backoff_min_seconds and both must be positive",
            ));
        }
        if self.extensions.circuit_failure_threshold == 0 {
            return Err(invalid(
                "extensions.circuit_failure_threshold",
                "must be at least 1",
            ));
        }
        if self.extensions.circuit_open_seconds == 0 {
            return Err(invalid(
                "extensions.circuit_open_seconds",
                "must be at least 1",
            ));
        }
        if self.extensions.circuit_open_seconds > i64::MAX as u64 / 1_000 {
            return Err(invalid(
                "extensions.circuit_open_seconds",
                "duration is too large",
            ));
        }
        if self.extensions.delivery_timeout_seconds > u64::from(u32::MAX) / 1_000 {
            return Err(invalid(
                "extensions.delivery_timeout_seconds",
                "duration does not fit milliseconds",
            ));
        }
        if self.extensions.backoff_min_seconds > i64::MAX as u64 / 1_000
            || self.extensions.backoff_max_seconds > i64::MAX as u64 / 1_000
        {
            return Err(invalid(
                "extensions.backoff_max_seconds",
                "duration is too large",
            ));
        }
        if self.extensions.dlq_retention_days == 0 {
            return Err(invalid(
                "extensions.dlq_retention_days",
                "must be at least 1",
            ));
        }
        if self.extensions.pre_commit_validation_timeout_ms == 0 {
            return Err(invalid(
                "extensions.pre_commit_validation_timeout_ms",
                "must be at least 1",
            ));
        }
        if self.extensions.pre_commit_validation_timeout_ms > i64::MAX as u64 {
            return Err(invalid(
                "extensions.pre_commit_validation_timeout_ms",
                "duration does not fit the persistence time type",
            ));
        }
        if self.extensions.pre_commit_validation_max_concurrency == 0
            || self.extensions.pre_commit_validation_max_concurrency > 1_024
        {
            return Err(invalid(
                "extensions.pre_commit_validation_max_concurrency",
                "must be in the range 1..=1024",
            ));
        }
        if !(1..=3_650).contains(&self.observability.audit_retention_days) {
            return Err(invalid(
                "observability.audit_retention_days",
                "must be in the range 1..=3650",
            ));
        }
        if !self.interest.cell_size.is_finite() || self.interest.cell_size <= 0.0 {
            return Err(invalid(
                "interest.cell_size",
                "must be a finite value greater than 0",
            ));
        }
        if !self.interest.near_radius.is_finite() || self.interest.near_radius <= 0.0 {
            return Err(invalid(
                "interest.near_radius",
                "must be a finite value greater than 0",
            ));
        }
        if !self.interest.unsubscribe_radius.is_finite()
            || self.interest.unsubscribe_radius <= self.interest.near_radius
        {
            return Err(ConfigError::new(
                ConfigErrorKind::Inconsistent,
                "interest.unsubscribe_radius",
                "must exceed interest.near_radius so visibility has hysteresis",
            ));
        }
        Ok(())
    }

    /// Returns the environment variable names that must carry a secret value.
    #[must_use]
    pub fn required_secret_env_vars(&self) -> Vec<&str> {
        vec![
            self.database.url_env.as_str(),
            self.auth.token_signing_key_env.as_str(),
            self.auth.pagination_hmac_key_env.as_str(),
            self.auth.refresh_token_hmac_key_env.as_str(),
            self.auth.realtime_ticket_hmac_key_env.as_str(),
            self.auth.idempotency_hmac_key_env.as_str(),
            "ORBISYNC_EXTENSION_TOKEN_HMAC_KEY",
        ]
    }
}

/// Authentication method names accepted in `auth.methods` (ADR-026 §2).
pub const AUTH_METHOD_LOCAL: &str = "local";
/// Guest participation, see [`AUTH_METHOD_LOCAL`].
pub const AUTH_METHOD_GUEST: &str = "guest";
/// Name-only participation, see [`AUTH_METHOD_LOCAL`].
pub const AUTH_METHOD_NAME_ONLY: &str = "name_only";
/// External signed-JWT authentication, see [`AUTH_METHOD_LOCAL`].
pub const AUTH_METHOD_EXTERNAL: &str = "external";

/// Every recognised value of `auth.methods`.
pub const AUTH_METHODS: &[&str] = &[
    AUTH_METHOD_LOCAL,
    AUTH_METHOD_GUEST,
    AUTH_METHOD_NAME_ONLY,
    AUTH_METHOD_EXTERNAL,
];

/// Signature algorithms accepted for external tokens.
///
/// `none` is absent by construction, so an unsigned token can never be
/// configured as acceptable.
pub const EXTERNAL_AUTH_ALGORITHMS: &[&str] = &["EdDSA", "RS256", "ES256"];

impl Config {
    /// Validates the ADR-026 authentication method selection.
    ///
    /// Enabling a method without the settings it needs fails startup rather
    /// than running in a half-configured state: a temporary subject with no
    /// role is denied everything, and one with no world boundary could join
    /// anywhere, so neither empty value is treated as "no restriction".
    fn validate_auth_methods(&self) -> Result<(), ConfigError> {
        if self.auth.methods.is_empty() {
            return Err(invalid("auth.methods", "must enable at least one method"));
        }
        for method in &self.auth.methods {
            if !AUTH_METHODS.contains(&method.as_str()) {
                return Err(invalid(
                    "auth.methods",
                    format!(
                        "`{method}` is not a known method (expected one of {})",
                        AUTH_METHODS.join(", ")
                    ),
                ));
            }
        }
        self.validate_ephemeral_method(AUTH_METHOD_GUEST, &self.auth.guest)?;
        self.validate_ephemeral_method(AUTH_METHOD_NAME_ONLY, &self.auth.name_only)?;
        self.validate_external_method()
    }

    fn validate_ephemeral_method(
        &self,
        method: &str,
        settings: &EphemeralMethodConfig,
    ) -> Result<(), ConfigError> {
        if !self.auth.methods.iter().any(|value| value == method) {
            return Ok(());
        }
        if settings.role_names.is_empty() {
            return Err(invalid(
                format!("auth.{method}.role_names"),
                "must name at least one role while the method is enabled",
            ));
        }
        if settings.allowed_worlds.is_empty() {
            return Err(invalid(
                format!("auth.{method}.allowed_worlds"),
                "must name at least one world while the method is enabled",
            ));
        }
        if settings.session_ttl_seconds == 0 {
            return Err(invalid(
                format!("auth.{method}.session_ttl_seconds"),
                "must be at least 1",
            ));
        }
        if settings.display_name_prefix.trim().is_empty() {
            return Err(invalid(
                format!("auth.{method}.display_name_prefix"),
                "must not be blank",
            ));
        }
        Ok(())
    }

    fn validate_external_method(&self) -> Result<(), ConfigError> {
        if !self
            .auth
            .methods
            .iter()
            .any(|value| value == AUTH_METHOD_EXTERNAL)
        {
            return Ok(());
        }
        let external = &self.auth.external;
        if external.issuer.trim().is_empty() {
            return Err(invalid(
                "auth.external.issuer",
                "must name the accepted token issuer while external auth is enabled",
            ));
        }
        if external.audience.trim().is_empty() {
            return Err(invalid(
                "auth.external.audience",
                "must name the accepted audience while external auth is enabled",
            ));
        }
        if external.jwks_path.trim().is_empty() {
            return Err(invalid(
                "auth.external.jwks_path",
                "must point at the static JWKS file while external auth is enabled",
            ));
        }
        if !EXTERNAL_AUTH_ALGORITHMS.contains(&external.algorithm.as_str()) {
            return Err(invalid(
                "auth.external.algorithm",
                format!(
                    "`{}` is not accepted (expected one of {})",
                    external.algorithm,
                    EXTERNAL_AUTH_ALGORITHMS.join(", ")
                ),
            ));
        }
        if external.role_names.is_empty() {
            return Err(invalid(
                "auth.external.role_names",
                "must name at least one role while external auth is enabled",
            ));
        }
        Ok(())
    }
}

fn invalid(key: impl Into<String>, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorKind::InvalidValue, key, detail)
}

fn parse_list(key: &str, raw: &str) -> Result<Vec<String>, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let document = format!("value = {trimmed}");
        let table: toml::Table = document
            .parse()
            .map_err(|_| invalid(key, "must be a TOML array of strings"))?;
        let Some(toml::Value::Array(values)) = table.get("value") else {
            return Err(invalid(key, "must be a TOML array of strings"));
        };
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid(key, "must be a TOML array of strings"))
            })
            .collect()
    } else {
        Ok(trimmed
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

fn parse<T>(key: &str, raw: &str) -> Result<T, ConfigError>
where
    T: FromStr,
{
    raw.parse::<T>()
        .map_err(|_| invalid(key, format!("`{raw}` is not a valid value for this key")))
}

#[cfg(test)]
mod tests {
    use super::{Config, LogFormat, LogLevel, parse_trusted_proxies};
    use crate::error::ConfigErrorKind;
    use crate::source::MapEnv;
    use std::net::IpAddr;

    #[test]
    fn test_default_configuration_is_valid() {
        Config::default()
            .validate()
            .expect("defaults must validate");
    }

    #[test]
    fn test_defaults_match_the_design_recommendation() {
        let config = Config::default();
        assert_eq!(config.server.worker_threads, 0);
        assert_eq!(config.server.request_timeout_seconds, 30);
        assert_eq!(config.server.shutdown_drain_timeout_seconds, 30);
        assert_eq!(config.server.shutdown_force_timeout_seconds, 10);
        assert_eq!(config.database.acquire_timeout_seconds, 2);
        assert_eq!(config.database.readiness_timeout_seconds, 3);
        assert_eq!(config.auth.argon2_memory_cost_kib, 65_536);
        assert_eq!(config.auth.argon2_iterations, 3);
        assert_eq!(config.auth.argon2_parallelism, 1);
        assert_eq!(config.auth.password_hash_concurrency, 4);
        assert_eq!(config.auth.login_failure_threshold, 5);
        assert_eq!(config.auth.lockout_duration_seconds, 900);
        assert_eq!(config.rate_limit.normal_per_sec, 100);
        assert_eq!(config.rate_limit.custom_per_sec, 10);
        assert_eq!(config.world.component_updates_per_sec, 10);
        assert_eq!(config.rate_limit.persistent_threshold, 5);
        assert_eq!(config.rate_limit.max_buckets, 10_000);
        assert_eq!(config.rate_limit.login_per_ip_per_minute, 10);
        assert_eq!(config.rate_limit.login_ip_block_seconds, 300);
        assert_eq!(config.rate_limit.login_ip_max_buckets, 10_000);
        assert_eq!(config.rate_limit.realtime_ticket_per_interval, 10);
        assert_eq!(config.realtime.per_connection_capacity, 256);
        assert_eq!(config.realtime.resume_prune_interval_seconds, 15);
        assert_eq!(config.realtime.heartbeat_interval_seconds, 20);
        assert_eq!(config.realtime.connection_timeout_seconds, 60);
        assert_eq!(config.realtime.outbound_queue_capacity, 256);
        assert_eq!(config.world.server_tick_hz, 10);
        assert_eq!(config.world.resume_grace_seconds, 60);
        assert_eq!(config.world.checkpoint_interval_ticks, 300);
        assert_eq!(config.world.history_capacity, 256);
        assert!((config.world.max_speed - 50.0).abs() < f64::EPSILON);
        assert!((config.world.max_acceleration - 10.0).abs() < f64::EPSILON);
        assert!(config.world.speed_acceleration_check_enabled);
        assert_eq!(config.world.mailbox_control_capacity, 64);
        assert_eq!(config.world.mailbox_transform_capacity, 256);
        assert_eq!(config.world.mailbox_entity_capacity, 256);
        assert_eq!(config.identity.csv_max_bytes, 1_048_576);
        assert_eq!(config.identity.csv_max_rows, 1_000);
        assert_eq!(config.retention.realtime_ticket_interval_seconds, 60);
        assert_eq!(config.retention.idempotency_interval_seconds, 3_600);
        assert_eq!(config.retention.drain_budget_seconds, 20);
        assert_eq!(config.retention.restart_backoff_seconds, 5);
        assert_eq!(config.retention.extension_outbox_days, 1);
        assert_eq!(config.extensions.max_concurrency, 16);
        assert_eq!(config.extensions.delivery_timeout_seconds, 5);
        assert_eq!(config.extensions.max_attempts, 5);
        assert_eq!(config.extensions.backoff_min_seconds, 1);
        assert_eq!(config.extensions.backoff_max_seconds, 30);
        assert_eq!(config.extensions.circuit_failure_threshold, 5);
        assert_eq!(config.extensions.circuit_open_seconds, 30);
        assert_eq!(config.extensions.dlq_retention_days, 7);
        assert_eq!(config.extensions.pre_commit_validation_timeout_ms, 500);
        assert_eq!(config.extensions.pre_commit_validation_max_concurrency, 16);
        assert!(!config.extensions.allow_loopback_endpoints);
        assert!((config.interest.near_radius - 30.0).abs() < f64::EPSILON);
        assert!((config.interest.unsubscribe_radius - 35.0).abs() < f64::EPSILON);
        assert!((config.interest.cell_size - 20.0).abs() < f64::EPSILON);
        assert_eq!(config.observability.log_format, LogFormat::Json);
        assert_eq!(config.observability.log_level, LogLevel::Info);
    }

    #[test]
    fn test_auth_allow_stub_bearer_is_overridable_from_the_environment() {
        // H-5 added the `apply` branch but not the `CONFIG_KEYS` entry, so the
        // environment loop in `source.rs` — which iterates `CONFIG_KEYS` —
        // never applied it and then reported ORBISYNC_AUTH_ALLOW_STUB_BEARER
        // as an unknown variable. TOML and CLI reached `apply` directly and
        // did work. Removing the `keys.rs` entry turns this red.
        let env = MapEnv::from_pairs([("ORBISYNC_AUTH_ALLOW_STUB_BEARER", "true")]);
        let loaded = Config::load(None, &env, &[]).expect("configuration is valid");
        assert!(loaded.config.auth.allow_stub_bearer);
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn test_runtime_tuning_keys_are_applied_from_cli_overrides() {
        let mut config = Config::default();
        for (key, value) in [
            ("server.worker_threads", "2"),
            ("server.request_timeout_seconds", "45"),
            ("rate_limit.normal_per_sec", "25"),
            ("rate_limit.login_per_ip_per_minute", "25"),
            ("rate_limit.realtime_ticket_per_interval", "25"),
            ("auth.argon2_memory_cost_kib", "32768"),
            ("auth.password_hash_concurrency", "8"),
            ("world.checkpoint_interval_ticks", "120"),
            ("world.mailbox_control_capacity", "8"),
            ("world.mailbox_transform_capacity", "16"),
            ("world.mailbox_entity_capacity", "32"),
        ] {
            config.apply(key, value).expect("known runtime key");
        }
        assert_eq!(config.server.worker_threads, 2);
        assert_eq!(config.server.request_timeout_seconds, 45);
        assert_eq!(config.rate_limit.normal_per_sec, 25);
        assert_eq!(config.rate_limit.login_per_ip_per_minute, 25);
        assert_eq!(config.rate_limit.realtime_ticket_per_interval, 25);
        assert_eq!(config.auth.argon2_memory_cost_kib, 32_768);
        assert_eq!(config.auth.password_hash_concurrency, 8);
        assert_eq!(config.world.checkpoint_interval_ticks, 120);
        assert_eq!(config.world.mailbox_control_capacity, 8);
        assert_eq!(config.world.mailbox_transform_capacity, 16);
        assert_eq!(config.world.mailbox_entity_capacity, 32);
        config.validate().expect("tuned configuration is valid");
    }

    #[test]
    fn test_cfg2_limits_and_cors_validation_are_enforced() {
        let mut config = Config::default();
        config.apply("server.page_max_limit", "10").expect("key");
        config
            .apply("server.page_default_limit", "11")
            .expect("key");
        assert_eq!(
            config.validate().expect_err("default over max").kind(),
            ConfigErrorKind::Inconsistent
        );

        let mut timeout = Config::default();
        timeout
            .apply("server.request_timeout_seconds", "0")
            .expect("key");
        assert_eq!(
            timeout
                .validate()
                .expect_err("zero request timeout must fail")
                .kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut wildcard = Config::default();
        wildcard
            .apply("cors.allowed_origins", "[\"*\"]")
            .expect("list key");
        assert_eq!(
            wildcard.validate().expect_err("wildcard must fail").kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut credentials = Config::default();
        credentials
            .apply("cors.allow_credentials", "true")
            .expect("credentials key");
        assert_eq!(
            credentials
                .validate()
                .expect_err("credentials without origins must fail")
                .kind(),
            ConfigErrorKind::Inconsistent
        );
    }

    #[test]
    fn trusted_proxy_entries_are_typed_and_family_prefixes_are_strict() {
        let networks = parse_trusted_proxies("10.0.0.1,10.0.0.0/8,::1,2001:db8::/32")
            .expect("valid trusted proxy entries");
        assert_eq!(networks.len(), 4);
        assert!(networks[0].contains(&"10.0.0.1".parse::<IpAddr>().expect("ip")));
        assert!(networks[1].contains(&"10.12.0.1".parse::<IpAddr>().expect("ip")));
        assert!(networks[2].contains(&"::1".parse::<IpAddr>().expect("ip")));
        assert!(networks[3].contains(&"2001:db8::1".parse::<IpAddr>().expect("ip")));

        for value in ["10.0.0.0/33", "2001:db8::/129", "10.0.0.0/not-a-prefix"] {
            let mut config = Config::default();
            config.apply("server.trusted_proxies", value).expect("key");
            assert_eq!(
                config
                    .validate()
                    .expect_err("invalid prefix must fail")
                    .kind(),
                ConfigErrorKind::InvalidValue
            );
        }
        assert!(parse_trusted_proxies("0.0.0.0/0,::/0").is_ok());
        assert!(parse_trusted_proxies("10.0.0.0/32,2001:db8::/128").is_ok());
    }

    #[test]
    fn test_realtime_stub_ticket_is_disabled_by_default() {
        // Fail closed: without a real ticket verifier the gateway must refuse
        // every realtime connection (N-1).
        assert!(!Config::default().realtime.allow_stub_ticket);
    }

    #[test]
    fn resume_prune_interval_is_positive_and_shorter_than_grace() {
        let mut zero = Config::default();
        zero.realtime.resume_prune_interval_seconds = 0;
        assert_eq!(
            zero.validate().expect_err("zero interval must fail").kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut too_slow = Config::default();
        too_slow.realtime.resume_prune_interval_seconds = too_slow.world.resume_grace_seconds;
        assert_eq!(
            too_slow
                .validate()
                .expect_err("interval at grace must fail")
                .kind(),
            ConfigErrorKind::InvalidValue
        );
    }

    #[test]
    fn test_realtime_allow_stub_ticket_is_overridable() {
        // Goes through the full load path on purpose rather than calling
        // `apply` directly: the environment loop in `source.rs` iterates
        // `CONFIG_KEYS`, so this fails if the key is missing from `keys.rs`,
        // and `warnings.is_empty()` fails if the key is registered
        // inconsistently between the schema and `apply`.
        let env = MapEnv::from_pairs([("ORBISYNC_REALTIME_ALLOW_STUB_TICKET", "true")]);
        let loaded = Config::load(None, &env, &[]).expect("configuration is valid");
        assert!(loaded.config.realtime.allow_stub_ticket);
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    /// ADR-024: omitting `world.speed_acceleration_check_enabled` from every
    /// source (file, environment) must default to `true` (backward
    /// compatible with the pre-ADR-024 always-on check), through the full
    /// load path rather than a bare `Config::default()` read.
    #[test]
    fn test_speed_acceleration_check_enabled_defaults_to_true_when_omitted() {
        let loaded = Config::load(None, &MapEnv::default(), &[]).expect("configuration is valid");
        assert!(loaded.config.world.speed_acceleration_check_enabled);
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    /// ADR-024: `world.speed_acceleration_check_enabled` is overridable from
    /// the environment. Goes through the full load path for the same reason
    /// as `test_realtime_allow_stub_ticket_is_overridable`: it fails if the
    /// key is missing from `CONFIG_KEYS` or registered inconsistently
    /// between the schema and `apply`.
    #[test]
    fn test_speed_acceleration_check_enabled_is_overridable_from_the_environment() {
        let env =
            MapEnv::from_pairs([("ORBISYNC_WORLD_SPEED_ACCELERATION_CHECK_ENABLED", "false")]);
        let loaded = Config::load(None, &env, &[]).expect("configuration is valid");
        assert!(!loaded.config.world.speed_acceleration_check_enabled);
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    /// ADR-024: a non-boolean value must fail startup rather than silently
    /// falling back to a default (fail closed, consistent with every other
    /// `bool` key such as `realtime.allow_stub_ticket`).
    #[test]
    fn test_speed_acceleration_check_enabled_rejects_invalid_value() {
        let mut config = Config::default();
        let error = config
            .apply("world.speed_acceleration_check_enabled", "not-a-bool")
            .expect_err("non-boolean value must be rejected");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    /// ADR-025: omitting the pre-commit validation hook keys must default to
    /// the disabled/safe posture (short timeout, bounded concurrency, no
    /// loopback endpoints), through the full load path.
    #[test]
    fn test_pre_commit_validation_keys_default_when_omitted() {
        let loaded = Config::load(None, &MapEnv::default(), &[]).expect("configuration is valid");
        assert_eq!(
            loaded.config.extensions.pre_commit_validation_timeout_ms,
            500
        );
        assert_eq!(
            loaded
                .config
                .extensions
                .pre_commit_validation_max_concurrency,
            16
        );
        assert!(!loaded.config.extensions.allow_loopback_endpoints);
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    /// ADR-025: every pre-commit validation hook key is overridable from the
    /// environment. Goes through the full load path so a key missing from
    /// `CONFIG_KEYS` or registered inconsistently in `apply` fails the test.
    #[test]
    fn test_pre_commit_validation_keys_are_overridable_from_the_environment() {
        let env = MapEnv::from_pairs([
            (
                "ORBISYNC_EXTENSIONS_PRE_COMMIT_VALIDATION_TIMEOUT_MS",
                "250",
            ),
            (
                "ORBISYNC_EXTENSIONS_PRE_COMMIT_VALIDATION_MAX_CONCURRENCY",
                "4",
            ),
            ("ORBISYNC_EXTENSIONS_ALLOW_LOOPBACK_ENDPOINTS", "true"),
        ]);
        let loaded = Config::load(None, &env, &[]).expect("configuration is valid");
        assert_eq!(
            loaded.config.extensions.pre_commit_validation_timeout_ms,
            250
        );
        assert_eq!(
            loaded
                .config
                .extensions
                .pre_commit_validation_max_concurrency,
            4
        );
        assert!(loaded.config.extensions.allow_loopback_endpoints);
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    /// ADR-025: a zero timeout must fail startup rather than silently
    /// creating a hook that always times out immediately.
    #[test]
    fn test_pre_commit_validation_timeout_rejects_zero() {
        let mut config = Config::default();
        config.extensions.pre_commit_validation_timeout_ms = 0;
        let error = config
            .validate()
            .expect_err("zero timeout must be rejected");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    /// ADR-025: concurrency must stay within the same bound as the webhook
    /// delivery concurrency limit (1..=1024), for the same reason
    /// (`DeliveryPolicy::new_with_concurrency`): an unbounded value would let
    /// a misconfiguration create an unreasonable number of in-flight network
    /// operations.
    #[test]
    fn test_pre_commit_validation_max_concurrency_is_bounded() {
        let mut zero = Config::default();
        zero.extensions.pre_commit_validation_max_concurrency = 0;
        assert_eq!(
            zero.validate()
                .expect_err("zero concurrency must be rejected")
                .kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut too_high = Config::default();
        too_high.extensions.pre_commit_validation_max_concurrency = 1_025;
        assert_eq!(
            too_high
                .validate()
                .expect_err("excessive concurrency must be rejected")
                .kind(),
            ConfigErrorKind::InvalidValue
        );
    }

    #[test]
    fn test_apply_rejects_unknown_key() {
        let mut config = Config::default();
        let error = config
            .apply("server.unknown", "1")
            .expect_err("unknown key must fail");
        assert_eq!(error.kind(), ConfigErrorKind::UnknownKey);
    }

    #[test]
    fn test_apply_rejects_wrong_type() {
        let mut config = Config::default();
        let error = config
            .apply("database.max_connections", "many")
            .expect_err("non numeric value must fail");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    #[test]
    fn test_validate_rejects_out_of_range_values() {
        let mut config = Config::default();
        config.auth.access_token_ttl_seconds = 30;
        assert_eq!(
            config.validate().expect_err("ttl too small").kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut config = Config::default();
        config.realtime.max_message_bytes = 2_000_000;
        assert_eq!(
            config.validate().expect_err("message too large").kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut config = Config::default();
        config.extensions.max_concurrency = 0;
        assert_eq!(
            config
                .validate()
                .expect_err("zero delivery concurrency")
                .kind(),
            ConfigErrorKind::InvalidValue
        );

        let mut config = Config::default();
        config.extensions.max_concurrency = 1_025;
        assert_eq!(
            config
                .validate()
                .expect_err("excessive delivery concurrency")
                .kind(),
            ConfigErrorKind::InvalidValue
        );
    }

    #[test]
    fn test_validate_rejects_inconsistent_interest_radii() {
        let mut config = Config::default();
        config.interest.unsubscribe_radius = config.interest.near_radius;
        let error = config.validate().expect_err("hysteresis is required");
        assert_eq!(error.kind(), ConfigErrorKind::Inconsistent);
        assert_eq!(error.key(), "interest.unsubscribe_radius");
    }

    #[test]
    fn test_required_secret_env_vars_are_names_not_values() {
        let config = Config::default();
        assert_eq!(
            config.required_secret_env_vars(),
            [
                "DATABASE_URL",
                "ORBISYNC_TOKEN_SIGNING_KEY",
                "ORBISYNC_PAGINATION_HMAC_KEY",
                "ORBISYNC_REFRESH_TOKEN_HMAC_KEY",
                "ORBISYNC_REALTIME_TICKET_HMAC_KEY",
                "ORBISYNC_IDEMPOTENCY_HMAC_KEY",
                "ORBISYNC_EXTENSION_TOKEN_HMAC_KEY"
            ]
        );
    }
}

#[cfg(test)]
mod auth_method_tests {
    use super::{AUTH_METHOD_GUEST, AUTH_METHOD_LOCAL, Config};
    use crate::error::ConfigErrorKind;

    /// Builds a config with `guest` enabled and every required guest setting
    /// filled in, so each test can knock out exactly one of them.
    fn guest_enabled() -> Config {
        let mut config = Config::default();
        config.auth.methods = vec![AUTH_METHOD_LOCAL.to_owned(), AUTH_METHOD_GUEST.to_owned()];
        config.auth.guest.role_names = vec!["Visitor".to_owned()];
        config.auth.guest.allowed_worlds = vec!["0192d43d-a18a-7fed-8123-0123456789ab".to_owned()];
        config
    }

    #[test]
    fn default_enables_only_local_authentication() {
        let config = Config::default();
        assert_eq!(config.auth.methods, vec![AUTH_METHOD_LOCAL.to_owned()]);
        assert!(config.auth.guest.role_names.is_empty());
        assert!(config.auth.name_only.role_names.is_empty());
        assert!(config.auth.external.issuer.is_empty());
        config.validate().expect("default must stay valid");
    }

    #[test]
    fn unknown_method_name_is_rejected() {
        let mut config = Config::default();
        config.auth.methods = vec!["sso".to_owned()];
        let error = config.validate().expect_err("unknown method must fail");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    #[test]
    fn empty_method_list_is_rejected() {
        let mut config = Config::default();
        config.auth.methods = Vec::new();
        let error = config.validate().expect_err("empty method list must fail");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    #[test]
    fn guest_settings_are_only_required_once_guest_is_enabled() {
        // The defaults leave guest role_names and allowed_worlds empty. That
        // must stay valid while the method is off, otherwise every existing
        // deployment would fail to start after upgrading.
        let config = Config::default();
        assert!(config.auth.guest.role_names.is_empty());
        config.validate().expect("guest settings unused while off");

        guest_enabled().validate().expect("fully configured guest");
    }

    #[test]
    fn enabled_guest_without_roles_is_rejected() {
        // A subject with no role is denied every permission by the authorizer,
        // so this would look configured while refusing all work.
        let mut config = guest_enabled();
        config.auth.guest.role_names = Vec::new();
        let error = config.validate().expect_err("empty role_names must fail");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    #[test]
    fn enabled_guest_without_allowed_worlds_is_rejected() {
        // Empty must not be read as "no restriction": that would let a guest
        // join every world the role permits.
        let mut config = guest_enabled();
        config.auth.guest.allowed_worlds = Vec::new();
        let error = config
            .validate()
            .expect_err("empty allowed_worlds must fail");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }

    #[test]
    fn enabled_external_requires_issuer_audience_and_keys() {
        let mut config = Config::default();
        config.auth.methods = vec!["external".to_owned()];
        config.auth.external.role_names = vec!["Member".to_owned()];
        for (field, value) in [
            ("issuer", "https://idp.example.org"),
            ("audience", "orbisync"),
            ("jwks_path", "/etc/orbisync/jwks.json"),
        ] {
            let error = config.validate().expect_err("incomplete external config");
            assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
            match field {
                "issuer" => config.auth.external.issuer = value.to_owned(),
                "audience" => config.auth.external.audience = value.to_owned(),
                _ => config.auth.external.jwks_path = value.to_owned(),
            }
        }
        config.validate().expect("complete external config");
    }

    #[test]
    fn external_algorithm_none_is_rejected() {
        let mut config = Config::default();
        config.auth.methods = vec!["external".to_owned()];
        config.auth.external.issuer = "https://idp.example.org".to_owned();
        config.auth.external.audience = "orbisync".to_owned();
        config.auth.external.jwks_path = "/etc/orbisync/jwks.json".to_owned();
        config.auth.external.role_names = vec!["Member".to_owned()];
        for rejected in ["none", "None", "HS256", ""] {
            config.auth.external.algorithm = rejected.to_owned();
            let error = config
                .validate()
                .expect_err("unsigned or unlisted algorithm must fail");
            assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
        }
    }

    #[test]
    fn method_settings_apply_from_dotted_keys() {
        let mut config = Config::default();
        config
            .apply("auth.methods", "local,guest")
            .expect("methods apply");
        config
            .apply("auth.guest.role_names", "Visitor,Observer")
            .expect("role names apply");
        config
            .apply("auth.guest.session_ttl_seconds", "600")
            .expect("ttl applies");
        config
            .apply("auth.external.algorithm", "ES256")
            .expect("algorithm applies");
        assert_eq!(config.auth.methods, vec!["local", "guest"]);
        assert_eq!(config.auth.guest.role_names, vec!["Visitor", "Observer"]);
        assert_eq!(config.auth.guest.session_ttl_seconds, 600);
        assert_eq!(config.auth.external.algorithm, "ES256");
    }

    #[test]
    fn method_settings_accept_toml_array_form() {
        let mut config = Config::default();
        config
            .apply("auth.guest.allowed_worlds", r#"["world-a", "world-b"]"#)
            .expect("array form applies");
        assert_eq!(config.auth.guest.allowed_worlds, vec!["world-a", "world-b"]);
    }
}
