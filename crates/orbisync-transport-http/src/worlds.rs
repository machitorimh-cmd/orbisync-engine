//! World and instance HTTP handlers.
//!
//! Minimal Axum handlers for `POST /v1/worlds` and `POST /v1/instances` as required
//! by the M2 Realtime Minimum milestone. Handlers call `WorldDirectoryUseCase`,
//! validate the stub Bearer auth, and map `ApplicationErrorKind` onto the public
//! `ErrorCode` registry without leaking internal details.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use orbisync_application::{
    ApplicationError, ApplicationErrorKind, ArchiveWorldCommand, CreateInstanceCommand,
    CreateWorldCommand, InstanceView, Page, PageRequest, QueryCursor, StartInstanceCommand,
    StopInstanceCommand, UpdateWorldCommand, WorldAuthorizer, WorldDirectoryStore,
    WorldDirectoryUseCase, WorldView,
};
use orbisync_domain::{
    InstanceId, Revision, Timestamp, UserId, WorldId,
    transform::{Quaternion, Transform, Vec3},
};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use crate::{ErrorCode, RequestContext, parse_uuid_v7};

/// Object-safe world directory port used by the HTTP adapter.
#[async_trait::async_trait]
pub trait WorldDirectory: Send + Sync + 'static {
    /// Creates a world.
    async fn create_world(
        &self,
        command: CreateWorldCommand,
    ) -> Result<WorldView, ApplicationError>;

    /// Creates a world while preserving the request source IP for audit.
    async fn create_world_with_source_ip(
        &self,
        command: CreateWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        let _ = source_ip;
        self.create_world(command).await
    }

    /// Fetches a single world after checking `admin.worlds.read`.
    async fn get_world(
        &self,
        actor_id: UserId,
        world_id: WorldId,
    ) -> Result<WorldView, ApplicationError>;

    /// Lists worlds after checking `admin.worlds.read`.
    async fn list_worlds(
        &self,
        actor_id: UserId,
        page: PageRequest,
    ) -> Result<Page<WorldView>, ApplicationError>;

    /// Updates a world definition.
    async fn update_world(
        &self,
        command: UpdateWorldCommand,
    ) -> Result<WorldView, ApplicationError> {
        self.update_world_with_source_ip(command, None).await
    }

    /// Updates a world while preserving the request source IP for audit.
    async fn update_world_with_source_ip(
        &self,
        command: UpdateWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError>;

    /// Archives a world.
    async fn archive_world(
        &self,
        command: ArchiveWorldCommand,
    ) -> Result<WorldView, ApplicationError> {
        self.archive_world_with_source_ip(command, None).await
    }

    /// Archives a world while preserving the request source IP for audit.
    async fn archive_world_with_source_ip(
        &self,
        command: ArchiveWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError>;

    /// Creates an instance.
    async fn create_instance(
        &self,
        command: CreateInstanceCommand,
    ) -> Result<InstanceView, ApplicationError>;

    /// Creates an instance while preserving the request source IP for audit.
    async fn create_instance_with_source_ip(
        &self,
        command: CreateInstanceCommand,
        source_ip: Option<String>,
    ) -> Result<InstanceView, ApplicationError> {
        let _ = source_ip;
        self.create_instance(command).await
    }

    /// Starts an instance.
    async fn start_instance(
        &self,
        command: StartInstanceCommand,
    ) -> Result<InstanceView, ApplicationError>;

    /// Requests an instance stop.
    async fn stop_instance(
        &self,
        command: StopInstanceCommand,
    ) -> Result<InstanceView, ApplicationError>;

    /// Fetches a single instance.
    async fn get_instance(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
    ) -> Result<InstanceView, ApplicationError>;

    /// Lists instances using stable keyset pagination.
    async fn list_instances(
        &self,
        actor_id: UserId,
        page: PageRequest,
    ) -> Result<Page<InstanceView>, ApplicationError>;
}

#[async_trait::async_trait]
impl<S, A> WorldDirectory for WorldDirectoryUseCase<S, A>
where
    S: WorldDirectoryStore,
    A: WorldAuthorizer,
{
    async fn create_world(
        &self,
        command: CreateWorldCommand,
    ) -> Result<WorldView, ApplicationError> {
        WorldDirectoryUseCase::create_world(self, command).await
    }

    async fn create_world_with_source_ip(
        &self,
        command: CreateWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        WorldDirectoryUseCase::create_world_with_source_ip(self, command, source_ip).await
    }

    async fn get_world(
        &self,
        actor_id: UserId,
        world_id: WorldId,
    ) -> Result<WorldView, ApplicationError> {
        WorldDirectoryUseCase::get_world(self, actor_id, world_id).await
    }

    async fn list_worlds(
        &self,
        actor_id: UserId,
        page: PageRequest,
    ) -> Result<Page<WorldView>, ApplicationError> {
        WorldDirectoryUseCase::list_worlds(self, actor_id, page).await
    }

    async fn update_world_with_source_ip(
        &self,
        command: UpdateWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        WorldDirectoryUseCase::update_world_with_source_ip(self, command, source_ip).await
    }

    async fn archive_world_with_source_ip(
        &self,
        command: ArchiveWorldCommand,
        source_ip: Option<String>,
    ) -> Result<WorldView, ApplicationError> {
        WorldDirectoryUseCase::archive_world_with_source_ip(self, command, source_ip).await
    }

    async fn create_instance(
        &self,
        command: CreateInstanceCommand,
    ) -> Result<InstanceView, ApplicationError> {
        WorldDirectoryUseCase::create_instance(self, command).await
    }

    async fn create_instance_with_source_ip(
        &self,
        command: CreateInstanceCommand,
        source_ip: Option<String>,
    ) -> Result<InstanceView, ApplicationError> {
        WorldDirectoryUseCase::create_instance_with_source_ip(self, command, source_ip).await
    }

    async fn start_instance(
        &self,
        command: StartInstanceCommand,
    ) -> Result<InstanceView, ApplicationError> {
        WorldDirectoryUseCase::start_instance(self, command).await
    }

    async fn stop_instance(
        &self,
        command: StopInstanceCommand,
    ) -> Result<InstanceView, ApplicationError> {
        WorldDirectoryUseCase::stop_instance(self, command).await
    }

    async fn get_instance(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
    ) -> Result<InstanceView, ApplicationError> {
        WorldDirectoryUseCase::get_instance(self, actor_id, instance_id).await
    }

    async fn list_instances(
        &self,
        actor_id: UserId,
        page: PageRequest,
    ) -> Result<Page<InstanceView>, ApplicationError> {
        WorldDirectoryUseCase::list_instances(self, actor_id, page).await
    }
}

/// Request body for `POST /v1/worlds`.
#[derive(Debug, Deserialize)]
pub struct CreateWorldRequest {
    /// World name 1..128 non-control characters.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Capacity 1..1000.
    pub capacity: u32,
    /// Optional spawn transform; defaults to identity.
    #[serde(default)]
    pub default_spawn: Option<TransformDto>,
}

/// Transform DTO matching `domain::Transform` wire shape.
#[derive(Debug, Deserialize)]
pub struct TransformDto {
    /// Position vector.
    pub position: Option<Vec3Dto>,
    /// Rotation quaternion.
    pub rotation: Option<QuaternionDto>,
    /// Scale vector.
    pub scale: Option<Vec3Dto>,
}

/// 3-component vector DTO.
#[derive(Debug, Deserialize)]
pub struct Vec3Dto {
    /// X.
    pub x: f64,
    /// Y.
    pub y: f64,
    /// Z.
    pub z: f64,
}

/// Quaternion DTO.
#[derive(Debug, Deserialize)]
pub struct QuaternionDto {
    /// X.
    pub x: f64,
    /// Y.
    pub y: f64,
    /// Z.
    pub z: f64,
    /// W.
    pub w: f64,
}

/// Request body for `POST /v1/instances`.
#[derive(Debug, Deserialize)]
pub struct CreateInstanceRequest {
    /// World to instantiate, canonical lowercase UUIDv7.
    pub world_id: String,
    /// Optional capacity override.
    pub capacity: Option<u32>,
}

#[derive(Debug, Serialize)]
struct WorldResponse {
    id: String,
    name: String,
    revision: u64,
    status: String,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Query params for `GET /v1/worlds`.
#[derive(Debug, Deserialize)]
pub struct WorldListQuery {
    /// Opaque pagination cursor.
    pub cursor: Option<String>,
    /// Page size (1..200, default 50).
    pub limit: Option<u16>,
}

/// Request body for `PATCH /v1/worlds/{world_id}` (`application/merge-patch+json`).
#[derive(Debug, Deserialize)]
pub struct UpdateWorldRequest {
    /// New name, when present.
    pub name: Option<String>,
    /// New description; key absent leaves it unchanged, `null` clears it.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub description: Option<Option<String>>,
    /// New capacity, when present.
    pub capacity: Option<u32>,
    /// New default spawn transform, when present.
    pub default_spawn: Option<TransformDto>,
}

#[derive(Debug, Serialize)]
struct WorldPageResponse {
    items: Vec<WorldResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

fn world_response(view: WorldView) -> WorldResponse {
    WorldResponse {
        id: view.id.to_string(),
        name: view.name,
        revision: view.revision.as_u64(),
        status: view.status,
    }
}

#[derive(Debug, Serialize)]
struct InstanceResponse {
    id: String,
    world_id: String,
    status: String,
    revision: u64,
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

pub(crate) fn error_response(code: ErrorCode, message: String, request_id: String) -> Response {
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

pub(crate) fn map_application_error(kind: ApplicationErrorKind) -> ErrorCode {
    match kind {
        ApplicationErrorKind::DomainRule => ErrorCode::InvalidRequest,
        ApplicationErrorKind::NotFound => ErrorCode::ResourceNotFound,
        ApplicationErrorKind::PortFailure => ErrorCode::InternalError,
        ApplicationErrorKind::Unavailable => ErrorCode::ServiceUnavailable,
        ApplicationErrorKind::Unauthenticated => ErrorCode::AuthenticationRequired,
        ApplicationErrorKind::NotAuthorized => ErrorCode::AccessDenied,
        ApplicationErrorKind::Conflict => ErrorCode::ResourceConflict,
        ApplicationErrorKind::RateLimited => ErrorCode::RateLimited,
        _ => ErrorCode::InternalError,
    }
}

pub(crate) async fn require_bearer(
    headers: &HeaderMap,
    state: &crate::HttpState,
    request_id: &str,
) -> Result<UserId, ErrorCode> {
    // `auth.allow_stub_bearer` is a development-only, fail-open path placed
    // before `crate::authenticate()` and must remain false in production.
    // When true, any non-empty bearer is accepted and a synthetic `UserId`
    // is minted without cryptographic verification or session liveness checks,
    // unlike the strict `crate::authenticate()` path used by other routes.
    // Configuration key `auth.allow_stub_bearer` must be false in production
    // because it bypasses token signature/time and repository revocation checks.
    if state.allow_stub_bearer() {
        let Some(_) = crate::extract_bearer(headers) else {
            return Err(ErrorCode::AuthenticationRequired);
        };
        return Ok(UserId::generate());
    }
    // Delegate to the centralized `crate::authenticate()` (CR-13 consolidation).
    // Convert its `Response`-based error back to `ErrorCode` without changing
    // semantics: `InternalError` stays `InternalError`, `AuthenticationRequired`
    // stays `AuthenticationRequired`.
    match crate::authenticate(state, headers, request_id).await {
        Ok((user_id, _, _)) => Ok(user_id),
        Err(resp) => {
            let status = resp.status();
            if status == StatusCode::INTERNAL_SERVER_ERROR {
                Err(ErrorCode::InternalError)
            } else {
                // `authenticate()` only returns 401 or 500; default to 401 to
                // preserve authentication failure semantics.
                Err(ErrorCode::AuthenticationRequired)
            }
        }
    }
}

pub(crate) fn build_timestamp() -> Timestamp {
    Timestamp::from_offset_date_time(OffsetDateTime::now_utc())
}

fn build_transform(dto: Option<TransformDto>) -> Result<Transform, String> {
    let Some(transform) = dto else {
        return Ok(Transform::identity());
    };
    let position = if let Some(pos) = transform.position {
        Vec3::new(pos.x, pos.y, pos.z).map_err(|e| e.to_string())?
    } else {
        Vec3::new(0.0, 0.0, 0.0).map_err(|e| e.to_string())?
    };
    let rotation = if let Some(rot) = transform.rotation {
        Quaternion::new(rot.x, rot.y, rot.z, rot.w).map_err(|e| e.to_string())?
    } else {
        Quaternion::new(0.0, 0.0, 0.0, 1.0).map_err(|e| e.to_string())?
    };
    let scale = if let Some(sc) = transform.scale {
        Vec3::new(sc.x, sc.y, sc.z).map_err(|e| e.to_string())?
    } else {
        Vec3::new(1.0, 1.0, 1.0).map_err(|e| e.to_string())?
    };
    Transform::new(position, rotation, scale).map_err(|e| e.to_string())
}

/// `POST /v1/worlds` handler.
pub async fn create_world(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<CreateWorldRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let transform = match build_transform(body.default_spawn) {
        Ok(value) => value,
        Err(detail) => {
            return error_response(ErrorCode::InvalidRequest, detail, request_id);
        }
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = CreateWorldCommand {
        actor_id,
        name: body.name,
        description: body.description,
        default_spawn: transform,
        capacity: body.capacity,
        now: build_timestamp(),
        request_id: app_request_id,
    };

    match directory
        .create_world_with_source_ip(command, context.source_ip().map(str::to_owned))
        .await
    {
        Ok(view) => {
            let response = WorldResponse {
                id: view.id.to_string(),
                name: view.name,
                revision: view.revision.as_u64(),
                status: view.status,
            };
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `POST /v1/instances` handler.
pub async fn create_instance(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<CreateInstanceRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    if state.is_shutting_down() {
        return error_response(
            ErrorCode::ServiceUnavailable,
            "server is shutting down".to_owned(),
            request_id,
        );
    }

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let world_id = match WorldId::parse(&body.world_id) {
        Ok(value) => value,
        Err(detail) => {
            return error_response(ErrorCode::InvalidRequest, detail.to_string(), request_id);
        }
    };

    // Validate instance id generation is not needed here; optional capacity
    // validation is delegated to domain, but we can early reject zero.
    if let Some(capacity) = body.capacity {
        if !(1..=1000).contains(&capacity) {
            return error_response(
                ErrorCode::InvalidRequest,
                "instance capacity must be 1..1000".to_owned(),
                request_id,
            );
        }
        // Ensure InstanceId import is used (avoid unused warning) by also
        // validating that generated ids are valid; no runtime effect.
        // Use assignment to `_` to discard must_use value without `let _ =` lint.
        _ = InstanceId::generate();
    }

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = CreateInstanceCommand {
        actor_id,
        world_id,
        capacity: body.capacity,
        now: build_timestamp(),
        request_id: app_request_id,
    };

    match directory
        .create_instance_with_source_ip(command, context.source_ip().map(str::to_owned))
        .await
    {
        Ok(view) => {
            let response = InstanceResponse {
                id: view.id.to_string(),
                world_id: view.world_id.to_string(),
                status: view.status,
                revision: view.revision.as_u64(),
            };
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

fn parse_instance_id(raw: &str) -> Result<InstanceId, (ErrorCode, String)> {
    match parse_uuid_v7(raw) {
        Ok(u) => InstanceId::new(u).map_err(|e| (ErrorCode::InvalidRequest, e.to_string())),
        Err(code) => Err((code, "invalid instance_id".to_owned())),
    }
}

/// `POST /v1/instances/{instance_id}/start` handler.
pub async fn start_instance(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(instance_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();

    if state.is_shutting_down() {
        return error_response(
            ErrorCode::ServiceUnavailable,
            "server is shutting down".to_owned(),
            request_id,
        );
    }

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let instance_id = match parse_instance_id(&instance_id_raw) {
        Ok(id) => id,
        Err((code, message)) => return error_response(code, message, request_id),
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = StartInstanceCommand {
        actor_id,
        instance_id,
        now: build_timestamp(),
        request_id: app_request_id,
    };

    match directory.start_instance(command).await {
        Ok(view) => {
            let response = InstanceResponse {
                id: view.id.to_string(),
                world_id: view.world_id.to_string(),
                status: view.status,
                revision: view.revision.as_u64(),
            };
            (StatusCode::ACCEPTED, Json(response)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `POST /v1/instances/{instance_id}/stop` handler.
pub async fn stop_instance(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(instance_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();

    if state.is_shutting_down() {
        return error_response(
            ErrorCode::ServiceUnavailable,
            "server is shutting down".to_owned(),
            request_id,
        );
    }

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let instance_id = match parse_instance_id(&instance_id_raw) {
        Ok(id) => id,
        Err((code, message)) => return error_response(code, message, request_id),
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = StopInstanceCommand {
        actor_id,
        instance_id,
        now: build_timestamp(),
        request_id: app_request_id,
    };

    match directory.stop_instance(command).await {
        Ok(view) => {
            let response = InstanceResponse {
                id: view.id.to_string(),
                world_id: view.world_id.to_string(),
                status: view.status,
                revision: view.revision.as_u64(),
            };
            (StatusCode::ACCEPTED, Json(response)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `GET /v1/instances/{instance_id}` handler.
pub async fn get_instance(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(instance_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let instance_id = match parse_instance_id(&instance_id_raw) {
        Ok(id) => id,
        Err((code, message)) => return error_response(code, message, request_id),
    };

    match directory.get_instance(actor_id, instance_id).await {
        Ok(view) => {
            let response = InstanceResponse {
                id: view.id.to_string(),
                world_id: view.world_id.to_string(),
                status: view.status,
                revision: view.revision.as_u64(),
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// Query params for `GET /v1/instances`.
#[derive(Debug, Deserialize)]
pub struct InstanceListQuery {
    /// Opaque pagination cursor.
    pub cursor: Option<String>,
    /// Page size (1..200, default 50).
    pub limit: Option<u16>,
}

#[derive(Debug, Serialize)]
struct InstancePageResponse {
    items: Vec<InstanceResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// `GET /v1/instances` handler.
pub async fn list_instances(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<InstanceListQuery>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let limit = match state.page_limit(q.limit) {
        Ok(v) => v,
        Err(e) => return error_response(e, "invalid limit".to_owned(), request_id),
    };
    let after = q.cursor.map(QueryCursor);
    let page_req = PageRequest {
        limit: u32::from(limit),
        after,
    };

    match directory.list_instances(actor_id, page_req).await {
        Ok(page) => {
            let items = page
                .items
                .into_iter()
                .map(|view| InstanceResponse {
                    id: view.id.to_string(),
                    world_id: view.world_id.to_string(),
                    status: view.status,
                    revision: view.revision.as_u64(),
                })
                .collect::<Vec<_>>();
            let next_cursor = page.next.map(|c| c.0);
            let body = InstancePageResponse { items, next_cursor };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `GET /v1/worlds/{world_id}` handler.
pub async fn get_world(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(world_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let world_id = match WorldId::parse(&world_id_raw) {
        Ok(value) => value,
        Err(detail) => {
            return error_response(ErrorCode::InvalidRequest, detail.to_string(), request_id);
        }
    };

    match directory.get_world(actor_id, world_id).await {
        Ok(view) => (StatusCode::OK, Json(world_response(view))).into_response(),
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `GET /v1/worlds` handler.
pub async fn list_worlds(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<WorldListQuery>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let limit = match state.page_limit(q.limit) {
        Ok(v) => v,
        Err(e) => return error_response(e, "invalid limit".to_owned(), request_id),
    };
    let after = q.cursor.map(QueryCursor);
    let page_req = PageRequest {
        limit: u32::from(limit),
        after,
    };

    match directory.list_worlds(actor_id, page_req).await {
        Ok(page) => {
            let items = page.items.into_iter().map(world_response).collect();
            let next_cursor = page.next.map(|c| c.0);
            let body = WorldPageResponse { items, next_cursor };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `PATCH /v1/worlds/{world_id}` handler.
pub async fn update_world(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(world_id_raw): Path<String>,
    Json(body): Json<UpdateWorldRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let world_id = match WorldId::parse(&world_id_raw) {
        Ok(value) => value,
        Err(detail) => {
            return error_response(ErrorCode::InvalidRequest, detail.to_string(), request_id);
        }
    };

    let if_match_raw = match headers.get("if-match").and_then(|v| v.to_str().ok()) {
        Some(v) => v,
        None => {
            return error_response(
                ErrorCode::InvalidRequest,
                "If-Match header is required".to_owned(),
                request_id,
            );
        }
    };
    let expected_revision = match crate::parse_if_match(if_match_raw) {
        Ok(v) => v,
        Err(code) => return error_response(code, "invalid If-Match header".to_owned(), request_id),
    };

    if body.name.is_none()
        && body.description.is_none()
        && body.capacity.is_none()
        && body.default_spawn.is_none()
    {
        return error_response(
            ErrorCode::InvalidRequest,
            "request body must set at least one field".to_owned(),
            request_id,
        );
    }

    let default_spawn = match body.default_spawn {
        Some(dto) => match build_transform(Some(dto)) {
            Ok(value) => Some(value),
            Err(detail) => {
                return error_response(ErrorCode::InvalidRequest, detail, request_id);
            }
        },
        None => None,
    };

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = UpdateWorldCommand {
        actor_id,
        world_id,
        expected_revision: Revision::from_u64(expected_revision),
        name: body.name,
        description: body.description,
        default_spawn,
        capacity: body.capacity,
        now: build_timestamp(),
        request_id: app_request_id,
    };

    match directory
        .update_world_with_source_ip(command, context.source_ip().map(str::to_owned))
        .await
    {
        Ok(view) => (StatusCode::OK, Json(world_response(view))).into_response(),
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}

/// `POST /v1/worlds/{world_id}/archive` handler.
///
/// `Idempotency-Key` is required by the contract but archiving is already
/// idempotent at the domain level, so the handler validates the key's presence
/// and UUIDv7 shape without a replay store.
pub async fn archive_world(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(world_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let world_id = match WorldId::parse(&world_id_raw) {
        Ok(value) => value,
        Err(detail) => {
            return error_response(ErrorCode::InvalidRequest, detail.to_string(), request_id);
        }
    };

    let idempotency_key_raw = match headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        Some(v) => v,
        None => {
            return error_response(
                ErrorCode::InvalidRequest,
                "Idempotency-Key header is required".to_owned(),
                request_id,
            );
        }
    };
    if parse_uuid_v7(idempotency_key_raw).is_err() {
        return error_response(
            ErrorCode::InvalidRequest,
            "Idempotency-Key must be canonical UUIDv7".to_owned(),
            request_id,
        );
    }

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(directory) = state.world_directory() else {
        return error_response(
            ErrorCode::InternalError,
            "internal error".to_owned(),
            request_id,
        );
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = ArchiveWorldCommand {
        actor_id,
        world_id,
        now: build_timestamp(),
        request_id: app_request_id,
    };

    match directory
        .archive_world_with_source_ip(command, context.source_ip().map(str::to_owned))
        .await
    {
        Ok(view) => (StatusCode::OK, Json(world_response(view))).into_response(),
        Err(error) => {
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id)
        }
    }
}
