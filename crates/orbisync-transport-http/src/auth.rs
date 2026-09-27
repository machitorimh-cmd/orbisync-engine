//! Authentication HTTP handlers (`POST /v1/auth/login`, `POST /v1/auth/refresh`, `POST /v1/realtime/tickets`).

use axum::Json;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use orbisync_application::{
    CreateRealtimeTicketCommand, RefreshRotationResult, RefreshTokenReplacement, RequestId,
    RotateRefreshTokenCommand, SecretString,
};
use orbisync_domain::LoginId;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use uuid::Uuid;

use crate::{ErrorCode, HttpState, RequestContext};

/// Checks the existing role-assignment permission for local operator settings.
/// The launcher additionally requires its process-local capability and origin.
pub async fn administration_access(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    match crate::authorize(&state, &headers, context.request_id(), "admin.roles.assign").await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

/// Body for `POST /v1/auth/login`.
#[derive(Deserialize)]
pub struct LoginRequest {
    /// Login identifier (1..128 non-control).
    pub login_id: String,
    /// Plain password (secret).
    pub password: String,
}

impl core::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("login_id", &self.login_id)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: u64,
}

/// Body for `POST /v1/auth/refresh`.
#[derive(Deserialize)]
pub struct RefreshRequest {
    /// Opaque refresh token previously issued by login or refresh.
    pub refresh_token: String,
}

impl core::fmt::Debug for RefreshRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RefreshRequest")
            .field("refresh_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct TokenPairResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: u64,
}

#[derive(Debug, Serialize)]
struct TicketResponse {
    realtime_ticket: String,
    expires_in: u64,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    request_id: String,
    details: serde_json::Value,
}

fn error_response(code: ErrorCode, message: String, request_id: String) -> Response {
    let status = code.status();
    let body = ErrorEnvelope {
        error: ErrorBody {
            code: code.as_str(),
            message,
            request_id,
            details: serde_json::json!({}),
        },
    };
    (status, Json(body)).into_response()
}

fn login_rate_limited_response(request_id: String, retry_after_seconds: u64) -> Response {
    let mut response = error_response(
        ErrorCode::RateLimited,
        "rate limited".to_owned(),
        request_id,
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// `POST /v1/auth/login`.
///
/// All business orchestration is delegated to `LoginService` (P2-C4, `orbisync-identity`).
/// The handler only parses transport values, resolves the trusted-proxy-filtered
/// source IP (ADR-009), and maps the application outcome to HTTP. Failures
/// for missing account, wrong password, disabled/locked all map to the same
/// `AUTHENTICATION_REQUIRED` (401) without distinguishing the reason
/// (enumeration resistance).
pub async fn login(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id = context.request_id().to_owned();
    let body = match read_json(body, &request_id) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    if !state
        .enabled_auth_methods()
        .contains(&orbisync_domain::AuthMethod::Local)
    {
        return method_disabled_error_response(request_id);
    }

    let source_ip = state.resolve_source_ip(&headers, connect_info.map(|value| value.0.0));
    let source_ip_for_limiter = source_ip.as_deref().and_then(|value| value.parse().ok());
    let decision = state
        .login_rate_limiter()
        .check_and_record(source_ip_for_limiter, state.now().as_offset_date_time());
    if !decision.allowed {
        crate::metrics::record_rate_limit_rejected(
            &state,
            orbisync_application::metrics::RateLimitScope::Ip,
        );
        return login_rate_limited_response(request_id, decision.retry_after_seconds.unwrap_or(1));
    }

    let Some(service) = state.login_service() else {
        return error_response(
            ErrorCode::InternalError,
            "login service unavailable".to_owned(),
            request_id,
        );
    };

    let login_id = match LoginId::new(body.login_id.clone()) {
        Ok(v) => v,
        Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
    };

    let secret = SecretString::new(body.password);
    let req_id = match RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    match service.login(login_id, secret, req_id, source_ip).await {
        Ok(result) => {
            let response = LoginResponse {
                access_token: result.access_token.expose_secret().to_owned(),
                refresh_token: result.refresh_token.expose_secret().to_owned(),
                token_type: "Bearer".to_owned(),
                expires_in: result.expires_in,
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(error) => {
            use orbisync_application::ApplicationErrorKind;
            match error.kind() {
                ApplicationErrorKind::Unauthenticated => {
                    crate::metrics::record_login_failure(&state);
                    error_response(
                        ErrorCode::AuthenticationRequired,
                        "authentication failed".to_owned(),
                        request_id,
                    )
                }
                ApplicationErrorKind::RateLimited => {
                    crate::metrics::record_rate_limit_rejected(
                        &state,
                        orbisync_application::metrics::RateLimitScope::PasswordHash,
                    );
                    error_response(
                        ErrorCode::RateLimited,
                        "rate limited".to_owned(),
                        request_id,
                    )
                }
                _ => {
                    tracing::warn!(
                        event = "auth.login_failed",
                        request_id = %request_id,
                        error_kind = %error.kind(),
                        "login failed"
                    );
                    error_response(
                        ErrorCode::InternalError,
                        "internal error".to_owned(),
                        request_id,
                    )
                }
            }
        }
    }
}

/// `POST /v1/auth/refresh` – rotates a refresh token (CR-H, ADR-008).
///
/// `security: []` – the refresh token itself is the credential and is supplied
/// in the JSON body. No Bearer header is required. The handler validates the
/// token via HMAC digest, calls `RefreshTokenRotationStore::rotate`, and maps
/// the domain outcomes to HTTP. Token and digest material is never logged.
pub async fn refresh(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    Json(body): Json<RefreshRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    if body.refresh_token.trim().is_empty() {
        return error_response(
            ErrorCode::AuthenticationRequired,
            "authentication required".to_owned(),
            request_id,
        );
    }

    let Some(tokens) = state.token_service() else {
        return error_response(
            ErrorCode::InternalError,
            "token service unavailable".to_owned(),
            request_id,
        );
    };
    let Some(rotation_store) = state.refresh_rotation_store() else {
        return error_response(
            ErrorCode::InternalError,
            "refresh service unavailable".to_owned(),
            request_id,
        );
    };

    let now = state.clock().now();
    let refresh_hmac_key = state.refresh_token_hmac_key().to_vec();
    let presented = SecretString::new(body.refresh_token);
    let presented_digest = crate::refresh_token_digest(&refresh_hmac_key, &presented);

    let refresh_ttl = state.refresh_token_ttl_seconds();
    let refresh_ttl_millis = match refresh_ttl.checked_mul(1_000) {
        Some(v) => v,
        None => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let expires_at = match now.checked_add_millis(refresh_ttl_millis as i64) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let new_refresh = crate::generate_refresh_token();
    let replacement_digest = crate::refresh_token_digest(&refresh_hmac_key, &new_refresh);

    let req_id = match RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let cmd = RotateRefreshTokenCommand {
        presented_digest,
        replacement: RefreshTokenReplacement {
            token_id: Uuid::now_v7().to_string(),
            digest: replacement_digest,
            issued_at: now,
            expires_at,
        },
        now,
        request_id: req_id,
    };

    let outcome = match rotation_store
        .rotate_with_source_ip(cmd, context.source_ip().map(str::to_owned))
        .await
    {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id.clone(),
            );
        }
    };

    match outcome {
        RefreshRotationResult::Rotated {
            user_id,
            session_id,
            absolute_deadline,
        } => {
            let (access, expires_in) =
                match tokens.issue_with_deadline(user_id, session_id, now, absolute_deadline) {
                    Ok(t) => t,
                    Err(orbisync_identity::token::AccessTokenError::InvalidToken) => {
                        return error_response(
                            ErrorCode::AuthenticationRequired,
                            "authentication required".to_owned(),
                            request_id,
                        );
                    }
                    Err(_) => {
                        return error_response(
                            ErrorCode::InternalError,
                            "token issuance failed".to_owned(),
                            request_id,
                        );
                    }
                };
            let response = TokenPairResponse {
                access_token: access.expose_secret().to_owned(),
                refresh_token: new_refresh.expose_secret().to_owned(),
                token_type: "Bearer".to_owned(),
                expires_in,
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        RefreshRotationResult::ReuseDetected | RefreshRotationResult::Rejected => error_response(
            ErrorCode::AuthenticationRequired,
            "authentication required".to_owned(),
            request_id,
        ),
    }
}

/// `POST /v1/realtime/tickets`.
///
/// Validates `Authorization: Bearer <access token>` and issues a single-use,
/// opaque realtime ticket (RV-A C1). The ticket is a 32-byte random value
/// (base64url) whose HMAC-SHA-256 digest is stored with a 60-second TTL.
/// Consumption is atomic (`DELETE USING auth_sessions ... RETURNING`) and
/// checks that the session is `active` and not expired. The raw ticket and
/// digest are never logged.
pub async fn create_realtime_ticket(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    let request_id = context.request_id().to_owned();

    let (user_id, session_id, now) = match crate::authenticate(&state, &headers, &request_id).await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let Some(ticket_store) = state.realtime_ticket_store() else {
        tracing::warn!(
            event = "realtime.ticket_store_unwired",
            request_id = %request_id,
            "realtime ticket store not wired – fail-closed (C3)"
        );
        return error_response(
            ErrorCode::InternalError,
            "ticket service unavailable".to_owned(),
            request_id,
        );
    };
    // C5: per-user / per-session rate limit for ticket issuance (bounded retention).
    // Must be checked BEFORE generating the ticket or inserting a row, so that
    // a 429 does not increase storage. The limiter is keyed by both user and
    // session; exceeding either returns 429. The limiter is always present via
    // HttpState::new default, so no fail-open `if let Some` is used (C3 gate).
    let limiter = state.realtime_ticket_rate_limiter();
    let allowed = limiter.check_and_record(
        user_id.as_uuid(),
        session_id.as_uuid(),
        now.as_offset_date_time(),
    );
    if !allowed {
        tracing::warn!(
            event = "realtime.ticket_rate_limited",
            request_id = %request_id,
            "realtime ticket rate limit exceeded"
        );
        crate::metrics::record_rate_limit_rejected(
            &state,
            orbisync_application::metrics::RateLimitScope::User,
        );
        return error_response(
            ErrorCode::RateLimited,
            "rate limited".to_owned(),
            request_id,
        );
    }
    let hmac_key = state.realtime_ticket_hmac_key().to_vec();
    if hmac_key.is_empty() {
        tracing::warn!(
            event = "realtime.ticket_hmac_missing",
            request_id = %request_id,
            "realtime ticket HMAC key not configured"
        );
        return error_response(
            ErrorCode::InternalError,
            "ticket service unavailable".to_owned(),
            request_id,
        );
    }

    let raw_ticket = crate::generate_realtime_ticket();
    let digest = crate::realtime_ticket_digest(&hmac_key, &raw_ticket);
    // Ticket TTL is 60s per openapi `expires_in` andtoken.rs constant.
    let expires_at = match now.checked_add_millis(60_000) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "ticket expiry overflow".to_owned(),
                request_id,
            );
        }
    };
    let cmd = CreateRealtimeTicketCommand {
        token_digest: digest,
        session_id,
        user_id,
        issued_at: now,
        expires_at,
    };
    if let Err(err) = ticket_store.create(cmd).await {
        tracing::warn!(
            event = "realtime.ticket_create_failed",
            request_id = %request_id,
            error = %err,
            "failed to persist realtime ticket digest"
        );
        return error_response(
            ErrorCode::InternalError,
            "ticket issuance failed".to_owned(),
            request_id,
        );
    }

    let response = TicketResponse {
        realtime_ticket: raw_ticket.expose_secret().to_owned(),
        expires_in: 60,
    };
    (StatusCode::OK, Json(response)).into_response()
}

#[derive(Debug, Serialize)]
struct CurrentUserResponse {
    id: String,
    login_id: String,
    display_name: String,
    enabled: bool,
    revision: u64,
}

/// `GET /v1/auth/me`.
///
/// Returns the authenticated subject's own profile. No RBAC permission is
/// required beyond a valid access token – this is self-service, not
/// administration (unlike `GET /v1/users/{user_id}`, which requires
/// `admin.users.read`).
pub async fn me(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    let request_id = context.request_id().to_owned();
    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(query) = state.identity_query() else {
        return error_response(
            ErrorCode::InternalError,
            "identity query unavailable".to_owned(),
            request_id,
        );
    };
    match query.user(actor_id).await {
        Ok(Some(user)) => {
            let body = CurrentUserResponse {
                id: user.id.to_string(),
                login_id: user.login_id,
                display_name: user.display_name,
                enabled: user.status == orbisync_domain::UserStatus::Active,
                revision: user.revision,
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(None) => error_response(
            ErrorCode::ResourceNotFound,
            "user not found".to_owned(),
            request_id,
        ),
        Err(_) => error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        ),
    }
}

/// `POST /v1/auth/logout`.
///
/// Revokes the session bound to the presented access token via
/// `IdentityAdministrationService::logout` (`StoreSession` mutation, ADR-017).
/// Once revoked, a later `POST /v1/auth/refresh` with a refresh token that
/// belonged to this session is rejected because
/// `rotate_refresh_token_with_source_ip` joins `auth_sessions` and requires
/// `status = 'active'`. There is no request body: the OpenAPI contract for
/// this operation carries no requestBody and only `bearerAuth` security, so
/// unlike `POST /v1/auth/refresh` there is no client-supplied refresh token
/// to route through `RefreshTokenRotationStore` here; revoking the session
/// is the mechanism that invalidates it. Repeated calls are idempotent
/// (already-revoked and already-absent sessions both return 204).
pub async fn logout(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    let request_id = context.request_id().to_owned();
    let (_, session_id, _) = match crate::authenticate(&state, &headers, &request_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(repo) = state.identity_repository() else {
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id,
        );
    };
    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };
    let mut session = match repo.find_session(session_id).await {
        Ok(Some(session)) => session,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let req_id = match RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    if admin.logout(&mut session, req_id).await.is_err() {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// ADR-026 authentication methods
// ---------------------------------------------------------------------------

/// Body for `POST /v1/auth/guest`.
///
/// `deny_unknown_fields` is what turns a forged `role`, `permission`,
/// `user_id` or `owner` into a 400 rather than a silently ignored field. The
/// struct has no such member to ignore in the first place: the subject's id
/// and roles are decided entirely by the server.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRequest {}

/// Body for `POST /v1/auth/name`.
///
/// Carries the display name and nothing else, for the same reason as
/// [`GuestRequest`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameOnlyRequest {
    /// Visitor-supplied display name. A label, never an authentication factor:
    /// two visitors may choose the same one and stay distinct subjects.
    pub display_name: String,
}

/// Body for `POST /v1/auth/external`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRequest {
    /// Signed token issued by the configured provider.
    pub token: String,
}

impl core::fmt::Debug for ExternalRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExternalRequest")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Response of `GET /v1/auth/methods`.
#[derive(Debug, Serialize)]
struct AuthMethodsResponse {
    methods: Vec<&'static str>,
}

fn method_disabled_error_response(request_id: String) -> Response {
    // Identical whether the method is switched off or was never configured, so
    // the response cannot be used to probe the deployment's settings.
    error_response(
        ErrorCode::AuthMethodDisabled,
        "authentication method is not enabled".to_owned(),
        request_id,
    )
}

/// Maps an issued session onto the same response shape `login` returns.
///
/// Sharing the shape is what lets `/v1/auth/refresh`, `/v1/realtime/tickets`,
/// `/v1/auth/logout` and `/v1/auth/me` serve all four methods unchanged.
fn login_response(result: orbisync_application::LoginResult) -> Response {
    let response = LoginResponse {
        access_token: result.access_token.expose_secret().to_owned(),
        refresh_token: result.refresh_token.expose_secret().to_owned(),
        token_type: "Bearer".to_owned(),
        expires_in: result.expires_in,
    };
    (StatusCode::OK, Json(response)).into_response()
}

/// Maps an issue failure onto HTTP without revealing which check failed.
fn issue_error_response(
    state: &HttpState,
    error: &orbisync_application::ApplicationError,
    request_id: String,
) -> Response {
    use orbisync_application::ApplicationErrorKind;
    match error.kind() {
        ApplicationErrorKind::NotAuthorized => method_disabled_error_response(request_id),
        ApplicationErrorKind::DomainRule => error_response(
            ErrorCode::InvalidRequest,
            error.detail().to_owned(),
            request_id,
        ),
        ApplicationErrorKind::Unauthenticated => {
            crate::metrics::record_login_failure(state);
            error_response(
                ErrorCode::AuthenticationRequired,
                "authentication failed".to_owned(),
                request_id,
            )
        }
        ApplicationErrorKind::RateLimited => error_response(
            ErrorCode::RateLimited,
            "rate limited".to_owned(),
            request_id,
        ),
        _ => {
            tracing::warn!(
                event = "auth.subject_issue_failed",
                request_id = %request_id,
                error_kind = %error.kind(),
                "subject issue failed"
            );
            error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            )
        }
    }
}

/// `GET /v1/auth/methods` -- mode discovery (ADR-026 §7).
///
/// Reachable without authentication, because a client has to know which call
/// to make before it can authenticate at all. Only the method identifiers are
/// returned: issuer URLs, key locations and every other setting stay
/// server-side.
pub async fn auth_methods(
    State(state): State<HttpState>,
    Extension(_context): Extension<RequestContext>,
) -> Response {
    let response = AuthMethodsResponse {
        methods: state
            .enabled_auth_methods()
            .iter()
            .map(|method| method.as_str())
            .collect(),
    };
    (StatusCode::OK, Json(response)).into_response()
}

/// Unwraps a JSON body, turning a rejection into the standard error envelope.
///
/// Axum would otherwise answer a malformed or unexpected body with a bare 422
/// carrying no envelope, which is neither documented in the OpenAPI contract
/// nor parseable by a client that expects `error.code`. A body holding a field
/// the request has no business carrying -- `role`, `user_id`, `owner` -- lands
/// here, so a forged claim is answered with an explicit refusal rather than a
/// status nothing describes.
fn read_json<T>(
    body: Result<Json<T>, axum::extract::rejection::JsonRejection>,
    request_id: &str,
) -> Result<T, Box<Response>> {
    match body {
        Ok(Json(value)) => Ok(value),
        Err(rejection) => Err(Box::new(error_response(
            ErrorCode::InvalidRequest,
            rejection.body_text(),
            request_id.to_owned(),
        ))),
    }
}

/// Resolves the trusted-proxy-filtered source IP and the correlation id.
fn issue_context(
    state: &HttpState,
    headers: &HeaderMap,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    request_id: &str,
) -> Result<(Option<String>, RequestId), Box<Response>> {
    let source_ip = state.resolve_source_ip(headers, connect_info.map(|value| value.0.0));
    let req_id = RequestId::new(request_id.to_owned()).map_err(|_| {
        Box::new(error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id.to_owned(),
        ))
    })?;
    Ok((source_ip, req_id))
}

/// `POST /v1/auth/guest` -- anonymous participation (ADR-026 §3).
pub async fn guest(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<GuestRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id = context.request_id().to_owned();
    // A caller that sends nothing at all is fine: this request carries no
    // input, so insisting on an empty object would be ceremony. A caller that
    // *does* send a body must send a valid one -- accepting the request with
    // `Option` would turn a body claiming `role` or `user_id` into a silent
    // `None` and answer 200, which is precisely the "ignored rather than
    // rejected" behaviour the forged-field rule forbids.
    if let Err(rejection) = &body
        && !matches!(
            rejection,
            axum::extract::rejection::JsonRejection::MissingJsonContentType(_)
        )
    {
        return error_response(ErrorCode::InvalidRequest, rejection.body_text(), request_id);
    }
    let Some(service) = state.ephemeral_subject_service() else {
        return method_disabled_error_response(request_id);
    };
    let (source_ip, req_id) = match issue_context(&state, &headers, connect_info, &request_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match service.issue_guest(req_id, source_ip).await {
        Ok(result) => login_response(result),
        Err(error) => issue_error_response(&state, &error, request_id),
    }
}

/// `POST /v1/auth/name` -- participation with a supplied display name.
pub async fn name_only(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<NameOnlyRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id = context.request_id().to_owned();
    let body = match read_json(body, &request_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let Some(service) = state.ephemeral_subject_service() else {
        return method_disabled_error_response(request_id);
    };
    let (source_ip, req_id) = match issue_context(&state, &headers, connect_info, &request_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match service
        .issue_name_only(&body.display_name, req_id, source_ip)
        .await
    {
        Ok(result) => login_response(result),
        Err(error) => issue_error_response(&state, &error, request_id),
    }
}

/// `POST /v1/auth/external` -- signed token from a configured issuer.
pub async fn external(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<ExternalRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id = context.request_id().to_owned();
    let Some(service) = state.external_auth_service() else {
        return method_disabled_error_response(request_id);
    };
    let body = match read_json(body, &request_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let (source_ip, req_id) = match issue_context(&state, &headers, connect_info, &request_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match service.authenticate(&body.token, req_id, source_ip).await {
        Ok(result) => login_response(result),
        Err(error) => issue_error_response(&state, &error, request_id),
    }
}
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod realtime_ticket_fail_closed_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::IdentityRepository;
    use orbisync_domain::{LoginId, Timestamp};
    use orbisync_identity::{
        DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
        PasswordService, token::AccessTokenService,
    };
    use orbisync_testkit::{FakeIdentityStore, FakeRealtimeTicketStore, FixedClock};
    use std::sync::Arc;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::{HttpState, router};

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn token_service() -> Arc<AccessTokenService> {
        Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test-key-1",
            )
            .expect("token service"),
        )
    }

    struct NopRefreshCreationStore;
    #[async_trait::async_trait]
    impl orbisync_application::RefreshTokenCreationStore for NopRefreshCreationStore {
        async fn create(
            &self,
            _cmd: orbisync_application::CreateRefreshTokenCommand,
        ) -> Result<(), orbisync_application::IdentityPortError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn realtime_ticket_without_store_is_500_without_body() {
        // Public constructor HttpState without wiring realtime_ticket_store must be 500 fail-closed (C3).
        // This uses the public `router(HttpState)` constructor directly, not via main.rs,
        // so composition-root wiring cannot hide the defect.
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
            store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>;
        let dyn_store = DynIdentityAdministrationStore(admin_port);
        let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
        let admin = Arc::new(IdentityAdministrationService::new(
            Arc::new(dyn_store),
            Arc::new(dyn_clock),
            (*passwords).clone(),
        ));
        let (admin_user, admin_pw) = admin
            .bootstrap_administrator(
                LoginId::new("admin-tc").expect("login"),
                "Admin".to_owned(),
                orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                    .expect("req"),
            )
            .await
            .expect("bootstrap");
        let _ = admin_user;

        // Build state WITHOUT realtime_ticket_store (and without hmac key wired – default empty)
        // Use required HMAC env wiring but deliberately omit the store to test fail-closed.
        let login_tx: Arc<dyn orbisync_application::LoginTransactionStore> =
            store.clone() as Arc<dyn orbisync_application::LoginTransactionStore>;
        let login_service = Arc::new(orbisync_identity::LoginService::new(
            repo.clone(),
            login_tx,
            (*passwords).clone(),
            tokens.clone(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
            900,
            2_592_000,
        ));
        let state = HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        )
        .with_token_service(Arc::clone(&tokens))
        .with_identity_repository(repo.clone())
        .with_password_service(Arc::clone(&passwords))
        .with_identity_admin_service(Arc::clone(&admin))
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
        .with_login_service(login_service);
        // Intentionally NOT calling .with_realtime_ticket_store

        // Login to get token
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admin-tc", "password": admin_pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();

        // Call /v1/realtime/tickets – must be 500 INTERNAL_ERROR, not 200 with ticket
        let app = router(state.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "without store must be 500 fail-closed (C3)"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let body_str = json.to_string();
        assert!(
            !body_str.contains("realtime_ticket"),
            "ticket body must not be returned on 500"
        );
        let code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(code, "INTERNAL_ERROR");
    }

    #[tokio::test]
    async fn realtime_ticket_with_store_succeeds() {
        // Sanity: wiring the fake store yields 200 with opaque ticket
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
            store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>;
        let dyn_store = DynIdentityAdministrationStore(admin_port);
        let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
        let admin = Arc::new(IdentityAdministrationService::new(
            Arc::new(dyn_store),
            Arc::new(dyn_clock),
            (*passwords).clone(),
        ));
        let (admin_user, admin_pw) = admin
            .bootstrap_administrator(
                LoginId::new("admin-tc2").expect("login"),
                "Admin".to_owned(),
                orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                    .expect("req"),
            )
            .await
            .expect("bootstrap");
        let _ = admin_user;
        let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
            Arc::new(FakeRealtimeTicketStore::default());

        let login_tx: Arc<dyn orbisync_application::LoginTransactionStore> =
            store.clone() as Arc<dyn orbisync_application::LoginTransactionStore>;
        let login_service = Arc::new(orbisync_identity::LoginService::new(
            repo.clone(),
            login_tx,
            (*passwords).clone(),
            tokens.clone(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
            900,
            2_592_000,
        ));
        let state = HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        )
        .with_token_service(Arc::clone(&tokens))
        .with_identity_repository(repo.clone())
        .with_password_service(Arc::clone(&passwords))
        .with_identity_admin_service(Arc::clone(&admin))
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
        .with_realtime_ticket_store(Arc::clone(&ticket_store))
        .with_login_service(login_service);

        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admin-tc2", "password": admin_pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();

        let app = router(state.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let ticket = json
            .get("realtime_ticket")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            !ticket.is_empty(),
            "wired store must return realtime_ticket"
        );
        let expires = json.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(0);
        assert_eq!(expires, 60);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod realtime_ticket_rate_limit_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::IdentityRepository;
    use orbisync_domain::{LoginId, Timestamp};
    use orbisync_identity::{
        DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
        PasswordService, token::AccessTokenService,
    };
    use orbisync_testkit::{FakeIdentityStore, FakeRealtimeTicketStore, FixedClock};
    use std::sync::Arc;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::ticket_rate_limit::RealtimeTicketRateLimiter;
    use crate::{HttpState, router};

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn token_service() -> Arc<AccessTokenService> {
        Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test-key-1",
            )
            .expect("token service"),
        )
    }

    struct NopRefreshCreationStore;
    #[async_trait::async_trait]
    impl orbisync_application::RefreshTokenCreationStore for NopRefreshCreationStore {
        async fn create(
            &self,
            _cmd: orbisync_application::CreateRefreshTokenCommand,
        ) -> Result<(), orbisync_application::IdentityPortError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn ticket_issuance_is_rate_limited_per_user() {
        // C5: per-user/session limit must return 429 and not insert a row.
        // Use a tiny limiter (3 per 60s) so we can hit the limit quickly.
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
            store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>;
        let dyn_store = DynIdentityAdministrationStore(admin_port);
        let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
        let admin = Arc::new(IdentityAdministrationService::new(
            Arc::new(dyn_store),
            Arc::new(dyn_clock),
            (*passwords).clone(),
        ));
        let (admin_user, admin_pw) = admin
            .bootstrap_administrator(
                LoginId::new("rate-limit-user").expect("login"),
                "Admin".to_owned(),
                orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                    .expect("req"),
            )
            .await
            .expect("bootstrap");
        let _ = admin_user;

        // Fake ticket store that counts rows
        #[derive(Debug, Default)]
        struct CountingStore {
            inner: FakeRealtimeTicketStore,
            count: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl orbisync_application::RealtimeTicketStore for CountingStore {
            async fn create(
                &self,
                cmd: orbisync_application::CreateRealtimeTicketCommand,
            ) -> Result<(), orbisync_application::IdentityPortError> {
                self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.create(cmd).await
            }
            async fn consume(
                &self,
                digest: [u8; 32],
                now: Timestamp,
            ) -> Result<
                orbisync_application::RealtimeTicketConsumption,
                orbisync_application::IdentityPortError,
            > {
                self.inner.consume(digest, now).await
            }
        }
        let counting = Arc::new(CountingStore::default());
        let limiter = Arc::new(RealtimeTicketRateLimiter::new(3, 60));
        let login_tx: Arc<dyn orbisync_application::LoginTransactionStore> =
            store.clone() as Arc<dyn orbisync_application::LoginTransactionStore>;
        let login_service = Arc::new(orbisync_identity::LoginService::new(
            repo.clone(),
            login_tx,
            (*passwords).clone(),
            tokens.clone(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
            900,
            2_592_000,
        ));

        let state = HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        )
        .with_token_service(Arc::clone(&tokens))
        .with_identity_repository(repo.clone())
        .with_password_service(Arc::clone(&passwords))
        .with_identity_admin_service(Arc::clone(&admin))
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
        .with_realtime_ticket_store(
            counting.clone() as Arc<dyn orbisync_application::RealtimeTicketStore>
        )
        .with_realtime_ticket_rate_limiter(Arc::clone(&limiter))
        .with_login_service(login_service);

        // Login to get token
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "rate-limit-user", "password": admin_pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();

        // Issue 3 tickets – should succeed
        for i in 0..3 {
            let app = router(state.clone());
            let req = Request::builder()
                .uri("/v1/realtime/tickets")
                .method("POST")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "ticket {i} should be 200");
        }
        let count_after_3 = counting.count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(count_after_3, 3);

        // 4th ticket – must be 429 and must NOT increase row count
        let app = router(state.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "4th ticket should be 429 rate limited"
        );
        let count_after_4 = counting.count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            count_after_4, 3,
            "rate limited request must not increase row count"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(code, "RATE_LIMITED");
    }

    #[tokio::test]
    async fn public_constructor_uses_safe_default_without_explicit_wiring() {
        // Axis 4: the public constructor must be safe-by-default. If the library
        // default were dangerously permissive (e.g. 1000 per minute), a test that
        // only exercises `main.rs` would be overwritten by the safe value and the
        // defect would be invisible. This test uses the public constructor
        // directly, without calling `with_realtime_ticket_rate_limiter`, and
        // verifies that the built-in default (10 per 60s) still rate limits.
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
            store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>;
        let dyn_store = DynIdentityAdministrationStore(admin_port);
        let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
        let admin = Arc::new(IdentityAdministrationService::new(
            Arc::new(dyn_store),
            Arc::new(dyn_clock),
            (*passwords).clone(),
        ));
        let (admin_user, admin_pw) = admin
            .bootstrap_administrator(
                LoginId::new("default-limiter-user").expect("login"),
                "Admin".to_owned(),
                orbisync_application::RequestId::new(format!("req_{}", Uuid::now_v7()))
                    .expect("req"),
            )
            .await
            .expect("bootstrap");
        let _ = admin_user;
        let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
            Arc::new(FakeRealtimeTicketStore::default());

        // Use public constructor WITHOUT explicit limiter – must still enforce default (10 per 60s).
        let login_tx: Arc<dyn orbisync_application::LoginTransactionStore> =
            store.clone() as Arc<dyn orbisync_application::LoginTransactionStore>;
        let login_service = Arc::new(orbisync_identity::LoginService::new(
            repo.clone(),
            login_tx,
            (*passwords).clone(),
            tokens.clone(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
            900,
            2_592_000,
        ));
        let state_default = HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        )
        .with_token_service(Arc::clone(&tokens))
        .with_identity_repository(repo.clone())
        .with_password_service(Arc::clone(&passwords))
        .with_identity_admin_service(Arc::clone(&admin))
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
        .with_realtime_ticket_store(Arc::clone(&ticket_store))
        .with_login_service(login_service);

        let app = router(state_default.clone());
        let body = serde_json::json!({ "login_id": "default-limiter-user", "password": admin_pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();

        // Default is 10 per 60s – first 10 OK, 11th must be 429.
        for i in 0..10 {
            let app = router(state_default.clone());
            let req = Request::builder()
                .uri("/v1/realtime/tickets")
                .method("POST")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "default limiter ticket {i} should be 200"
            );
        }
        let app = router(state_default.clone());
        let req = Request::builder()
            .uri("/v1/realtime/tickets")
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "11th ticket with default limit 10 should be 429"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod login_p2c4_tests {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::{IdentityRepository, LoginTransactionStore, SecretString};
    use orbisync_domain::{Clock as _, LoginId, Timestamp};
    use orbisync_identity::{
        DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, LoginService,
        PasswordPolicy, PasswordService, token::AccessTokenService,
    };
    use orbisync_testkit::{FakeIdentityStore, FixedClock};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    use crate::{HttpState, router};

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn token_service() -> Arc<AccessTokenService> {
        Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test-key-1",
            )
            .expect("token service"),
        )
    }

    fn make_state(
        store: Arc<FakeIdentityStore>,
        clock: Arc<FixedClock>,
        tokens: Arc<AccessTokenService>,
        passwords: Arc<PasswordService>,
        trusted: Vec<String>,
    ) -> HttpState {
        let repo: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        let tx: Arc<dyn LoginTransactionStore> = store.clone() as Arc<dyn LoginTransactionStore>;
        let login_service = Arc::new(LoginService::new(
            repo.clone(),
            tx,
            (*passwords).clone(),
            tokens.clone(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
            900,
            2_592_000,
        ));
        // need admin service for bootstrap but not needed for login state; we still keep it
        let admin_port: Arc<dyn orbisync_application::IdentityAdministrationStore> =
            store.clone() as Arc<dyn orbisync_application::IdentityAdministrationStore>;
        let dyn_store = DynIdentityAdministrationStore(admin_port);
        let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
        let admin = Arc::new(IdentityAdministrationService::new(
            Arc::new(dyn_store),
            Arc::new(dyn_clock),
            (*passwords).clone(),
        ));
        HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        )
        .with_token_service(tokens)
        .with_identity_repository(repo)
        .with_password_service(passwords)
        .with_identity_admin_service(admin)
        .with_login_service(login_service)
        .with_trusted_proxies(trusted)
        .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec())
    }

    async fn bootstrap(
        store: &FakeIdentityStore,
        passwords: &PasswordService,
        clock: &FixedClock,
        login_id: &str,
    ) -> (orbisync_domain::User, SecretString) {
        let now = clock.now();
        let user_id = orbisync_domain::UserId::generate();
        let login = LoginId::new(login_id).expect("login");
        let user = orbisync_domain::User::new(user_id, login.clone(), "Test", now).expect("user");
        let hash = passwords
            .hash(SecretString::new("Cedar!Lake7-Comet"))
            .await
            .expect("hash");
        let cred = orbisync_domain::Credential::new(user_id, hash, now);
        let account = orbisync_application::LoginAccount {
            user: user.clone(),
            credential: cred,
        };
        store.insert_account(account);
        (user, SecretString::new("Cedar!Lake7-Comet"))
    }

    fn login_request(
        ip: [u8; 4],
        login_id: &str,
        password: &str,
        forwarded_for: &str,
    ) -> Request<Body> {
        let body = serde_json::json!({ "login_id": login_id, "password": password });
        let mut request = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-forwarded-for", forwarded_for)
            .body(Body::from(body.to_string()))
            .expect("valid login request");
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((ip, 443))));
        request
    }

    #[tokio::test]
    async fn disabled_local_rejects_credentials_without_issuing_or_revoking_sessions() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).unwrap(),
        ));
        let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).unwrap());
        let (_, password) = bootstrap(&store, &passwords, &clock, "local-toggle").await;
        let state = make_state(store.clone(), clock, token_service(), passwords, vec![]);
        let response = router(state.clone())
            .oneshot(login_request(
                [127, 0, 0, 1],
                "local-toggle",
                password.expose_secret(),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sessions = store.sessions_snapshot();
        let refreshes = store.login_refresh_tokens();
        let audits = store.login_audits();
        let disabled = state.with_enabled_auth_methods(vec![orbisync_domain::AuthMethod::External]);
        let response = router(disabled.clone())
            .oneshot(login_request(
                [127, 0, 0, 1],
                "local-toggle",
                password.expose_secret(),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "AUTH_METHOD_DISABLED");
        assert_eq!(store.sessions_snapshot(), sessions);
        assert_eq!(store.login_refresh_tokens(), refreshes);
        assert_eq!(store.login_audits(), audits);
        for body in ["{", "{}", r#"{"login_id":42,"password":true}"#] {
            let request = Request::builder()
                .uri("/v1/auth/login")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let response = router(disabled.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], "INVALID_REQUEST");
        }
    }

    #[tokio::test]
    async fn login_success_commits_session_refresh_and_audit_atomically() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        // Bootstrap via manual insert for determinism
        let (user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-success").await;
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec![],
        );
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "p2c4-success", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::from(body.to_string()))
            .unwrap();
        // Without trusted proxies, source_ip will be None; with trusted, it will be header value.
        // Use trusted to verify source_ip is stored.
        // For this test, we use no trusted, so audit source_ip will be None, but commit still happens.
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "login must succeed");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("access_token").is_some());
        assert!(json.get("refresh_token").is_some());
        // Verify atomic commit: session, refresh, audit all present
        let sessions = store.sessions_snapshot();
        assert_eq!(sessions.len(), 1, "exactly one session must be persisted");
        let refreshes = store.login_refresh_tokens();
        assert_eq!(refreshes.len(), 1, "exactly one refresh must be persisted");
        let audits = store.login_audits();
        assert_eq!(audits.len(), 1, "exactly one audit must be persisted");
        let audit = &audits[0];
        assert_eq!(audit.action, "auth.login");
        assert!(audit.succeeded, "success audit");
        assert_eq!(audit.actor_id, Some(user.id()));
        // No secrets in audit
        let audit_str = format!("{audit:?}");
        assert!(
            !audit_str.contains("Cedar"),
            "audit must not contain password"
        );
        assert!(
            !audit_str.contains("p2c4-success"),
            "audit must not contain raw login_id"
        );
        // Also verify session belongs to user
        let sess = sessions.values().next().unwrap();
        assert_eq!(sess.user_id(), user.id());
    }

    #[tokio::test]
    async fn login_success_audit_contains_trusted_source_ip() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-ip").await;
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec!["10.0.0.1".to_owned()],
        );
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "p2c4-ip", "password": pw.expose_secret() });
        let mut req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-forwarded-for", "198.51.100.9")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 443))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let audits = store.login_audits();
        assert_eq!(audits.len(), 1);
        let audit = &audits[0];
        // With trusted proxy configured, the header IP is stored as source_ip
        assert_eq!(audit.source_ip.as_deref(), Some("198.51.100.9"));
        assert_eq!(audit.actor_id, Some(user.id()));
    }

    #[tokio::test]
    async fn login_success_partial_failure_leaves_no_orphan() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (_user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-partial").await;

        // Fail session insert
        store.fail_next_login_session();
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec![],
        );
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "p2c4-partial", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "session failure must be 500"
        );
        assert_eq!(store.sessions_snapshot().len(), 0, "no session must remain");
        assert_eq!(
            store.login_refresh_tokens().len(),
            0,
            "no refresh must remain"
        );
        assert_eq!(
            store.login_audits().len(),
            0,
            "no audit must remain on failure"
        );

        // Fail refresh insert
        store.fail_next_login_refresh();
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "p2c4-partial", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(store.sessions_snapshot().len(), 0);
        assert_eq!(store.login_refresh_tokens().len(), 0);
        assert_eq!(store.login_audits().len(), 0);

        // Fail audit insert – must also be atomic and fail-closed
        store.fail_next_login_audit();
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "p2c4-partial", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "audit failure must be 500 fail-closed"
        );
        assert_eq!(
            store.sessions_snapshot().len(),
            0,
            "audit failure must roll back session"
        );
        assert_eq!(
            store.login_refresh_tokens().len(),
            0,
            "audit failure must roll back refresh"
        );
        assert_eq!(store.login_audits().len(), 0);
    }

    #[tokio::test]
    async fn login_failures_are_audited_and_enumeration_resistant() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (_user, _pw) = bootstrap(&store, &passwords, &clock, "p2c4-enum").await;
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec![],
        );

        // Wrong password
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "p2c4-enum", "password": "Wrong!Lake7-Comet" });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Unknown account
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "unknown-xyz", "password": "Cedar!Lake7-Comet" });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Locked account – trigger lockout by 5 failures
        for _ in 0..4 {
            let app = router(state.clone());
            let body =
                serde_json::json!({ "login_id": "p2c4-enum", "password": "Wrong!Lake7-Comet" });
            let req = Request::builder()
                .uri("/v1/auth/login")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let _ = app.oneshot(req).await.unwrap();
        }
        // Now even correct password should be locked and return 401 with audit
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "p2c4-enum", "password": "Cedar!Lake7-Comet" });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let audits = store.login_audits();
        // At least 1 (wrong) +1 (unknown)+4 (extra wrong)+1 (locked) =7
        assert!(
            audits.len() >= 7,
            "each failure must be audited, got {}",
            audits.len()
        );
        for audit in &audits {
            assert_eq!(audit.action, "auth.login");
            assert!(!audit.succeeded, "failure audits must be failure");
            // Enumeration resistance: no raw login_id in audit, actor is None for all failures
            assert_eq!(
                audit.actor_id, None,
                "failure actor must be None for enumeration resistance"
            );
            let s = format!("{audit:?}");
            assert!(!s.contains("p2c4-enum"));
            assert!(!s.contains("unknown-xyz"));
            assert!(!s.contains("Cedar"));
            assert!(!s.contains("Wrong"));
        }
        // Success vs failure indistinguishable externally: both return 401 with same body shape
        // Verified above by status code equality; body shape is same error envelope.
    }

    #[tokio::test]
    async fn login_audit_failure_is_fail_closed() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (_user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-failclosed").await;
        store.fail_next_login_audit();
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec![],
        );
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "p2c4-failclosed", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "audit write failure must be 500"
        );
        assert_eq!(
            store.sessions_snapshot().len(),
            0,
            "fail-closed: no session"
        );
        assert_eq!(store.login_audits().len(), 0, "fail-closed: no audit");
    }

    #[tokio::test]
    async fn login_response_and_logs_contain_no_secrets() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (_user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-secret").await;
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec![],
        );
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "p2c4-secret", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let body_str = json.to_string();
        // Response contains tokens but not password or login_id; ensure password not echoed
        assert!(!body_str.contains("Cedar"));
        // Audit must not contain secrets
        let audits = store.login_audits();
        let audit_str = format!("{audits:?}");
        assert!(!audit_str.contains("Cedar"));
        // Debug of state must be redacted
        let state_str = format!("{state:?}");
        assert!(!state_str.contains("Cedar"));
        assert!(!state_str.contains("p2c4-secret"));
    }

    #[tokio::test]
    async fn untrusted_header_is_not_used_as_source_ip() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (_user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-untrusted").await;
        // No trusted proxies – header must be ignored
        let state = make_state(
            store.clone(),
            clock.clone(),
            tokens.clone(),
            passwords.clone(),
            vec![],
        );
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "p2c4-untrusted", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-forwarded-for", "198.51.100.99")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let audits = store.login_audits();
        assert_eq!(
            audits[0].source_ip, None,
            "without trusted proxy, header must not be used"
        );
    }

    #[tokio::test]
    async fn login_ip_rate_limit_counts_successes_and_ignores_untrusted_xff() {
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (_user_a, pw_a) = bootstrap(&store, &passwords, &clock, "ip-limit-a").await;
        let (_user_b, pw_b) = bootstrap(&store, &passwords, &clock, "ip-limit-b").await;
        let limiter = Arc::new(crate::login_rate_limit::LoginRateLimiter::new(2, 30, 10));
        let state = make_state(store, clock.clone(), tokens, passwords, vec![])
            .with_login_rate_limiter(limiter);

        let response = router(state.clone())
            .oneshot(login_request(
                [192, 0, 2, 1],
                "ip-limit-a",
                pw_a.expose_secret(),
                "198.51.100.1",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "success is one attempt");

        let response = router(state.clone())
            .oneshot(login_request(
                [192, 0, 2, 1],
                "ip-limit-a",
                "wrong-password",
                "198.51.100.2",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = router(state.clone())
            .oneshot(login_request(
                [192, 0, 2, 1],
                "ip-limit-a",
                pw_a.expose_secret(),
                "198.51.100.3",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "30");

        let response = router(state.clone())
            .oneshot(login_request(
                [192, 0, 2, 2],
                "ip-limit-b",
                pw_b.expose_secret(),
                "203.0.113.99",
            ))
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "another IP is independent"
        );

        clock.advance_millis(31_000);
        let response = router(state)
            .oneshot(login_request(
                [192, 0, 2, 1],
                "ip-limit-a",
                pw_a.expose_secret(),
                "203.0.113.100",
            ))
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "source may retry after the block and window expire"
        );
    }

    #[tokio::test]
    async fn public_constructor_without_login_service_is_500() {
        // Axis 4: public constructor without composition root wiring must be fail-closed.
        let store = Arc::new(FakeIdentityStore::new());
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let tokens = token_service();
        let passwords =
            Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw"));
        let (user, pw) = bootstrap(&store, &passwords, &clock, "p2c4-axis4").await;
        let state = HttpState::new(
            vec![],
            "orbisync",
            "0.1.0",
            1,
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        )
        .with_token_service(tokens)
        .with_identity_repository(store.clone() as Arc<dyn IdentityRepository>)
        .with_password_service(passwords)
        .with_realtime_ticket_hmac_key(b"test-realtime-ticket-hmac-key-32b!!".to_vec());
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "p2c4-axis4", "password": pw.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "without login_service must be 500 fail-closed (axis 4)"
        );
        let _ = user;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod auth_method_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_domain::{AuthMethod, Permission, Role, RoleId, Timestamp};
    use orbisync_identity::{
        EphemeralMethodPolicy, EphemeralSubjectService, SessionIssuer, token::AccessTokenService,
    };
    use orbisync_testkit::{FakeIdentityStore, FixedClock};
    use std::sync::Arc;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::{HttpState, router};

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn token_service() -> Arc<AccessTokenService> {
        Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test-key-1",
            )
            .expect("token service"),
        )
    }

    fn world_id() -> Uuid {
        Uuid::parse_str("0192d43d-a18a-7fed-8123-0123456789ab").expect("valid uuid")
    }

    /// Builds state with guest and name-only enabled.
    fn state_with_methods(store: Arc<FakeIdentityStore>) -> (HttpState, RoleId) {
        state_with_ttl(store, 3_600)
    }

    fn state_with_ttl(store: Arc<FakeIdentityStore>, ttl: u64) -> (HttpState, RoleId) {
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        let role_id = RoleId::generate();
        let role = Role::new(
            role_id,
            "Visitor",
            None,
            [
                Permission::new("entity.spawn").expect("valid"),
                Permission::new("entity.update.own").expect("valid"),
            ],
        )
        .expect("role");
        store.insert_role_definition(role);

        let issuer = Arc::new(SessionIssuer::new(
            store.clone() as Arc<dyn orbisync_application::LoginTransactionStore>,
            token_service(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
            b"test-refresh-hmac-key".to_vec(),
            900,
            2_592_000,
        ));
        let policy = |method: AuthMethod| EphemeralMethodPolicy {
            method,
            grant_roles: vec![role_id],
            session_ttl_seconds: ttl,
            allowed_worlds: vec![world_id()],
            display_name_prefix: "Guest".to_owned(),
        };
        let service = Arc::new(EphemeralSubjectService::new(
            issuer,
            Some(policy(AuthMethod::Guest)),
            Some(policy(AuthMethod::NameOnly)),
        ));

        let state = HttpState::new(
            Vec::new(),
            "orbisync",
            "0.1.0",
            1,
            clock as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-refresh-hmac-key".to_vec(),
        )
        .with_ephemeral_subject_service(service)
        .with_enabled_auth_methods(vec![
            AuthMethod::Local,
            AuthMethod::Guest,
            AuthMethod::NameOnly,
        ]);
        (state, role_id)
    }

    /// Builds state with every new method left disabled (the default).
    fn state_without_methods() -> HttpState {
        let clock = Arc::new(FixedClock::new(
            Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        ));
        HttpState::new(
            Vec::new(),
            "orbisync",
            "0.1.0",
            1,
            clock as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-refresh-hmac-key".to_vec(),
        )
    }

    async fn post(state: &HttpState, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .uri(path)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .expect("request");
        let response = router(state.clone())
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn short_subject_ttl_caps_initial_signed_jwt_for_both_methods() {
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _) = state_with_ttl(store, 30);
        for (path, body) in [
            ("/v1/auth/guest", "{}"),
            ("/v1/auth/name", r#"{"display_name":"Ada"}"#),
        ] {
            let (status, json) = post(&state, path, body).await;
            assert_eq!(status, StatusCode::OK);
            let token =
                orbisync_application::SecretString::new(json["access_token"].as_str().unwrap());
            let claims = token_service()
                .validate(&token, state.clock().now())
                .unwrap();
            assert_eq!(claims.expires_at_unix_seconds(), 1_700_000_030);
            assert_eq!(json["expires_in"], 30);
        }
    }

    struct RotationWithDeadline(Option<Timestamp>);

    #[async_trait::async_trait]
    impl orbisync_application::RefreshTokenRotationStore for RotationWithDeadline {
        async fn rotate(
            &self,
            _: orbisync_application::RotateRefreshTokenCommand,
        ) -> Result<
            orbisync_application::RefreshRotationResult,
            orbisync_application::IdentityPortError,
        > {
            Ok(orbisync_application::RefreshRotationResult::Rotated {
                user_id: orbisync_domain::UserId::generate(),
                session_id: orbisync_domain::AuthSessionId::generate(),
                absolute_deadline: self.0,
            })
        }
    }

    #[tokio::test]
    async fn refresh_signed_jwt_uses_port_deadline_and_preserves_permanent_ttl() {
        for (remaining, expected) in [(Some(20_500), 20), (Some(3_600_000), 900), (None, 900)] {
            let state = state_without_methods();
            let now = state.clock().now();
            let deadline = remaining.map(|ms| now.checked_add_millis(ms).unwrap());
            let state = state
                .with_token_service(token_service())
                .with_refresh_rotation_store(Arc::new(RotationWithDeadline(deadline)));
            let (status, json) = post(
                &state,
                "/v1/auth/refresh",
                r#"{"refresh_token":"test-refresh"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{json}");
            let token =
                orbisync_application::SecretString::new(json["access_token"].as_str().unwrap());
            let claims = token_service().validate(&token, now).unwrap();
            assert_eq!(claims.expires_at_unix_seconds(), 1_700_000_000 + expected);
            assert_eq!(json["expires_in"], expected);
        }
    }

    #[tokio::test]
    async fn refresh_issuer_refuses_deadline_even_if_port_returns_rotated() {
        for offset in [0, -1_000] {
            let state = state_without_methods();
            let deadline = state.clock().now().checked_add_millis(offset).unwrap();
            let state = state
                .with_token_service(token_service())
                .with_refresh_rotation_store(Arc::new(RotationWithDeadline(Some(deadline))));
            let (status, json) = post(
                &state,
                "/v1/auth/refresh",
                r#"{"refresh_token":"test-refresh"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
            assert!(json.get("access_token").is_none());
        }
    }

    #[tokio::test]
    async fn disabled_methods_are_refused_and_return_no_token() {
        // The default deployment has only local enabled, so these three must
        // refuse without minting anything.
        let state = state_without_methods();
        for (path, body) in [
            ("/v1/auth/guest", "{}"),
            ("/v1/auth/name", r#"{"display_name":"Ada"}"#),
            ("/v1/auth/external", r#"{"token":"anything"}"#),
        ] {
            let (status, json) = post(&state, path, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} must be refused");
            assert_eq!(json["error"]["code"], "AUTH_METHOD_DISABLED");
            let rendered = json.to_string();
            assert!(
                !rendered.contains("access_token") && !rendered.contains("refresh_token"),
                "{path} must not return a token while disabled"
            );
        }
    }

    #[tokio::test]
    async fn enabled_guest_and_name_only_issue_a_session() {
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _role) = state_with_methods(store);
        for (path, body) in [
            ("/v1/auth/guest", "{}"),
            ("/v1/auth/name", r#"{"display_name":"Ada"}"#),
        ] {
            let (status, json) = post(&state, path, body).await;
            assert_eq!(status, StatusCode::OK, "{path} returned {json}");
            assert_eq!(json["token_type"], "Bearer");
            assert!(json["access_token"].as_str().is_some_and(|t| !t.is_empty()));
            assert!(
                json["refresh_token"]
                    .as_str()
                    .is_some_and(|t| !t.is_empty())
            );
        }
    }

    #[tokio::test]
    async fn two_guests_receive_distinct_subjects() {
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _role) = state_with_methods(store.clone());
        let (_, first) = post(&state, "/v1/auth/guest", "{}").await;
        let (_, second) = post(&state, "/v1/auth/guest", "{}").await;
        assert_ne!(first["access_token"], second["access_token"]);
        assert_ne!(first["refresh_token"], second["refresh_token"]);
        assert_eq!(store.issued_subject_count(), 2);
        assert_eq!(
            store.distinct_issued_user_count(),
            2,
            "a fresh guest must be a different subject"
        );
    }

    #[tokio::test]
    async fn the_same_display_name_twice_yields_two_subjects() {
        // The name is a label. Two visitors picking "Ada" must not collide on
        // the users.login_id unique constraint, nor share an identity.
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _role) = state_with_methods(store.clone());
        let body = r#"{"display_name":"Ada"}"#;
        let (first_status, _) = post(&state, "/v1/auth/name", body).await;
        let (second_status, _) = post(&state, "/v1/auth/name", body).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(store.distinct_issued_user_count(), 2);
    }

    #[tokio::test]
    async fn invalid_display_names_are_refused() {
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _role) = state_with_methods(store);
        let overlong = "x".repeat(129);
        for body in [
            r#"{"display_name":""}"#.to_owned(),
            r#"{"display_name":"   "}"#.to_owned(),
            format!(r#"{{"display_name":"{overlong}"}}"#),
            r#"{"display_name":"bad name"}"#.to_owned(),
        ] {
            let (status, _) = post(&state, "/v1/auth/name", &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "body {body} must fail");
        }
    }

    #[tokio::test]
    async fn a_request_claiming_roles_or_an_identity_is_refused() {
        // Not "ignored": the body is rejected outright, so there is no way to
        // read the response as though the claim had been honoured.
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _role) = state_with_methods(store.clone());
        for (path, body) in [
            ("/v1/auth/guest", r#"{"role":"Administrator"}"#),
            (
                "/v1/auth/guest",
                r#"{"user_id":"0192d43d-a18a-7fed-8123-0123456789ab"}"#,
            ),
            (
                "/v1/auth/guest",
                r#"{"permissions":["admin.users.create"]}"#,
            ),
            (
                "/v1/auth/name",
                r#"{"display_name":"Ada","role":"Administrator"}"#,
            ),
            (
                "/v1/auth/name",
                r#"{"display_name":"Ada","owner":"someone-else"}"#,
            ),
        ] {
            let (status, _) = post(&state, path, body).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{path} must reject a forged field: {body}"
            );
        }
        assert_eq!(
            store.issued_subject_count(),
            0,
            "a rejected request must not create a subject"
        );
    }

    #[tokio::test]
    async fn granted_roles_come_from_configuration_only() {
        let store = Arc::new(FakeIdentityStore::new());
        let (state, role_id) = state_with_methods(store.clone());
        let (status, _) = post(&state, "/v1/auth/guest", "{}").await;
        assert_eq!(status, StatusCode::OK);
        let granted = store.last_issued_roles().expect("a subject was issued");
        assert_eq!(
            granted,
            vec![role_id],
            "only the configured role is granted"
        );
    }

    #[tokio::test]
    async fn mode_discovery_lists_methods_without_exposing_settings() {
        let store = Arc::new(FakeIdentityStore::new());
        let (state, _role) = state_with_methods(store);
        let request = Request::builder()
            .uri("/v1/auth/methods")
            .method("GET")
            .body(Body::empty())
            .expect("request");
        let response = router(state).oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let methods: Vec<&str> = json["methods"]
            .as_array()
            .expect("array")
            .iter()
            .map(|value| value.as_str().expect("string"))
            .collect();
        assert!(methods.contains(&"local"));
        assert!(methods.contains(&"guest"));
        assert!(methods.contains(&"name_only"));
        assert!(!methods.contains(&"external"));
        // Only identifiers: no issuer URL, key path or other setting.
        let rendered = json.to_string();
        for leaked in ["jwks", "issuer", "audience", "role", "world", "ttl"] {
            assert!(
                !rendered.contains(leaked),
                "discovery must not expose `{leaked}`"
            );
        }
    }

    #[tokio::test]
    async fn discovery_on_a_default_deployment_reports_local_only() {
        let state = state_without_methods();
        let request = Request::builder()
            .uri("/v1/auth/methods")
            .method("GET")
            .body(Body::empty())
            .expect("request");
        let response = router(state).oneshot(request).await.expect("response");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["methods"], serde_json::json!(["local"]));
    }
}
