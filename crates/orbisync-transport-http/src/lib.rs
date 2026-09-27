//! REST and operational HTTP endpoints.
//!
//! Owns HTTP routes and calls application use cases (`architecture.md` §3).
//! Axum stays inside this crate (`architecture.md` §2.1).
//!
//! Milestone 0 serves the operational endpoints that the container health check
//! and the Compose development environment need:
//!
//! | Endpoint | Meaning |
//! |---|
//! | `GET /health/live` | process liveness, always `200` |
//! | `GET /health/ready` | `200` when every dependency answers, `503` otherwise |
//! | `GET /version` | build version |
//!
//! Health endpoints require no authentication and are expected to be reachable
//! from the internal network only (ADR-009).
//!
//! # Dependency rule
//!
//! `transport-http` depends on `domain` and `application`
//! (`repo-crate-conventions.md` §3.2). The readiness decision is computed by
//! the application layer; this crate only maps it onto a status code.

pub mod audit;
pub mod auth;
pub mod diagnostics;
mod extension_commands;
pub mod instance_membership;
pub mod login_rate_limit;
pub mod metrics;
pub mod password;
pub mod roles;
pub mod ticket_rate_limit;
pub mod users;
pub mod worlds;

use axum::body::{Body, to_bytes};
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use instance_membership::InstanceMembership;
use ipnet::IpNet;
use orbisync_application::metrics::{MetricsExporter, MetricsRecorder, NoopMetrics};
use orbisync_application::{
    AuditQueryPort, HealthProbe, IdempotencyStore, IdentityQueryPort, IdentityRepository,
    OperationalDiagnosticsPort, RealtimeTicketStore, RefreshTokenCreationStore,
    RefreshTokenRotationStore, SecretString, check_readiness,
};
use orbisync_domain::{AuthSessionId, Clock, Permission, Timestamp, UserId};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
    PasswordService, token::AccessTokenService,
};
use serde::Serialize;
use std::error::Error as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tracing::Instrument as _;
use uuid::Uuid;
use worlds::WorldDirectory;

/// Response header carrying the server-generated ADR-014 correlation ID.
pub const X_REQUEST_ID: &str = "x-request-id";

/// Stable public error registry from `openapi/errors.yaml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Malformed input.
    InvalidRequest,
    /// Missing or invalid authentication.
    AuthenticationRequired,
    /// Authenticated subject lacks permission.
    AccessDenied,
    /// The requested authentication method is not enabled on this server.
    ///
    /// Returned identically whether the method is switched off or was never
    /// configured, so a caller cannot use the response to probe the
    /// deployment's settings.
    AuthMethodDisabled,
    /// Resource does not exist.
    ResourceNotFound,
    /// An idempotency key was bound to another request.
    IdempotencyKeyReused,
    /// An idempotency key is in progress for the same request.
    IdempotencyInProgress,
    /// Unique or state conflict.
    ResourceConflict,
    /// Optimistic revision did not match.
    RevisionMismatch,
    /// Request rate exceeded.
    RateLimited,
    /// Request body exceeded the configured limit.
    PayloadTooLarge,
    /// Unexpected internal failure.
    InternalError,
    /// A required dependency is unavailable.
    ServiceUnavailable,
}

impl ErrorCode {
    /// Returns the registry spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::AuthenticationRequired => "AUTHENTICATION_REQUIRED",
            Self::AccessDenied => "ACCESS_DENIED",
            Self::AuthMethodDisabled => "AUTH_METHOD_DISABLED",
            Self::ResourceNotFound => "RESOURCE_NOT_FOUND",
            Self::IdempotencyKeyReused => "IDEMPOTENCY_KEY_REUSED",
            Self::IdempotencyInProgress => "IDEMPOTENCY_IN_PROGRESS",
            Self::ResourceConflict => "RESOURCE_CONFLICT",
            Self::RevisionMismatch => "REVISION_MISMATCH",
            Self::RateLimited => "RATE_LIMITED",
            Self::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            Self::InternalError => "INTERNAL_ERROR",
            Self::ServiceUnavailable => "SERVICE_UNAVAILABLE",
        }
    }

    /// Returns the status fixed by the public registry.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::AuthenticationRequired => StatusCode::UNAUTHORIZED,
            Self::AccessDenied | Self::AuthMethodDisabled => StatusCode::FORBIDDEN,
            Self::ResourceNotFound => StatusCode::NOT_FOUND,
            Self::IdempotencyKeyReused | Self::IdempotencyInProgress | Self::ResourceConflict => {
                StatusCode::CONFLICT
            }
            Self::RevisionMismatch => StatusCode::PRECONDITION_FAILED,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// Parses the canonical lowercase UUIDv7 form required for identifiers and
/// `Idempotency-Key`.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] for another UUID version, uppercase,
/// non-canonical hyphen placement, or invalid syntax.
pub fn parse_uuid_v7(value: &str) -> Result<Uuid, ErrorCode> {
    let parsed = Uuid::parse_str(value).map_err(|_| ErrorCode::InvalidRequest)?;
    if parsed.get_version_num() != 7 || parsed.hyphenated().to_string() != value {
        return Err(ErrorCode::InvalidRequest);
    }
    Ok(parsed)
}

/// Parses a strong `If-Match` revision in the OpenAPI form `"<digits>"`.
///
/// Wired for `DELETE /v1/roles/{role_id}` (W-G / CR-05). Previously no
/// production handler called this (CR-17), but it is now used by the role
/// deletion handler to enforce optimistic concurrency.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] for weak, unquoted, zero, or
/// non-numeric values.
pub fn parse_if_match(value: &str) -> Result<u64, ErrorCode> {
    let digits = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .ok_or(ErrorCode::InvalidRequest)?;
    let revision = digits
        .parse::<u64>()
        .map_err(|_| ErrorCode::InvalidRequest)?;
    if revision == 0 {
        return Err(ErrorCode::InvalidRequest);
    }
    Ok(revision)
}

/// Validates a page limit, applying the OpenAPI default of 50.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] outside 1 through 200.
pub fn page_limit(value: Option<u16>) -> Result<u16, ErrorCode> {
    page_limit_with_bounds(value, 50, 200)
}

/// Validates a page limit against configured default and maximum values.
pub fn page_limit_with_bounds(
    value: Option<u16>,
    default_limit: u16,
    max_limit: u16,
) -> Result<u16, ErrorCode> {
    let limit = value.unwrap_or(default_limit);
    if !(1..=max_limit).contains(&limit) {
        return Err(ErrorCode::InvalidRequest);
    }
    Ok(limit)
}

/// One allowlisted row from the ADR-016 user import contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserImportRow {
    /// Source row number, including the header row.
    pub row: usize,
    /// Login identifier text.
    pub login_id: String,
    /// Display name text.
    pub display_name: String,
}

/// Default maximum size for an imported CSV body.
pub const DEFAULT_CSV_MAX_BYTES: usize = 1_048_576;
/// Default maximum number of imported CSV data rows.
pub const DEFAULT_CSV_MAX_ROWS: usize = 1_000;
/// Defense-in-depth deadline for buffering a normal HTTP request body.
/// WebSocket routes are mounted on a separate router and do not inherit it.
pub const DEFAULT_REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// Default total processing deadline for a normal HTTP request.
/// This matches the legacy body-read deadline unless explicitly overridden.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = DEFAULT_REQUEST_BODY_TIMEOUT;

/// Decodes the strict M1 `text/csv` user import shape.
///
/// # Errors
///
/// Returns invalid request for input over 1 MiB, a header other than exactly
/// `login_id,display_name`, malformed CSV, extra fields, or over 1,000 rows.
pub fn decode_user_import(body: &[u8]) -> Result<Vec<UserImportRow>, ErrorCode> {
    decode_user_import_with_limits(body, DEFAULT_CSV_MAX_BYTES, DEFAULT_CSV_MAX_ROWS)
}

/// Decodes a user import with configured body-size and row-count limits.
pub fn decode_user_import_with_limits(
    body: &[u8],
    max_bytes: usize,
    max_rows: usize,
) -> Result<Vec<UserImportRow>, ErrorCode> {
    if body.len() > max_bytes {
        return Err(ErrorCode::InvalidRequest);
    }
    let text = core::str::from_utf8(body).map_err(|_| ErrorCode::InvalidRequest)?;
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|_| ErrorCode::InvalidRequest)?;
    if headers.len() != 2
        || headers.get(0) != Some("login_id")
        || headers.get(1) != Some("display_name")
    {
        return Err(ErrorCode::InvalidRequest);
    }
    let mut rows = Vec::new();
    for (index, record) in reader.records().enumerate() {
        if index >= max_rows {
            return Err(ErrorCode::InvalidRequest);
        }
        let record = record.map_err(|_| ErrorCode::InvalidRequest)?;
        rows.push(UserImportRow {
            row: index + 2,
            login_id: record.get(0).ok_or(ErrorCode::InvalidRequest)?.to_owned(),
            display_name: record.get(1).ok_or(ErrorCode::InvalidRequest)?.to_owned(),
        });
    }
    Ok(rows)
}

/// Correlation value generated exactly once before authentication.
#[derive(Debug, Clone)]
pub struct RequestContext {
    request_id: String,
    source_ip: Option<String>,
}

impl RequestContext {
    /// Returns `req_<canonical lowercase UUIDv7>`.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Returns the client IP after applying the configured trusted-proxy filter.
    #[must_use]
    pub fn source_ip(&self) -> Option<&str> {
        self.source_ip.as_deref()
    }
}

/// Shared state of the HTTP adapter.
#[derive(Clone)]
pub struct HttpState {
    extension_commands:
        Option<Arc<dyn orbisync_application::extension_command::ExtensionCommandApi>>,
    probes: Arc<Vec<Arc<dyn HealthProbe>>>,
    service: &'static str,
    version: &'static str,
    protocol_major: u32,
    worlds: Option<Arc<dyn WorldDirectory>>,
    instance_membership: Option<Arc<dyn InstanceMembership>>,
    allow_stub_bearer: bool,
    token_service: Option<Arc<AccessTokenService>>,
    clock: Arc<dyn Clock>,
    identity_repository: Option<Arc<dyn IdentityRepository>>,
    password_service: Option<Arc<PasswordService>>,
    identity_admin:
        Option<Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>>,
    identity_query: Option<Arc<dyn IdentityQueryPort>>,
    audit_query: Option<Arc<dyn AuditQueryPort>>,
    access_token_ttl_seconds: u64,
    refresh_token_ttl_seconds: u64,
    csv_max_bytes: usize,
    csv_max_rows: usize,
    max_request_body_bytes: usize,
    page_default_limit: u16,
    page_max_limit: u16,
    allowed_origins: Arc<Vec<HeaderValue>>,
    allow_credentials: bool,
    refresh_token_hmac_key: Vec<u8>,
    realtime_ticket_hmac_key: Vec<u8>,
    idempotency_hmac_key: Vec<u8>,
    realtime_ticket_store: Option<Arc<dyn RealtimeTicketStore>>,
    refresh_creation_store: Option<Arc<dyn RefreshTokenCreationStore>>,
    refresh_rotation_store: Option<Arc<dyn RefreshTokenRotationStore>>,
    idempotency_store: Option<Arc<dyn IdempotencyStore>>,
    login_rate_limiter: Arc<login_rate_limit::LoginRateLimiter>,
    realtime_ticket_rate_limiter: Arc<ticket_rate_limit::RealtimeTicketRateLimiter>,
    login_service: Option<Arc<LoginService>>,
    /// Guest and name-only issuing service; `None` when both are disabled.
    ephemeral_subject_service: Option<Arc<orbisync_identity::EphemeralSubjectService>>,
    /// External identity service; `None` when the method is disabled.
    external_auth_service: Option<Arc<orbisync_identity::ExternalAuthService>>,
    /// Methods this deployment accepts, reported by mode discovery.
    enabled_auth_methods: Vec<orbisync_domain::AuthMethod>,
    // Keep the original public representation for source compatibility while
    // retaining parsed networks for all trust decisions.
    trusted_proxies: Arc<Vec<String>>,
    trusted_proxy_networks: Arc<Vec<IpNet>>,
    request_body_timeout: Duration,
    request_timeout: Duration,
    metrics_recorder: Arc<dyn MetricsRecorder>,
    metrics_exporter: Arc<dyn MetricsExporter>,
    operational_diagnostics: Option<Arc<dyn OperationalDiagnosticsPort>>,
    shutting_down: Arc<AtomicBool>,
}

fn parse_trusted_proxy(value: &str) -> Option<IpNet> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.contains('/') {
        return value.parse().ok();
    }
    let ip = value.parse::<std::net::IpAddr>().ok()?;
    let prefix = match ip {
        std::net::IpAddr::V4(_) => 32,
        std::net::IpAddr::V6(_) => 128,
    };
    IpNet::new(ip, prefix).ok()
}

impl HttpState {
    /// Wires the extension-only gateway; user authentication remains independent.
    #[must_use]
    pub fn with_extension_commands(
        self,
        gateway: Arc<dyn orbisync_application::extension_command::ExtensionCommandApi>,
    ) -> Self {
        Self {
            extension_commands: Some(gateway),
            ..self
        }
    }
    /// Builds the state from the readiness probes wired by the composition
    /// root.
    ///
    /// `protocol_major` is passed in rather than read from `orbisync-protocol`:
    /// the dependency matrix keeps the protocol crate out of this adapter
    /// (`repo-crate-conventions.md` §3.2).
    /// `clock` is a required argument so that time-dependent validation
    /// (token expiry and session liveness) cannot silently fall back to a
    /// fixed timestamp when the caller forgets to wire it (CR-14).
    /// `access_token_ttl_seconds` and `refresh_token_ttl_seconds` are also
    /// required (CR-H, handover) so that forgetting to wire
    /// `auth.refresh_token_ttl_seconds` is a compile error instead of silently
    /// running with a hard-coded 30-day default.
    /// `refresh_token_hmac_key` is required (V-12) so that the refresh digest
    /// cannot silently fall back to keyless SHA-256; an empty key is a
    /// construction-time panic (fail-closed per ADR-002).
    /// `realtime_ticket_hmac_key` is required (RV-A C1) with the same
    /// fail-closed contract: it must be non-empty and is used to digest
    /// opaque realtime tickets.
    /// `idempotency_hmac_key` is required (P2-C1) with the same
    /// fail-closed contract: it must be non-empty and is used to HMAC
    /// idempotency `request_hash` so that `rest_idempotency.request_hash`
    /// does not allow offline brute-force without the server secret.
    #[must_use]
    pub fn new(
        probes: Vec<Arc<dyn HealthProbe>>,
        service: &'static str,
        version: &'static str,
        protocol_major: u32,
        clock: Arc<dyn Clock>,
        access_token_ttl_seconds: u64,
        refresh_token_ttl_seconds: u64,
        refresh_token_hmac_key: Vec<u8>,
    ) -> Self {
        assert!(
            !refresh_token_hmac_key.is_empty(),
            "refresh_token_hmac_key must not be empty (V-12: keyless SHA-256 fallback removed)"
        );
        Self {
            extension_commands: None,
            probes: Arc::new(probes),
            service,
            version,
            protocol_major,
            worlds: None,
            instance_membership: None,
            allow_stub_bearer: false,
            token_service: None,
            clock,
            identity_repository: None,
            password_service: None,
            identity_admin: None,
            identity_query: None,
            audit_query: None,
            access_token_ttl_seconds,
            refresh_token_ttl_seconds,
            csv_max_bytes: DEFAULT_CSV_MAX_BYTES,
            csv_max_rows: DEFAULT_CSV_MAX_ROWS,
            max_request_body_bytes: 2_097_152,
            page_default_limit: 50,
            page_max_limit: 200,
            allowed_origins: Arc::new(Vec::new()),
            allow_credentials: false,
            refresh_token_hmac_key,
            realtime_ticket_hmac_key: Vec::new(),
            idempotency_hmac_key: Vec::new(),
            realtime_ticket_store: None,
            refresh_creation_store: None,
            refresh_rotation_store: None,
            idempotency_store: None,
            login_rate_limiter: Arc::new(login_rate_limit::LoginRateLimiter::with_defaults()),
            realtime_ticket_rate_limiter: Arc::new(
                ticket_rate_limit::RealtimeTicketRateLimiter::with_defaults(),
            ),
            login_service: None,
            ephemeral_subject_service: None,
            external_auth_service: None,
            // Local only until the composition root says otherwise, matching
            // the configuration default.
            enabled_auth_methods: vec![orbisync_domain::AuthMethod::Local],
            trusted_proxies: Arc::new(Vec::new()),
            trusted_proxy_networks: Arc::new(Vec::new()),
            request_body_timeout: DEFAULT_REQUEST_BODY_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            metrics_recorder: Arc::new(NoopMetrics),
            metrics_exporter: Arc::new(NoopMetrics),
            operational_diagnostics: None,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Shares the process shutdown flag with the composition root.
    #[must_use]
    pub fn with_shutdown_flag(self, flag: Arc<AtomicBool>) -> Self {
        Self {
            shutting_down: flag,
            ..self
        }
    }

    /// Returns whether shutdown admission controls are active.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Attaches a world directory use case to the state.
    #[must_use]
    pub fn with_world_directory(self, directory: Arc<dyn WorldDirectory>) -> Self {
        Self {
            worlds: Some(directory),
            ..self
        }
    }

    /// Returns the world directory when configured.
    #[must_use]
    pub fn world_directory(&self) -> Option<Arc<dyn WorldDirectory>> {
        self.worlds.clone()
    }

    /// Attaches the instance membership use case to the state.
    #[must_use]
    pub fn with_instance_membership(self, membership: Arc<dyn InstanceMembership>) -> Self {
        Self {
            instance_membership: Some(membership),
            ..self
        }
    }

    /// Returns the instance membership use case when configured.
    #[must_use]
    pub fn instance_membership(&self) -> Option<Arc<dyn InstanceMembership>> {
        self.instance_membership.clone()
    }

    /// Configures whether the stub Bearer check is allowed (H-5).
    #[must_use]
    pub fn with_allow_stub_bearer(mut self, allow: bool) -> Self {
        self.allow_stub_bearer = allow;
        self
    }

    /// Returns whether stub Bearer is allowed.
    #[must_use]
    pub const fn allow_stub_bearer(&self) -> bool {
        self.allow_stub_bearer
    }

    /// Attaches the JWT token service for real Bearer validation.
    #[must_use]
    pub fn with_token_service(self, service: Arc<AccessTokenService>) -> Self {
        Self {
            token_service: Some(service),
            ..self
        }
    }

    /// Returns the token service when configured.
    #[must_use]
    pub fn token_service(&self) -> Option<Arc<AccessTokenService>> {
        self.token_service.clone()
    }

    /// Attaches the clock used for token time checks.
    ///
    /// The clock is already required by `new`; this method allows overriding it
    /// (e.g. in tests) while keeping the builder style.
    #[must_use]
    pub fn with_clock(self, clock: Arc<dyn Clock>) -> Self {
        Self { clock, ..self }
    }

    /// Returns the clock.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    /// Attaches the identity repository for login.
    #[must_use]
    pub fn with_identity_repository(self, repo: Arc<dyn IdentityRepository>) -> Self {
        Self {
            identity_repository: Some(repo),
            ..self
        }
    }

    /// Returns the identity repository.
    #[must_use]
    pub fn identity_repository(&self) -> Option<Arc<dyn IdentityRepository>> {
        self.identity_repository.clone()
    }

    /// Attaches the password service for login.
    #[must_use]
    pub fn with_password_service(self, service: Arc<PasswordService>) -> Self {
        Self {
            password_service: Some(service),
            ..self
        }
    }

    /// Returns the password service.
    #[must_use]
    pub fn password_service(&self) -> Option<Arc<PasswordService>> {
        self.password_service.clone()
    }

    /// Attaches the identity administration service for `POST /v1/users`.
    #[must_use]
    pub fn with_identity_admin_service(
        self,
        service: Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>,
    ) -> Self {
        Self {
            identity_admin: Some(service),
            ..self
        }
    }

    /// Returns the identity administration service.
    #[must_use]
    pub fn identity_admin_service(
        &self,
    ) -> Option<Arc<IdentityAdministrationService<DynIdentityAdministrationStore, DynClock>>> {
        self.identity_admin.clone()
    }

    /// Attaches the identity query port for GET /v1/users / roles.
    #[must_use]
    pub fn with_identity_query(self, query: Arc<dyn IdentityQueryPort>) -> Self {
        Self {
            identity_query: Some(query),
            ..self
        }
    }

    /// Returns the identity query port.
    #[must_use]
    pub fn identity_query(&self) -> Option<Arc<dyn IdentityQueryPort>> {
        self.identity_query.clone()
    }

    /// Attaches the audit query port for GET /v1/audit-events.
    #[must_use]
    pub fn with_audit_query(self, query: Arc<dyn AuditQueryPort>) -> Self {
        Self {
            audit_query: Some(query),
            ..self
        }
    }

    /// Returns the audit query port.
    #[must_use]
    pub fn audit_query(&self) -> Option<Arc<dyn AuditQueryPort>> {
        self.audit_query.clone()
    }

    /// Configures the access-token TTL from `auth.access_token_ttl_seconds`.
    #[must_use]
    pub fn with_access_token_ttl(mut self, ttl_seconds: u64) -> Self {
        self.access_token_ttl_seconds = ttl_seconds;
        self
    }

    /// Returns the configured access-token TTL.
    #[must_use]
    pub const fn access_token_ttl_seconds(&self) -> u64 {
        self.access_token_ttl_seconds
    }

    /// Configures the refresh-token TTL from `auth.refresh_token_ttl_seconds`.
    #[must_use]
    pub fn with_refresh_token_ttl(mut self, ttl_seconds: u64) -> Self {
        self.refresh_token_ttl_seconds = ttl_seconds;
        self
    }

    /// Returns the configured refresh-token TTL.
    #[must_use]
    pub const fn refresh_token_ttl_seconds(&self) -> u64 {
        self.refresh_token_ttl_seconds
    }

    /// Configures the CSV user-import limits from `identity` settings.
    #[must_use]
    pub fn with_csv_limits(mut self, max_bytes: usize, max_rows: usize) -> Self {
        self.csv_max_bytes = max_bytes;
        self.csv_max_rows = max_rows;
        self
    }

    /// Configures the maximum HTTP request body size.
    #[must_use]
    pub fn with_max_request_body_bytes(mut self, max_bytes: usize) -> Self {
        self.max_request_body_bytes = max_bytes;
        self
    }

    /// Returns the configured maximum HTTP request body size.
    #[must_use]
    pub const fn max_request_body_bytes(&self) -> usize {
        self.max_request_body_bytes
    }

    /// Configures pagination defaults and maximum.
    #[must_use]
    pub fn with_page_limits(mut self, default_limit: u16, max_limit: u16) -> Self {
        self.page_default_limit = default_limit;
        self.page_max_limit = max_limit;
        self
    }

    /// Validates a query page limit using the configured bounds.
    pub fn page_limit(&self, value: Option<u16>) -> Result<u16, ErrorCode> {
        page_limit_with_bounds(value, self.page_default_limit, self.page_max_limit)
    }

    /// Configures the explicit CORS origin allowlist.
    #[must_use]
    pub fn with_allowed_origins(mut self, allowed_origins: Vec<HeaderValue>) -> Self {
        self.allowed_origins = Arc::new(allowed_origins);
        self
    }

    /// Configures whether CORS requests may include credentials.
    #[must_use]
    pub fn with_allow_credentials(mut self, allow_credentials: bool) -> Self {
        self.allow_credentials = allow_credentials;
        self
    }

    /// Returns the configured CORS origins.
    #[must_use]
    pub fn allowed_origins(&self) -> Arc<Vec<HeaderValue>> {
        Arc::clone(&self.allowed_origins)
    }

    /// Returns whether CORS credentials are enabled.
    #[must_use]
    pub const fn allow_credentials(&self) -> bool {
        self.allow_credentials
    }

    /// Decodes a CSV user import using this HTTP state's configured limits.
    pub fn decode_user_import_body(&self, body: &[u8]) -> Result<Vec<UserImportRow>, ErrorCode> {
        decode_user_import_with_limits(body, self.csv_max_bytes, self.csv_max_rows)
    }

    /// Configures the HMAC key for refresh-token digests.
    ///
    /// Panics if the key is empty (V-12: keyless fallback removed).
    #[must_use]
    pub fn with_refresh_token_hmac_key(mut self, key: Vec<u8>) -> Self {
        assert!(
            !key.is_empty(),
            "refresh_token_hmac_key must not be empty (V-12)"
        );
        self.refresh_token_hmac_key = key;
        self
    }

    /// Returns the refresh-token HMAC key.
    #[must_use]
    pub fn refresh_token_hmac_key(&self) -> &[u8] {
        &self.refresh_token_hmac_key
    }

    /// Configures the HMAC key for realtime-ticket digests (RV-A C1).
    #[must_use]
    pub fn with_realtime_ticket_hmac_key(mut self, key: Vec<u8>) -> Self {
        assert!(
            !key.is_empty(),
            "realtime_ticket_hmac_key must not be empty (C1: keyless fallback forbidden)"
        );
        self.realtime_ticket_hmac_key = key;
        self
    }

    /// Returns the realtime-ticket HMAC key.
    #[must_use]
    pub fn realtime_ticket_hmac_key(&self) -> &[u8] {
        &self.realtime_ticket_hmac_key
    }

    /// Configures the HMAC key for idempotency request digests (P2-C1).
    ///
    /// Panics if the key is empty (keyless SHA-256 fallback removed).
    #[must_use]
    pub fn with_idempotency_hmac_key(mut self, key: Vec<u8>) -> Self {
        assert!(
            !key.is_empty(),
            "idempotency_hmac_key must not be empty (P2-C1: keyless SHA-256 fallback removed)"
        );
        self.idempotency_hmac_key = key;
        self
    }

    /// Returns the idempotency HMAC key.
    #[must_use]
    pub fn idempotency_hmac_key(&self) -> &[u8] {
        &self.idempotency_hmac_key
    }

    /// Attaches the realtime-ticket store (RV-A C1).
    #[must_use]
    pub fn with_realtime_ticket_store(self, store: Arc<dyn RealtimeTicketStore>) -> Self {
        Self {
            realtime_ticket_store: Some(store),
            ..self
        }
    }

    /// Returns the realtime-ticket store.
    #[must_use]
    pub fn realtime_ticket_store(&self) -> Option<Arc<dyn RealtimeTicketStore>> {
        self.realtime_ticket_store.clone()
    }

    /// Attaches the refresh-token creation store (for login).
    #[must_use]
    pub fn with_refresh_creation_store(self, store: Arc<dyn RefreshTokenCreationStore>) -> Self {
        Self {
            refresh_creation_store: Some(store),
            ..self
        }
    }

    /// Returns the refresh-token creation store.
    #[must_use]
    pub fn refresh_creation_store(&self) -> Option<Arc<dyn RefreshTokenCreationStore>> {
        self.refresh_creation_store.clone()
    }

    /// Attaches the refresh-token rotation store.
    #[must_use]
    pub fn with_refresh_rotation_store(self, store: Arc<dyn RefreshTokenRotationStore>) -> Self {
        Self {
            refresh_rotation_store: Some(store),
            ..self
        }
    }

    /// Returns the refresh-token rotation store.
    #[must_use]
    pub fn refresh_rotation_store(&self) -> Option<Arc<dyn RefreshTokenRotationStore>> {
        self.refresh_rotation_store.clone()
    }

    /// Attaches the idempotency store for `POST /v1/users/{user_id}/reset-password`.
    #[must_use]
    pub fn with_idempotency_store(self, store: Arc<dyn IdempotencyStore>) -> Self {
        Self {
            idempotency_store: Some(store),
            ..self
        }
    }

    /// Returns the idempotency store.
    #[must_use]
    pub fn idempotency_store(&self) -> Option<Arc<dyn IdempotencyStore>> {
        self.idempotency_store.clone()
    }

    /// Attaches the per-source-IP login rate limiter.
    #[must_use]
    pub fn with_login_rate_limiter(self, limiter: Arc<login_rate_limit::LoginRateLimiter>) -> Self {
        Self {
            login_rate_limiter: limiter,
            ..self
        }
    }

    /// Returns the per-source-IP login rate limiter.
    #[must_use]
    pub fn login_rate_limiter(&self) -> Arc<login_rate_limit::LoginRateLimiter> {
        Arc::clone(&self.login_rate_limiter)
    }

    /// Attaches the rate limiter for realtime ticket issuance (C5).
    #[must_use]
    pub fn with_realtime_ticket_rate_limiter(
        self,
        limiter: Arc<ticket_rate_limit::RealtimeTicketRateLimiter>,
    ) -> Self {
        Self {
            realtime_ticket_rate_limiter: limiter,
            ..self
        }
    }

    /// Returns the realtime ticket rate limiter.
    #[must_use]
    pub fn realtime_ticket_rate_limiter(
        &self,
    ) -> Arc<ticket_rate_limit::RealtimeTicketRateLimiter> {
        Arc::clone(&self.realtime_ticket_rate_limiter)
    }

    /// Attaches the login service (P2-C4: handler delegates to application service).
    #[must_use]
    pub fn with_login_service(self, service: Arc<LoginService>) -> Self {
        Self {
            login_service: Some(service),
            ..self
        }
    }

    /// Returns the login service.
    #[must_use]
    pub fn login_service(&self) -> Option<Arc<LoginService>> {
        self.login_service.clone()
    }

    /// Attaches the guest and name-only service (ADR-026).
    #[must_use]
    pub fn with_ephemeral_subject_service(
        self,
        service: Arc<orbisync_identity::EphemeralSubjectService>,
    ) -> Self {
        Self {
            ephemeral_subject_service: Some(service),
            ..self
        }
    }

    /// Returns the guest and name-only service, if either method is enabled.
    #[must_use]
    pub fn ephemeral_subject_service(
        &self,
    ) -> Option<Arc<orbisync_identity::EphemeralSubjectService>> {
        self.ephemeral_subject_service.clone()
    }

    /// Attaches the external identity service (ADR-026).
    #[must_use]
    pub fn with_external_auth_service(
        self,
        service: Arc<orbisync_identity::ExternalAuthService>,
    ) -> Self {
        Self {
            external_auth_service: Some(service),
            ..self
        }
    }

    /// Returns the external identity service, if the method is enabled.
    #[must_use]
    pub fn external_auth_service(&self) -> Option<Arc<orbisync_identity::ExternalAuthService>> {
        self.external_auth_service.clone()
    }

    /// Records which authentication methods this deployment accepts.
    #[must_use]
    pub fn with_enabled_auth_methods(self, methods: Vec<orbisync_domain::AuthMethod>) -> Self {
        Self {
            enabled_auth_methods: methods,
            ..self
        }
    }

    /// Returns the accepted authentication methods, for mode discovery.
    ///
    /// Only the method identifiers are exposed. Issuer URLs, key locations and
    /// every other setting stay server-side: discovery tells a client which
    /// call to make, not how the deployment is configured.
    #[must_use]
    pub fn enabled_auth_methods(&self) -> &[orbisync_domain::AuthMethod] {
        &self.enabled_auth_methods
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Configures trusted proxies for source IP resolution (P2-C4, ADR-009).
    #[must_use]
    pub fn with_trusted_proxies(mut self, proxies: Vec<String>) -> Self {
        let networks = proxies
            .iter()
            .filter(|proxy| !proxy.trim().is_empty())
            .map(|proxy| parse_trusted_proxy(proxy))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        self.trusted_proxies = Arc::new(proxies);
        self.trusted_proxy_networks = Arc::new(networks);
        self
    }

    /// Configures already validated trusted proxy networks.
    #[must_use]
    pub fn with_trusted_proxy_networks(mut self, networks: Vec<IpNet>) -> Self {
        self.trusted_proxies = Arc::new(networks.iter().map(ToString::to_string).collect());
        self.trusted_proxy_networks = Arc::new(networks);
        self
    }

    /// Returns the trusted proxy list in its original string representation.
    ///
    /// This accessor intentionally retains the pre-typed API. Use
    /// [`Self::trusted_proxy_networks`] for the parsed representation used by
    /// source-IP resolution.
    #[must_use]
    pub fn trusted_proxies(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.trusted_proxies)
    }

    /// Returns the parsed trusted proxy networks used for trust decisions.
    #[must_use]
    pub fn trusted_proxy_networks(&self) -> Arc<Vec<IpNet>> {
        Arc::clone(&self.trusted_proxy_networks)
    }

    /// Configures the normal HTTP request body-read deadline.
    /// WebSocket routes are mounted separately and are not affected.
    #[must_use]
    pub fn with_request_body_timeout(mut self, timeout: Duration) -> Self {
        if !timeout.is_zero() {
            self.request_body_timeout = timeout;
        }
        self
    }

    /// Returns the normal HTTP request body-read deadline.
    #[must_use]
    pub const fn request_body_timeout(&self) -> Duration {
        self.request_body_timeout
    }

    /// Configures the total normal HTTP request deadline.
    /// WebSocket routes are mounted separately and are not affected.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        if !timeout.is_zero() {
            self.request_timeout = timeout;
        }
        self
    }

    /// Returns the total normal HTTP request deadline.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Resolves the client source IP using the trusted proxy filter.
    ///
    /// When the transport peer is trusted, `X-Forwarded-For` is walked from
    /// the right (closest to the trusted peer) and the first untrusted address
    /// is returned. This supports proxy chains without unconditionally
    /// accepting a left-most, client-controlled value. If the peer is absent,
    /// untrusted, the header is malformed, or the chain contains only trusted
    /// addresses, the peer address is used (or `None` when there is no peer).
    /// The result is always a validated IP string and never carries secrets.
    #[must_use]
    pub fn resolve_source_ip(
        &self,
        headers: &HeaderMap,
        connect_info: Option<std::net::SocketAddr>,
    ) -> Option<String> {
        let peer = connect_info.map(|addr| addr.ip());
        // If trusted proxies list empty, never trust headers (ADR-009 fail-closed).
        if self.trusted_proxy_networks.is_empty() {
            return peer.map(|ip| ip.to_string());
        }
        let peer = peer?;
        if !self.is_trusted_proxy(peer) {
            return Some(peer.to_string());
        }
        // `HeaderMap::get` only returns the first field. Collect every XFF
        // field instead: `get_all().iter()` is insertion-ordered, which is
        // the wire order preserved by the HTTP parser. Each field may itself
        // contain a comma-separated chain, so flatten both levels before the
        // right-to-left trusted-hop walk. Any value that cannot be represented
        // in that ordered chain fails closed to the transport peer.
        let mut chain = Vec::new();
        for value in headers.get_all("x-forwarded-for").iter() {
            let Ok(value) = value.to_str() else {
                return Some(peer.to_string());
            };
            for address in value.split(',') {
                let Ok(address) = address.trim().parse::<std::net::IpAddr>() else {
                    return Some(peer.to_string());
                };
                chain.push(address);
            }
        }
        if chain.is_empty() {
            return Some(peer.to_string());
        }
        if let Some(client) = chain
            .into_iter()
            .rev()
            .find(|address| !self.is_trusted_proxy(*address))
        {
            return Some(client.to_string());
        }
        Some(peer.to_string())
    }

    fn is_trusted_proxy(&self, address: std::net::IpAddr) -> bool {
        self.trusted_proxy_networks
            .iter()
            .any(|network| network.contains(&address))
    }
    /// Attaches the metrics recorder (D1-A).
    #[must_use]
    pub fn with_metrics_recorder(self, recorder: Arc<dyn MetricsRecorder>) -> Self {
        Self {
            metrics_recorder: recorder,
            ..self
        }
    }

    /// Returns the metrics recorder.
    #[must_use]
    pub fn metrics_recorder(&self) -> Arc<dyn MetricsRecorder> {
        Arc::clone(&self.metrics_recorder)
    }

    /// Attaches the metrics exporter for `GET /metrics` (D1-A).
    #[must_use]
    pub fn with_metrics_exporter(self, exporter: Arc<dyn MetricsExporter>) -> Self {
        Self {
            metrics_exporter: exporter,
            ..self
        }
    }

    /// Returns the metrics exporter.
    #[must_use]
    pub fn metrics_exporter(&self) -> Arc<dyn MetricsExporter> {
        Arc::clone(&self.metrics_exporter)
    }

    /// Attaches the privileged operational diagnostics provider.
    #[must_use]
    pub fn with_operational_diagnostics(
        self,
        diagnostics: Arc<dyn OperationalDiagnosticsPort>,
    ) -> Self {
        Self {
            operational_diagnostics: Some(diagnostics),
            ..self
        }
    }

    /// Returns the privileged operational diagnostics provider when wired.
    #[must_use]
    pub fn operational_diagnostics(&self) -> Option<Arc<dyn OperationalDiagnosticsPort>> {
        self.operational_diagnostics.as_ref().map(Arc::clone)
    }
}

impl core::fmt::Debug for HttpState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HttpState")
            .field("probes", &self.probes.len())
            .field("service", &self.service)
            .field("version", &self.version)
            .field("protocol_major", &self.protocol_major)
            .field("worlds", &self.worlds.is_some())
            .field("instance_membership", &self.instance_membership.is_some())
            .field("allow_stub_bearer", &self.allow_stub_bearer)
            .field("token_service", &self.token_service.is_some())
            .field("clock", &true)
            .field("identity_repository", &self.identity_repository.is_some())
            .field("password_service", &self.password_service.is_some())
            .field("identity_admin", &self.identity_admin.is_some())
            .field("identity_query", &self.identity_query.is_some())
            .field("audit_query", &self.audit_query.is_some())
            .field("access_token_ttl_seconds", &self.access_token_ttl_seconds)
            .field("refresh_token_ttl_seconds", &self.refresh_token_ttl_seconds)
            .field("refresh_token_hmac_key", &"[REDACTED]")
            .field("realtime_ticket_hmac_key", &"[REDACTED]")
            .field("idempotency_hmac_key", &"[REDACTED]")
            .field(
                "realtime_ticket_store",
                &self.realtime_ticket_store.is_some(),
            )
            .field(
                "refresh_creation_store",
                &self.refresh_creation_store.is_some(),
            )
            .field(
                "refresh_rotation_store",
                &self.refresh_rotation_store.is_some(),
            )
            .field("idempotency_store", &self.idempotency_store.is_some())
            .field("realtime_ticket_rate_limiter", &true)
            .field("login_service", &self.login_service.is_some())
            .field("trusted_proxies", &self.trusted_proxies.len())
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("page_default_limit", &self.page_default_limit)
            .field("page_max_limit", &self.page_max_limit)
            .field("allowed_origins", &self.allowed_origins.len())
            .field("allow_credentials", &self.allow_credentials)
            .field("metrics_recorder", &true)
            .field("metrics_exporter", &true)
            .field(
                "operational_diagnostics",
                &self.operational_diagnostics.is_some(),
            )
            .finish()
    }
}

/// Generates a 256-bit opaque refresh token (base64url, no pad).
#[must_use]
pub fn generate_refresh_token() -> SecretString {
    use base64::Engine as _;
    use rand::Rng as _;
    let mut bytes = [0_u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    SecretString::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Generates a 256-bit opaque realtime ticket (base64url, no pad) (RV-A C1).
#[must_use]
pub fn generate_realtime_ticket() -> SecretString {
    use base64::Engine as _;
    use rand::Rng as _;
    let mut bytes = [0_u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    SecretString::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Computes the HMAC-SHA-256 digest for a realtime ticket (RV-A C1).
#[must_use]
#[allow(clippy::expect_used)]
pub fn realtime_ticket_digest(key: &[u8], token: &SecretString) -> [u8; 32] {
    assert!(
        !key.is_empty(),
        "realtime_ticket_hmac_key must not be empty (C1)"
    );
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key must be valid (non-empty, C1)");
    mac.update(token.expose_secret().as_bytes());
    mac.finalize().into_bytes().into()
}

/// Computes the HMAC-SHA-256 digest for a refresh token (V-12).
///
/// The key must be non-empty; an empty key panics instead of silently falling
/// back to plain SHA-256 (ADR-002: reading `refresh_tokens.token_digest`
/// must not allow offline verification without the server secret).
#[must_use]
#[allow(clippy::expect_used)]
pub fn refresh_token_digest(key: &[u8], token: &SecretString) -> [u8; 32] {
    assert!(
        !key.is_empty(),
        "refresh_token_hmac_key must not be empty (V-12: keyless SHA-256 fallback removed)"
    );
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(key).expect("HMAC key must be valid (non-empty, V-12)");
    mac.update(token.expose_secret().as_bytes());
    mac.finalize().into_bytes().into()
}

fn put_len_prefixed(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_be_bytes());
    out.extend_from_slice(value);
}

/// Computes HMAC-SHA-256 for idempotency `request_hash` with length-prefixed canonical encoding (P2-C1).
///
/// The encoding concatenates `operation`, `actor` (when present), and each field
/// with a `u64` big-endian length prefix, so `("ab","c")` and `("a","bc")`
/// produce different digests. The result is HMAC-SHA-256 with the dedicated
/// idempotency key – a DB or backup leak does not allow offline brute-force
/// without the server secret.
#[must_use]
#[allow(clippy::expect_used)]
pub fn idempotency_request_digest(
    key: &[u8],
    operation: &str,
    actor: Option<UserId>,
    fields: &[&[u8]],
) -> [u8; 32] {
    assert!(
        !key.is_empty(),
        "idempotency_hmac_key must not be empty (P2-C1: keyless SHA-256 fallback removed)"
    );
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut canonical = Vec::new();
    put_len_prefixed(&mut canonical, operation.as_bytes());
    if let Some(id) = actor {
        put_len_prefixed(&mut canonical, id.to_string().as_bytes());
    } else {
        put_len_prefixed(&mut canonical, b"");
    }
    for field in fields {
        put_len_prefixed(&mut canonical, field);
    }
    let mut mac =
        HmacSha256::new_from_slice(key).expect("HMAC key must be valid (non-empty, P2-C1)");
    mac.update(&canonical);
    mac.finalize().into_bytes().into()
}

/// Extracts a Bearer token from `Authorization: Bearer <token>`.
#[must_use]
pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let text = value.to_str().ok()?;
    if text.starts_with("Bearer ") && text.len() > 7 {
        let token = text[7..].trim();
        if !token.is_empty() {
            return Some(token.to_owned());
        }
    }
    None
}

fn auth_error_response(code: ErrorCode, message: String, request_id: String) -> Response {
    let status = code.status();
    #[derive(Serialize)]
    struct ErrEnvelope {
        error: ErrBody,
    }
    #[derive(Serialize)]
    struct ErrBody {
        code: &'static str,
        message: String,
        request_id: String,
        details: serde_json::Value,
    }
    let body = ErrEnvelope {
        error: ErrBody {
            code: code.as_str(),
            message,
            request_id,
            details: serde_json::json!({}),
        },
    };
    (status, Json(body)).into_response()
}

/// Validates bearer, token signature/time, and session liveness in one place
/// (CR-13). Any failure of `session_id()` is 401 – the fail-open
/// `if let Ok(session_id) = claims.session_id()` pattern is gone.
pub(crate) async fn authenticate(
    state: &HttpState,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<(UserId, AuthSessionId, Timestamp), Response> {
    let Some(token_service) = state.token_service() else {
        return Err(auth_error_response(
            ErrorCode::InternalError,
            "token service unavailable".to_owned(),
            request_id.to_owned(),
        ));
    };
    let Some(repo) = state.identity_repository() else {
        return Err(auth_error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id.to_owned(),
        ));
    };
    let Some(bearer) = extract_bearer(headers) else {
        return Err(auth_error_response(
            ErrorCode::AuthenticationRequired,
            "authentication required".to_owned(),
            request_id.to_owned(),
        ));
    };
    let secret = SecretString::new(bearer);
    let now = state.clock().now();
    let claims = token_service.validate(&secret, now).map_err(|_| {
        auth_error_response(
            ErrorCode::AuthenticationRequired,
            "access token is invalid".to_owned(),
            request_id.to_owned(),
        )
    })?;
    let session_id = claims.session_id().map_err(|_| {
        auth_error_response(
            ErrorCode::AuthenticationRequired,
            "access token is invalid".to_owned(),
            request_id.to_owned(),
        )
    })?;
    let actor_id = claims.user_id().map_err(|_| {
        auth_error_response(
            ErrorCode::AuthenticationRequired,
            "access token is invalid".to_owned(),
            request_id.to_owned(),
        )
    })?;
    match repo.find_session(session_id).await {
        Ok(Some(session)) => {
            if !session.is_active_at(now) || session.user_id() != actor_id {
                return Err(auth_error_response(
                    ErrorCode::AuthenticationRequired,
                    "access token is invalid".to_owned(),
                    request_id.to_owned(),
                ));
            }
        }
        Ok(None) => {
            return Err(auth_error_response(
                ErrorCode::AuthenticationRequired,
                "access token is invalid".to_owned(),
                request_id.to_owned(),
            ));
        }
        Err(_) => {
            return Err(auth_error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id.to_owned(),
            ));
        }
    }
    Ok((actor_id, session_id, now))
}

/// Authenticates and checks a single RBAC permission (CR-13).
pub(crate) async fn authorize(
    state: &HttpState,
    headers: &HeaderMap,
    request_id: &str,
    permission: &str,
) -> Result<UserId, Response> {
    let (actor_id, _, _) = authenticate(state, headers, request_id).await?;
    let Some(repo) = state.identity_repository() else {
        return Err(auth_error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id.to_owned(),
        ));
    };
    let roles = repo.roles_for_user(actor_id).await.map_err(|_| {
        auth_error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id.to_owned(),
        )
    })?;
    let perm = Permission::new(permission.to_owned()).map_err(|e| {
        auth_error_response(
            ErrorCode::InternalError,
            e.to_string(),
            request_id.to_owned(),
        )
    })?;
    if orbisync_identity::RbacAuthorizer::authorize(&roles, &perm)
        != orbisync_identity::AuthorizationDecision::Allow
    {
        return Err(auth_error_response(
            ErrorCode::AccessDenied,
            "permission denied".to_owned(),
            request_id.to_owned(),
        ));
    }
    Ok(actor_id)
}

/// Body of `GET /version`.
#[derive(Debug, Serialize)]
pub struct VersionBody {
    /// Service name, always `orbisync`.
    pub service: &'static str,
    /// Build version.
    pub version: &'static str,
    /// Major realtime protocol version implemented by this build.
    pub protocol_major: u32,
}

/// Body of `GET /health/live` and `GET /health/ready`.
#[derive(Debug, Serialize)]
pub struct HealthBody {
    /// `live` or `ready`.
    pub status: &'static str,
    /// Names of the dependencies that failed, empty when healthy.
    pub failures: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
    request_id: String,
    details: ErrorDetails,
}

#[derive(Debug, Serialize)]
struct ErrorDetails {
    failures: Vec<&'static str>,
}

/// Builds the HTTP router.
pub fn router(state: HttpState) -> Router {
    let metrics_state = state.clone();
    let app = Router::new()
        .route("/v1/extensions/commands", post(extension_commands::execute))
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/version", get(version))
        .route("/metrics", get(metrics::handler))
        .route(
            "/v1/worlds",
            get(worlds::list_worlds).post(worlds::create_world),
        )
        .route(
            "/v1/worlds/{world_id}",
            get(worlds::get_world).patch(worlds::update_world),
        )
        .route("/v1/worlds/{world_id}/archive", post(worlds::archive_world))
        .route(
            "/v1/instances",
            get(worlds::list_instances).post(worlds::create_instance),
        )
        .route("/v1/instances/{instance_id}", get(worlds::get_instance))
        .route(
            "/v1/instances/{instance_id}/start",
            post(worlds::start_instance),
        )
        .route(
            "/v1/instances/{instance_id}/stop",
            post(worlds::stop_instance),
        )
        .route(
            "/v1/instances/{instance_id}/kick/{user_id}",
            post(instance_membership::kick_member),
        )
        .route(
            "/v1/instances/{instance_id}/members",
            get(instance_membership::list_members),
        )
        .route("/v1/auth/methods", get(auth::auth_methods))
        .route("/v1/auth/login", post(auth::login))
        .route("/v1/auth/guest", post(auth::guest))
        .route("/v1/auth/name", post(auth::name_only))
        .route("/v1/auth/external", post(auth::external))
        .route("/v1/auth/refresh", post(auth::refresh))
        .route("/v1/auth/logout", post(auth::logout))
        .route("/v1/auth/me", get(auth::me))
        .route(
            "/v1/auth/administration-access",
            get(auth::administration_access),
        )
        .route("/v1/auth/change-password", post(password::change_password))
        .route("/v1/realtime/tickets", post(auth::create_realtime_ticket))
        .route("/v1/users", get(users::list_users).post(users::create_user))
        .route(
            "/v1/users/{user_id}",
            get(users::get_user).patch(users::update_user),
        )
        .route(
            "/v1/users/{user_id}/reset-password",
            post(users::reset_user_password),
        )
        .route("/v1/users/{user_id}/disable", post(users::disable_user))
        .route("/v1/users/{user_id}/enable", post(users::enable_user))
        .route("/v1/users/import", post(users::import_users))
        .route(
            "/v1/users/{user_id}/roles",
            get(users::get_user_roles).put(roles::replace_user_roles),
        )
        .route("/v1/roles", get(roles::list_roles).post(roles::create_role))
        .route(
            "/v1/roles/{role_id}",
            get(roles::get_role)
                .patch(roles::update_role)
                .delete(roles::delete_role),
        )
        .route("/v1/audit-events", get(audit::list_audit_events))
        .route("/v1/audit-events/{event_id}", get(audit::get_audit_event))
        .route(
            "/v1/admin/diagnostics",
            get(diagnostics::get_operational_diagnostics),
        )
        .with_state(state.clone())
        .layer(from_fn_with_state(metrics_state, metrics::middleware))
        .layer(from_fn_with_state(state.clone(), request_body_limit))
        .layer(from_fn_with_state(state.clone(), shutdown_admission))
        .layer(from_fn_with_state(state.clone(), request_context));

    let origins = state.allowed_origins();
    if origins.is_empty() {
        app
    } else {
        app.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins.iter().cloned()))
                .allow_methods(AllowMethods::list([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                ]))
                .allow_headers(AllowHeaders::list([
                    axum::http::header::ACCEPT,
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::IF_MATCH,
                    HeaderName::from_static("idempotency-key"),
                ]))
                .allow_credentials(state.allow_credentials()),
        )
    }
}

/// Entry is the HTTP acceptance point: requests admitted before begin retain
/// their existing handler/body deadlines; shutdown does not cancel them.
async fn shutdown_admission(
    State(state): State<HttpState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if state.is_shutting_down()
        && !matches!(
            request.uri().path(),
            "/health/live" | "/health/ready" | "/metrics" | "/version"
        )
    {
        let request_id = request
            .extensions()
            .get::<RequestContext>()
            .map(|context| context.request_id().to_owned())
            .unwrap_or_else(|| format!("req_{}", Uuid::now_v7()));
        return auth_error_response(
            ErrorCode::ServiceUnavailable,
            "server is shutting down".to_owned(),
            request_id,
        );
    }
    next.run(request).await
}

async fn request_body_limit(
    State(state): State<HttpState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let request_id = parts
        .extensions
        .get::<RequestContext>()
        .map(|context| context.request_id().to_owned())
        .unwrap_or_else(|| format!("req_{}", Uuid::now_v7()));
    let deadline = tokio::time::Instant::now() + state.request_timeout();
    let body_timeout = state
        .request_body_timeout()
        .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
    match tokio::time::timeout(body_timeout, to_bytes(body, state.max_request_body_bytes())).await {
        Err(_) => auth_error_response(
            // A deliberate server-side deadline is an availability outcome,
            // not an unexpected internal failure; the registry marks
            // SERVICE_UNAVAILABLE retryable.
            ErrorCode::ServiceUnavailable,
            "request body read timed out".to_owned(),
            request_id,
        ),
        Ok(Ok(bytes)) => match tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            next.run(Request::from_parts(parts, Body::from(bytes))),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => auth_error_response(
                // The public error registry has no timeout-specific code.
                // SERVICE_UNAVAILABLE (503, retryable) expresses a deliberate
                // request deadline far better than an internal failure.
                ErrorCode::ServiceUnavailable,
                "request handler timed out".to_owned(),
                request_id,
            ),
        },
        Ok(Err(error))
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>()) =>
        {
            auth_error_response(
                ErrorCode::PayloadTooLarge,
                "request body exceeds the configured limit".to_owned(),
                request_id,
            )
        }
        Ok(Err(_)) => auth_error_response(
            ErrorCode::InternalError,
            "failed to read request body".to_owned(),
            request_id,
        ),
    }
}

async fn request_context(
    State(state): State<HttpState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let request_id = format!("req_{}", Uuid::now_v7());
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let source_ip = state.resolve_source_ip(
        request.headers(),
        request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|value| value.0),
    );
    request.extensions_mut().insert(RequestContext {
        request_id: request_id.clone(),
        source_ip,
    });
    let span = tracing::info_span!(
        "http.request",
        request_id = %request_id,
        method = %method,
        path = %path,
        status = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    let started = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    span.record("status", response.status().as_u16());
    span.record("duration_ms", started.elapsed().as_millis() as u64);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(X_REQUEST_ID, value);
    }
    response
}

async fn live() -> Response {
    (
        StatusCode::OK,
        Json(HealthBody {
            status: "live",
            failures: Vec::new(),
        }),
    )
        .into_response()
}

async fn ready(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
) -> Response {
    if state.is_shutting_down() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: "SERVICE_UNAVAILABLE",
                    message: "service is not ready",
                    request_id: context.request_id().to_owned(),
                    details: ErrorDetails {
                        failures: vec!["shutdown"],
                    },
                },
            }),
        )
            .into_response();
    }
    let probes: Vec<&dyn HealthProbe> = state
        .probes
        .iter()
        .map(|probe| probe.as_ref() as &dyn HealthProbe)
        .collect();
    let report = check_readiness(&probes).await;
    let failures: Vec<&'static str> = report.failures().iter().map(|(name, _)| *name).collect();

    if report.is_ready() {
        (
            StatusCode::OK,
            Json(HealthBody {
                status: "ready",
                failures,
            }),
        )
            .into_response()
    } else {
        let request_id = context.request_id().to_owned();
        // The failure reason stays in the log; the public body names the
        // dependency only (specification §21.8).
        for (name, reason) in report.failures() {
            tracing::warn!(
                event = "health.not_ready",
                request_id = %request_id,
                dependency = name,
                reason = reason
            );
        }
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: "SERVICE_UNAVAILABLE",
                    message: "service is not ready",
                    request_id,
                    details: ErrorDetails { failures },
                },
            }),
        )
            .into_response()
    }
}

async fn version(State(state): State<HttpState>) -> Response {
    (
        StatusCode::OK,
        Json(VersionBody {
            service: state.service,
            version: state.version,
            protocol_major: state.protocol_major,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorCode, HttpState, decode_user_import, page_limit, parse_if_match, parse_uuid_v7, router,
    };
    use axum::Extension;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::response::IntoResponse as _;
    use axum::routing::get;
    use http_body_util::BodyExt as _;
    use ipnet::IpNet;
    use orbisync_domain::SystemClock;
    use std::future::Future as _;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    include!("shutdown_tests.rs");

    #[test]
    fn uuid_v7_requires_canonical_lowercase_text() {
        let value = uuid::Uuid::now_v7().hyphenated().to_string();
        assert!(parse_uuid_v7(&value).is_ok());
        assert_eq!(
            parse_uuid_v7(&value.to_uppercase()),
            Err(ErrorCode::InvalidRequest)
        );
        assert_eq!(
            parse_uuid_v7("550e8400-e29b-41d4-a716-446655440000"),
            Err(ErrorCode::InvalidRequest)
        );
    }

    #[test]
    fn revision_and_page_limits_are_strict() {
        assert_eq!(parse_if_match("\"19\""), Ok(19));
        assert_eq!(parse_if_match("19"), Err(ErrorCode::InvalidRequest));
        assert_eq!(page_limit(None), Ok(50));
        assert_eq!(page_limit(Some(200)), Ok(200));
        assert_eq!(page_limit(Some(0)), Err(ErrorCode::InvalidRequest));
    }

    #[test]
    fn trusted_proxies_getter_keeps_legacy_type_and_values() {
        let configured = vec!["10.0.0.0/8".to_owned(), " 192.0.2.1 ".to_owned()];
        let state = test_state().with_trusted_proxies(configured.clone());

        // The explicit annotation is a compile-time compatibility check for
        // existing callers of the public accessor.
        let legacy: Arc<Vec<String>> = state.trusted_proxies();
        assert_eq!(legacy.as_ref(), &configured);
    }

    #[test]
    fn trusted_proxy_networks_getter_exposes_typed_networks() {
        let networks = vec![
            "10.0.0.0/8".parse::<IpNet>().expect("IPv4 network"),
            "2001:db8::/32".parse::<IpNet>().expect("IPv6 network"),
        ];
        let state = test_state().with_trusted_proxy_networks(networks.clone());

        assert_eq!(state.trusted_proxy_networks().as_ref(), &networks);
        assert_eq!(
            state.trusted_proxies().as_ref(),
            &vec!["10.0.0.0/8".to_owned(), "2001:db8::/32".to_owned()]
        );
    }

    #[test]
    fn composition_root_wiring_order_preserves_configured_trusted_proxies() {
        // The composition root parses the comma-separated config value into
        // typed networks, wires them through `with_trusted_proxy_networks`,
        // and then restates the original entries through the legacy
        // `with_trusted_proxies` builder. A bare configured host must keep
        // its original string instead of a synthesized prefix, and the trust
        // decision must keep the parsed networks.
        let configured = "10.0.0.1, 192.168.0.0/16";
        let entries: Vec<String> = configured
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        let networks: Vec<IpNet> = entries
            .iter()
            .map(|entry| {
                if entry.contains('/') {
                    entry.parse().expect("validated CIDR")
                } else {
                    let ip: std::net::IpAddr = entry.parse().expect("validated address");
                    let prefix = if ip.is_ipv4() { 32 } else { 128 };
                    IpNet::new(ip, prefix).expect("validated host prefix")
                }
            })
            .collect();

        let state = test_state()
            .with_trusted_proxy_networks(networks.clone())
            .with_trusted_proxies(entries.clone());

        assert_eq!(state.trusted_proxies().as_ref(), &entries);
        assert_eq!(state.trusted_proxy_networks().as_ref(), &networks);
    }

    #[test]
    fn trusted_proxy_resolution_uses_typed_networks_and_rightmost_chain() {
        let state = test_state()
            .with_trusted_proxies(vec!["10.0.0.0/8".to_owned(), "2001:db8::/32".to_owned()]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));
        assert_eq!(
            state.resolve_source_ip(&headers, Some("10.12.0.4:443".parse().expect("peer"))),
            Some("198.51.100.7".to_owned())
        );

        // An address outside the configured CIDR cannot use the forwarded value.
        assert_eq!(
            state.resolve_source_ip(&headers, Some("192.0.2.4:443".parse().expect("peer"))),
            Some("192.0.2.4".to_owned())
        );

        // Walk from the peer side: trusted hops are skipped until the first
        // untrusted address, rather than unconditionally choosing left-most.
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.7, 10.1.0.2, 2001:db8::2"),
        );
        assert_eq!(
            state.resolve_source_ip(&headers, Some("[2001:db8::1]:443".parse().expect("peer"))),
            Some("198.51.100.7".to_owned())
        );

        // A malformed chain and a missing peer both fail closed.
        headers.insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        assert_eq!(
            state.resolve_source_ip(&headers, Some("10.1.0.2:443".parse().expect("peer"))),
            Some("10.1.0.2".to_owned())
        );
        assert_eq!(state.resolve_source_ip(&headers, None), None);
    }

    #[test]
    fn duplicate_xff_fields_are_combined_in_wire_order_before_walking() {
        let state = test_state().with_trusted_proxies(vec!["10.0.0.0/8".to_owned()]);
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));
        headers.append("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));

        // The left-most field is attacker-controlled; the proxy-appended
        // right-most field is the observed client address and must win.
        assert_eq!(
            state.resolve_source_ip(&headers, Some("10.0.0.4:443".parse().expect("peer"))),
            Some("203.0.113.9".to_owned())
        );
    }

    #[test]
    fn multiple_untrusted_xff_addresses_return_the_nearest_untrusted_hop() {
        let state = test_state().with_trusted_proxies(vec!["10.0.0.0/8".to_owned()]);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.7, 203.0.113.8, 10.1.0.2"),
        );

        assert_eq!(
            state.resolve_source_ip(&headers, Some("10.0.0.4:443".parse().expect("peer"))),
            Some("203.0.113.8".to_owned())
        );
    }

    #[test]
    fn trusted_proxy_exact_ip_family_zero_and_max_prefixes_are_supported() {
        let state = test_state()
            .with_trusted_proxies(vec!["10.0.0.1".to_owned(), "2001:db8::/128".to_owned()]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(
            state.resolve_source_ip(&headers, Some("10.0.0.1:443".parse().expect("peer"))),
            Some("203.0.113.9".to_owned())
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("2001:db8::9"));
        assert_eq!(
            state.resolve_source_ip(&headers, Some("[2001:db8::]:443".parse().expect("peer"))),
            Some("2001:db8::9".to_owned())
        );

        // /0 is accepted as a typed network. With every IPv4 hop trusted and
        // no untrusted client address in the chain, resolution falls back to
        // the transport peer rather than guessing.
        let zero_state = test_state().with_trusted_proxies(vec!["0.0.0.0/0".to_owned()]);
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 192.0.2.1"),
        );
        assert_eq!(
            zero_state.resolve_source_ip(&headers, Some("192.0.2.1:443".parse().expect("peer"))),
            Some("192.0.2.1".to_owned())
        );

        // A valid forwarded chain cannot be trusted without a transport peer.
        assert_eq!(state.resolve_source_ip(&headers, None), None);

        // An all-trusted IPv6 chain falls back to the transport peer rather
        // than inventing a client address.
        let ipv6_state = test_state().with_trusted_proxies(vec!["2001:db8::/32".to_owned()]);
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("2001:db8::2, 2001:db8::3"),
        );
        assert_eq!(
            ipv6_state
                .resolve_source_ip(&headers, Some("[2001:db8::1]:443".parse().expect("peer")),),
            Some("2001:db8::1".to_owned())
        );
    }

    fn test_state() -> HttpState {
        HttpState::new(
            Vec::new(),
            "test",
            "test",
            1,
            Arc::new(SystemClock::new()),
            900,
            2_592_000,
            b"test-refresh-key".to_vec(),
        )
    }

    #[tokio::test]
    async fn configured_body_limit_returns_413_and_allows_smaller_body() {
        let app = router(test_state().with_max_request_body_bytes(4));
        let rejected = app
            .clone()
            .oneshot(
                Request::post("/health/live")
                    .body(Body::from("12345"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(rejected.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = rejected
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(body[0], b'{');
        assert!(String::from_utf8_lossy(&body).contains("PAYLOAD_TOO_LARGE"));

        let accepted = app
            .oneshot(
                Request::post("/health/live")
                    .body(Body::from("1234"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(accepted.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn body_at_the_configured_two_mebibyte_limit_is_not_rejected() {
        let limit = 2_097_152;
        let response = router(test_state().with_max_request_body_bytes(limit))
            .oneshot(
                Request::post("/health/live")
                    .body(Body::from(vec![b'x'; limit]))
                    .expect("request"),
            )
            .await
            .expect("response");

        // The body was accepted by the limit middleware; the route rejects
        // POST independently because `/health/live` is GET-only.
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    struct FailingBody;

    struct SlowBody {
        delay: std::pin::Pin<Box<tokio::time::Sleep>>,
        yielded: bool,
    }

    impl SlowBody {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                delay: Box::pin(tokio::time::sleep(delay)),
                yielded: false,
            }
        }
    }

    impl http_body::Body for SlowBody {
        type Data = axum::body::Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            if self.yielded {
                return std::task::Poll::Ready(None);
            }
            if self.delay.as_mut().poll(cx).is_pending() {
                return std::task::Poll::Pending;
            }
            self.yielded = true;
            std::task::Poll::Ready(Some(Ok(http_body::Frame::data(
                axum::body::Bytes::from_static(b"slow"),
            ))))
        }
    }

    impl http_body::Body for FailingBody {
        type Data = axum::body::Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "simulated client disconnect",
            ))))
        }
    }

    #[tokio::test]
    async fn body_read_failure_is_not_reported_as_413() {
        let response = router(test_state().with_max_request_body_bytes(4))
            .oneshot(
                Request::post("/health/live")
                    .body(Body::new(FailingBody))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("INTERNAL_ERROR"));
        assert!(!String::from_utf8_lossy(&body).contains("PAYLOAD_TOO_LARGE"));
    }

    #[tokio::test]
    async fn slow_body_read_fails_closed_at_body_deadline() {
        let response =
            router(test_state().with_request_body_timeout(std::time::Duration::from_millis(20)))
                .oneshot(
                    Request::post("/health/live")
                        .body(Body::new(SlowBody::new(std::time::Duration::from_millis(
                            100,
                        ))))
                        .expect("request"),
                )
                .await
                .expect("response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("SERVICE_UNAVAILABLE"));
        assert!(text.contains("request body read timed out"));
    }

    #[derive(Clone)]
    struct CancellationProbe(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for CancellationProbe {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    async fn deliberately_slow_handler(
        Extension(_probe): Extension<CancellationProbe>,
    ) -> super::Response {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        StatusCode::NO_CONTENT.into_response()
    }

    #[tokio::test]
    async fn slow_handler_is_bounded_by_request_deadline() {
        let cancellation_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = test_state().with_request_timeout(std::time::Duration::from_millis(100));
        let app = Router::new()
            .route("/slow", get(deliberately_slow_handler))
            .with_state(state.clone())
            .layer(from_fn_with_state(state, super::request_body_limit));
        let started = std::time::Instant::now();
        let response = app
            .oneshot(
                Request::get("/slow")
                    .extension(CancellationProbe(Arc::clone(&cancellation_observed)))
                    .body(Body::new(SlowBody::new(std::time::Duration::from_millis(
                        60,
                    ))))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("request handler timed out"));
        assert!(cancellation_observed.load(std::sync::atomic::Ordering::Acquire));
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(140),
            "request exceeded one deadline: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_keeps_liveness_but_lowers_readiness() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let app = router(test_state().with_shutdown_flag(Arc::clone(&flag)));
        flag.store(true, std::sync::atomic::Ordering::Release);
        let ready = app
            .clone()
            .oneshot(
                Request::get("/health/ready")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        let instance = app
            .clone()
            .oneshot(
                Request::post("/v1/instances")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"world_id":"not-used"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(instance.status(), StatusCode::SERVICE_UNAVAILABLE);
        let live = app
            .oneshot(
                Request::get("/health/live")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(live.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cors_is_explicitly_allowlisted() {
        let empty = router(test_state());
        let response = empty
            .oneshot(
                Request::get("/health/live")
                    .header("origin", "https://allowed.example")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );

        let allowed = router(
            test_state()
                .with_allowed_origins(vec![HeaderValue::from_static("https://allowed.example")])
                .with_allow_credentials(false),
        );
        let response = allowed
            .clone()
            .oneshot(
                Request::get("/health/live")
                    .header("origin", "https://allowed.example")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://allowed.example")
        );

        let response = allowed
            .oneshot(
                Request::get("/health/live")
                    .header("origin", "https://other.example")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }

    #[test]
    fn configured_page_maximum_is_enforced() {
        let state = test_state().with_page_limits(3, 3);
        assert_eq!(state.page_limit(None), Ok(3));
        assert_eq!(state.page_limit(Some(4)), Err(ErrorCode::InvalidRequest));
    }

    #[test]
    fn revision_mismatch_is_412() {
        assert_eq!(
            ErrorCode::RevisionMismatch.status(),
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(ErrorCode::RevisionMismatch.as_str(), "REVISION_MISMATCH");
    }

    #[test]
    fn csv_import_is_strict_and_row_numbered() {
        let rows =
            decode_user_import(b"login_id,display_name\nada,Ada Lovelace\n").expect("valid CSV");
        assert_eq!(rows[0].row, 2);
        assert_eq!(rows[0].login_id, "ada");
        assert_eq!(
            decode_user_import(b"display_name,login_id\nAda,ada\n"),
            Err(ErrorCode::InvalidRequest)
        );
        assert_eq!(
            decode_user_import(&vec![b'x'; 1_048_577]),
            Err(ErrorCode::InvalidRequest)
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod golden_refresh_digest_tests {
    use super::refresh_token_digest;
    use orbisync_application::SecretString;
    use std::path::PathBuf;

    fn hex_decode(s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len() / 2);
        for i in (0..s.len()).step_by(2) {
            let byte = u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex");
            out.push(byte);
        }
        out
    }

    fn load_vectors() -> serde_json::Value {
        let path = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"),
        )
        .join("../../test-vectors/rest/v1/refresh-digest-vectors.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        serde_json::from_str(&raw).expect("refresh vectors must be valid JSON")
    }

    #[test]
    fn golden_refresh_digest_matches_vector() {
        let v = load_vectors();
        let vectors = v["vectors"].as_array().expect("vectors array");
        for entry in vectors {
            let key = entry["key"].as_str().expect("key");
            let token = entry["token"].as_str().expect("token");
            let expected_hex = entry["digest_hex"].as_str().expect("digest_hex");
            let digest = refresh_token_digest(key.as_bytes(), &SecretString::new(token));
            let expected = hex_decode(expected_hex);
            assert_eq!(expected.len(), 32, "expected digest must be 32 bytes");
            let mut exp_arr = [0u8; 32];
            exp_arr.copy_from_slice(&expected);
            assert_eq!(
                digest, exp_arr,
                "refresh digest golden vector must match for key={key:?} token={token:?} (M4)"
            );
        }
    }

    #[test]
    fn golden_refresh_digest_is_not_plain_sha256() {
        // The first vector's digest must differ from plain SHA-256 of the token.
        // This proves the golden value is HMAC, not plain, so M4 (swap to plain) is caught.
        let v = load_vectors();
        let entry = &v["vectors"][0];
        let token = entry["token"].as_str().expect("token");
        let expected_hex = entry["digest_hex"].as_str().expect("digest_hex");
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let plain = hasher.finalize();
        let plain_hex: String = plain.iter().map(|b| format!("{b:02x}")).collect();
        assert_ne!(
            plain_hex, expected_hex,
            "golden digest must differ from plain SHA-256 (M4)"
        );
    }

    #[test]
    fn golden_refresh_digest_different_keys_produce_different_digests() {
        let v = load_vectors();
        let vectors = v["vectors"].as_array().expect("vectors array");
        // Vectors use different keys, so digests must differ. If implementation
        // ignored the key (plain SHA-256), they would be equal when tokens equal.
        // Our second vector uses key "key" with token "The quick brown fox".
        // First vector uses different key+token, but we test key sensitivity
        // by recomputing with a different key for same token as entry 1.
        let entry = &vectors[1];
        let token = entry["token"].as_str().expect("token");
        let key = entry["key"].as_str().expect("key");
        let expected_hex = entry["digest_hex"].as_str().expect("digest_hex");
        let other_key = "test-only-refresh-golden-v1-TEST-NOT-PROD-32B!!";
        let other_digest = refresh_token_digest(other_key.as_bytes(), &SecretString::new(token));
        let other_hex: String = other_digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_ne!(
            other_hex, expected_hex,
            "different keys must produce different digests"
        );
        // Also verify the golden entry itself differs from other_key's digest
        let _ = key; // keep variable used
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod idempotency_hmac_tests {
    use super::{idempotency_request_digest, put_len_prefixed};
    use orbisync_domain::UserId;
    use sha2::{Digest as _, Sha256};
    use uuid::Uuid;

    fn digest_hex(bytes: &[u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn stored_value_differs_from_plain_sha256() {
        // This is the main regression for P2-C1: DB value must NOT equal plain SHA-256(actor||current||new)
        let key = b"test-idempotency-hmac-key-32b!!-P2-C1";
        let actor = UserId::new(Uuid::now_v7()).expect("user id");
        let current = b"currentPass123!";
        let new = b"NewSecurePass123!@#";
        let hmac = idempotency_request_digest(key, "changePassword", Some(actor), &[current, new]);
        // Plain SHA-256 of concatenated bytes (the old vulnerable method)
        let mut hasher = Sha256::new();
        hasher.update(actor.to_string().as_bytes());
        hasher.update(current);
        hasher.update(new);
        let plain = hasher.finalize();
        let plain_arr: [u8; 32] = plain.into();
        assert_ne!(
            hmac,
            plain_arr,
            "HMAC digest must differ from plain SHA-256(actor||current||new); got same value {} (mutation: HMAC->SHA256 would be invisible)",
            digest_hex(&hmac)
        );
    }

    #[test]
    fn field_boundary_no_collision() {
        let key = b"test-idempotency-hmac-key-32b!!-P2-C1";
        let actor = UserId::new(Uuid::now_v7()).expect("u");
        let a1 = idempotency_request_digest(key, "changePassword", Some(actor), &[b"ab", b"c"]);
        let a2 = idempotency_request_digest(key, "changePassword", Some(actor), &[b"a", b"bc"]);
        assert_ne!(
            a1, a2,
            "(\"ab\",\"c\") and (\"a\",\"bc\") must produce different digests via length prefix"
        );
    }

    #[test]
    fn different_keys_differ_same_key_stable() {
        let key1 = b"test-idempotency-key-1-32b!!-AAAA";
        let key2 = b"test-idempotency-key-2-32b!!-BBBB";
        let actor = UserId::new(Uuid::now_v7()).expect("u");
        let fields: &[&[u8]] = &[b"current123", b"new123!@#"];
        let d1 = idempotency_request_digest(key1, "changePassword", Some(actor), fields);
        let d1_again = idempotency_request_digest(key1, "changePassword", Some(actor), fields);
        let d2 = idempotency_request_digest(key2, "changePassword", Some(actor), fields);
        assert_eq!(d1, d1_again, "same key+same request must be stable");
        assert_ne!(d1, d2, "different keys must produce different digests");
    }

    #[test]
    fn put_len_prefixed_is_length_prefix() {
        let mut out = Vec::new();
        put_len_prefixed(&mut out, b"ab");
        // first 8 bytes are len as u64 BE, then 2 bytes payload
        assert_eq!(out.len(), 8 + 2);
        let len = u64::from_be_bytes(out[0..8].try_into().unwrap());
        assert_eq!(len, 2);
        assert_eq!(&out[8..], b"ab");
        let mut out2 = Vec::new();
        put_len_prefixed(&mut out2, b"a");
        put_len_prefixed(&mut out2, b"bc");
        assert_ne!(out, out2);
    }

    #[test]
    #[should_panic(expected = "idempotency_hmac_key must not be empty")]
    fn missing_key_panics() {
        let actor = UserId::new(Uuid::now_v7()).expect("u");
        let _digest = idempotency_request_digest(b"", "changePassword", Some(actor), &[b"a", b"b"]);
    }

    #[test]
    fn operation_and_actor_are_part_of_digest() {
        let key = b"test-idempotency-hmac-key-32b!!-P2-C1";
        let actor1 = UserId::new(Uuid::now_v7()).expect("u");
        let actor2 = UserId::new(Uuid::now_v7()).expect("u");
        let fields: &[&[u8]] = &[b"pw1", b"pw2"];
        let d1 = idempotency_request_digest(key, "changePassword", Some(actor1), fields);
        let d2 = idempotency_request_digest(key, "changePassword", Some(actor2), fields);
        assert_ne!(d1, d2, "different actors must differ");
        let d3 = idempotency_request_digest(key, "resetUserPassword", Some(actor1), fields);
        assert_ne!(d1, d3, "different operations must differ");
    }

    #[test]
    fn rotation_same_request_different_key_is_not_idempotent() {
        // Simulates key rotation: same logical request with different server key
        // must NOT be considered the same `request_hash` – caller will get
        // IDEMPOTENCY_KEY_REUSED only if body/actor/operation truly differ,
        // but same body with new key is intentionally a different digest.
        // This demonstrates that rotation requires a 24h window where both keys
        // are accepted – we choose dual-key window over key_id column (see PR).
        let old_key = b"old-idempotency-key-32b!!-ROT1";
        let new_key = b"new-idempotency-key-32b!!-ROT2";
        let actor = UserId::new(Uuid::now_v7()).expect("u");
        let fields: &[&[u8]] = &[b"cur", b"new"];
        let old_digest = idempotency_request_digest(old_key, "changePassword", Some(actor), fields);
        let new_digest = idempotency_request_digest(new_key, "changePassword", Some(actor), fields);
        assert_ne!(old_digest, new_digest, "rotation changes digest");
        // Stable within same key
        let new_again = idempotency_request_digest(new_key, "changePassword", Some(actor), fields);
        assert_eq!(new_digest, new_again);
    }
}
