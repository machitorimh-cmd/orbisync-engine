//! Role administration HTTP handlers (`POST /v1/roles`, `PUT /v1/users/{user_id}/roles`).

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use orbisync_application::{ApplicationErrorKind, CreateRoleCommand, UpdateRoleCommand};
use orbisync_domain::{Permission, Revision, Role, RoleId, UserId};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{ErrorCode, HttpState, RequestContext};

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/roles`.
#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    /// Role name 1..128 non-control.
    pub name: String,
    /// Optional description up to 1024.
    pub description: Option<String>,
    /// Allow-only permission names.
    pub permissions: Vec<String>,
}

/// Distinguishes an absent key from an explicit `null` in a
/// `application/merge-patch+json` body (RFC 7396): absent leaves the field
/// unchanged, `null` clears it. Plain `#[serde(default)]` on `Option<T>`
/// cannot tell these apart because both deserialize to `None`; wrapping the
/// deserialized value in `Some` here means "the key was present" survives
/// into the outer `Option`.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Request body for `PATCH /v1/roles/{role_id}` (`application/merge-patch+json`).
#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    /// New name, when present.
    pub name: Option<String>,
    /// New description; key absent leaves it unchanged, `null` clears it.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub description: Option<Option<String>>,
    /// New permission set (full replacement), when present.
    pub permissions: Option<Vec<String>>,
}

/// Request body for `PUT /v1/users/{user_id}/roles`.
#[derive(Debug, Deserialize)]
pub struct ReplaceUserRolesRequest {
    /// Complete replacement set.
    pub role_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RoleResponse {
    id: String,
    name: String,
    description: Option<String>,
    permissions: Vec<String>,
    revision: u64,
}

#[derive(Debug, Serialize)]
struct UserRoleAssignmentResponse {
    user_id: String,
    role_ids: Vec<String>,
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

fn map_application_error(kind: ApplicationErrorKind) -> ErrorCode {
    match kind {
        ApplicationErrorKind::DomainRule => ErrorCode::InvalidRequest,
        ApplicationErrorKind::NotFound => ErrorCode::ResourceNotFound,
        ApplicationErrorKind::NotAuthorized => ErrorCode::AccessDenied,
        ApplicationErrorKind::Conflict => ErrorCode::ResourceConflict,
        ApplicationErrorKind::PortFailure => ErrorCode::InternalError,
        ApplicationErrorKind::Unauthenticated => ErrorCode::AuthenticationRequired,
        ApplicationErrorKind::RateLimited => ErrorCode::RateLimited,
        _ => ErrorCode::InternalError,
    }
}

/// `POST /v1/roles`.
///
/// Validates `Authorization: Bearer <access token>` (real JWT, audience
/// isolated – realtime tickets are 401), loads server-owned roles, then
/// delegates to `IdentityAdministrationService::create_role` which enforces
/// `admin.roles.create`. Returns 201 with `Role`.
pub async fn create_role(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<CreateRoleRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
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
    let actor_roles = match repo.roles_for_user(actor_id).await {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };

    // Validate and collect permissions via domain invariants.
    let mut permissions = BTreeSet::new();
    for raw in body.permissions {
        match Permission::new(raw.clone()) {
            Ok(p) => {
                permissions.insert(p);
            }
            Err(e) => {
                return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id);
            }
        }
    }

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = CreateRoleCommand {
        name: body.name,
        description: body.description,
        permissions,
        actor_id,
        request_id: app_request_id,
    };

    match admin
        .create_role_with_source_ip(
            command,
            &actor_roles,
            context.source_ip().map(str::to_owned),
        )
        .await
    {
        Ok(role) => {
            let response = RoleResponse {
                id: role.id().to_string(),
                name: role.name().to_owned(),
                description: role.description().map(str::to_owned),
                permissions: role
                    .permissions()
                    .iter()
                    .map(|p| p.as_str().to_owned())
                    .collect(),
                revision: role.revision().as_u64(),
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

/// `PUT /v1/users/{user_id}/roles`.
///
/// Replaces the target user's role assignments with the exact server-validated
/// set. Requires `admin.roles.assign`.
pub async fn replace_user_roles(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    Path(user_id_raw): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ReplaceUserRolesRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let target_user_id = match crate::parse_uuid_v7(&user_id_raw) {
        Ok(uuid) => match UserId::new(uuid) {
            Ok(id) => id,
            Err(e) => {
                return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id);
            }
        },
        Err(code) => {
            return error_response(
                code,
                "user_id must be canonical UUIDv7".to_owned(),
                request_id,
            );
        }
    };

    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
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
    let actor_roles = match repo.roles_for_user(actor_id).await {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };

    if body.role_ids.len() > 100 {
        return error_response(
            ErrorCode::InvalidRequest,
            "role_ids must contain at most 100 entries".to_owned(),
            request_id,
        );
    }

    let mut role_ids = BTreeSet::new();
    let mut seen = std::collections::HashSet::new();
    for raw in &body.role_ids {
        if !seen.insert(raw.clone()) {
            return error_response(
                ErrorCode::InvalidRequest,
                "role_ids must be unique".to_owned(),
                request_id,
            );
        }
        let uuid = match crate::parse_uuid_v7(raw) {
            Ok(v) => v,
            Err(_) => {
                return error_response(
                    ErrorCode::InvalidRequest,
                    format!("role_id {raw} must be canonical UUIDv7"),
                    request_id,
                );
            }
        };
        let role_id = match RoleId::new(uuid) {
            Ok(id) => id,
            Err(e) => {
                return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id);
            }
        };
        role_ids.insert(role_id);
    }

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    match admin
        .assign_roles_with_source_ip(
            target_user_id,
            role_ids.clone(),
            actor_id,
            app_request_id,
            &actor_roles,
            context.source_ip().map(str::to_owned),
        )
        .await
    {
        Ok(()) => {
            let mut sorted: Vec<String> = role_ids.iter().map(|id| id.to_string()).collect();
            sorted.sort();
            let response = UserRoleAssignmentResponse {
                user_id: target_user_id.to_string(),
                role_ids: sorted,
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
/// Query params for `GET /v1/roles`.
#[derive(Debug, Deserialize)]
pub struct RoleListQuery {
    /// Opaque pagination cursor.
    pub cursor: Option<String>,
    /// Page size (1..200, default 50).
    pub limit: Option<u16>,
}

#[derive(Debug, Serialize)]
struct RolePageResponse {
    items: Vec<RoleResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

fn map_query_error_roles(err: orbisync_application::IdentityPortError) -> ErrorCode {
    match err {
        orbisync_application::IdentityPortError::InvalidRequest => ErrorCode::InvalidRequest,
        orbisync_application::IdentityPortError::DataCorruption => ErrorCode::InternalError,
        orbisync_application::IdentityPortError::Unavailable => ErrorCode::ServiceUnavailable,
        orbisync_application::IdentityPortError::NotFound => ErrorCode::ResourceNotFound,
        orbisync_application::IdentityPortError::Conflict => ErrorCode::ResourceConflict,
    }
}

/// `GET /v1/roles`
pub async fn list_roles(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<RoleListQuery>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.roles.read").await {
        return resp;
    }
    let Some(query) = state.identity_query() else {
        return error_response(
            ErrorCode::InternalError,
            "identity query unavailable".to_owned(),
            request_id,
        );
    };
    let limit = match state.page_limit(q.limit) {
        Ok(v) => v,
        Err(e) => return error_response(e, "invalid limit".to_owned(), request_id),
    };
    let after = q.cursor.map(orbisync_application::QueryCursor);
    let page_req = orbisync_application::PageRequest {
        limit: u32::from(limit),
        after,
    };
    let page = match query.roles(page_req).await {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                map_query_error_roles(e),
                "query failed".to_owned(),
                request_id,
            );
        }
    };
    let items = page
        .items
        .into_iter()
        .map(|r| RoleResponse {
            id: r.id.to_string(),
            name: r.name.to_owned(),
            description: r.description.clone(),
            permissions: r.permissions.clone(),
            revision: r.revision,
        })
        .collect::<Vec<_>>();
    let next_cursor = page.next.map(|c| c.0);
    let body = RolePageResponse { items, next_cursor };
    (StatusCode::OK, Json(body)).into_response()
}

/// `GET /v1/roles/{role_id}`
pub async fn get_role(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(role_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.roles.read").await {
        return resp;
    }
    let Some(query) = state.identity_query() else {
        return error_response(
            ErrorCode::InternalError,
            "identity query unavailable".to_owned(),
            request_id,
        );
    };
    let role_id = match crate::parse_uuid_v7(&role_id_raw) {
        Ok(u) => match RoleId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid role_id".to_owned(), request_id),
    };
    let role = match query.role(role_id).await {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                map_query_error_roles(e),
                "query failed".to_owned(),
                request_id,
            );
        }
    };
    match role {
        Some(r) => {
            let body = RoleResponse {
                id: r.id.to_string(),
                name: r.name.to_owned(),
                description: r.description.clone(),
                permissions: r.permissions.clone(),
                revision: r.revision,
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        None => error_response(
            ErrorCode::ResourceNotFound,
            "role not found".to_owned(),
            request_id,
        ),
    }
}

/// `PATCH /v1/roles/{role_id}` with optimistic lock via `If-Match`
/// (`application/merge-patch+json`).
///
/// Requires `admin.roles.update`, enforced by
/// `IdentityAdministrationService::update_role_with_source_ip` (not
/// pre-checked here, mirroring `create_role`'s simpler pattern rather than
/// `delete_role`'s redundant manual pre-check).
pub async fn update_role(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(role_id_raw): Path<String>,
    Json(body): Json<UpdateRoleRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let role_id = match crate::parse_uuid_v7(&role_id_raw) {
        Ok(u) => match RoleId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid role_id".to_owned(), request_id),
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

    if body.name.is_none() && body.description.is_none() && body.permissions.is_none() {
        return error_response(
            ErrorCode::InvalidRequest,
            "request body must set at least one field".to_owned(),
            request_id,
        );
    }
    if let Some(name) = &body.name
        && !(1..=128).contains(&name.chars().count())
    {
        return error_response(
            ErrorCode::InvalidRequest,
            "name must be 1..128 characters".to_owned(),
            request_id,
        );
    }

    let permissions = match body.permissions {
        Some(raw) => {
            let mut set = BTreeSet::new();
            for value in raw {
                match Permission::new(value) {
                    Ok(p) => {
                        set.insert(p);
                    }
                    Err(e) => {
                        return error_response(
                            ErrorCode::InvalidRequest,
                            e.to_string(),
                            request_id,
                        );
                    }
                }
            }
            Some(set)
        }
        None => None,
    };

    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
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
    let actor_roles = match repo.roles_for_user(actor_id).await {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let Some(query) = state.identity_query() else {
        return error_response(
            ErrorCode::InternalError,
            "identity query unavailable".to_owned(),
            request_id,
        );
    };
    let current = match query.role(role_id).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return error_response(
                ErrorCode::ResourceNotFound,
                "role not found".to_owned(),
                request_id,
            );
        }
        Err(e) => {
            return error_response(
                map_query_error_roles(e),
                "query failed".to_owned(),
                request_id,
            );
        }
    };
    let current_permissions = match current
        .permissions
        .iter()
        .map(|p| Permission::new(p.clone()))
        .collect::<Result<BTreeSet<_>, _>>()
    {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let current_role = Role::reconstitute(
        current.id,
        current.name,
        current.description,
        current_permissions,
        Revision::from_u64(current.revision),
    );

    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let command = UpdateRoleCommand {
        role_id,
        expected_revision,
        name: body.name,
        description: body.description,
        permissions,
        actor_id,
        request_id: app_request_id,
    };

    match admin
        .update_role_with_source_ip(
            current_role,
            command,
            &actor_roles,
            context.source_ip().map(str::to_owned),
        )
        .await
    {
        Ok(role) => {
            let response = RoleResponse {
                id: role.id().to_string(),
                name: role.name().to_owned(),
                description: role.description().map(str::to_owned),
                permissions: role
                    .permissions()
                    .iter()
                    .map(|p| p.as_str().to_owned())
                    .collect(),
                revision: role.revision().as_u64(),
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

/// `DELETE /v1/roles/{role_id}` with optimistic lock via `If-Match`.
///
/// Requires `admin.roles.delete`. Uses `crate::authenticate` (CR-13) and
/// `crate::parse_if_match` (CR-17 wiring) in the hot path.
pub async fn delete_role(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(role_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();

    // Validate role_id first so 400 is returned before auth when format is wrong,
    // mirroring other handlers' order (parse then auth).
    let role_id = match crate::parse_uuid_v7(&role_id_raw) {
        Ok(u) => match RoleId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid role_id".to_owned(), request_id),
    };

    // If-Match is required and must be in the quoted form `"123"`.
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

    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
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
    let actor_roles = match repo.roles_for_user(actor_id).await {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let permission = match Permission::new("admin.roles.delete".to_owned()) {
        Ok(p) => p,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    if orbisync_identity::RbacAuthorizer::authorize(&actor_roles, &permission)
        != orbisync_identity::AuthorizationDecision::Allow
    {
        return error_response(
            ErrorCode::AccessDenied,
            "permission denied".to_owned(),
            request_id,
        );
    }

    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    match admin
        .delete_role_with_source_ip(
            role_id,
            expected_revision,
            actor_id,
            app_request_id,
            &actor_roles,
            context.source_ip().map(str::to_owned),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
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
