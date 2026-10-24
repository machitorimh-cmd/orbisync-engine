//! Owner-scoped session inventory and revocation; no token material crosses this API.

use crate::{ErrorCode, HttpState, RequestContext};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use orbisync_application::{
    IdentityPortError, PageRequest, QueryCursor, RequestId, SessionAccess, SessionSelection,
    SessionView, validate_device_label,
};
use orbisync_domain::{AuthSessionId, AuthSessionStatus, Timestamp, UserId};
use serde::{Deserialize, Serialize};

/// Bounded, opaque continuation request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListQuery {
    /// Maximum page size (1..200).
    pub limit: Option<u16>,
    /// Continuation from the previous page.
    pub cursor: Option<String>,
}

/// The label is advisory and never identifies or authenticates a device.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLabelRequest {
    /// String of 1..128 non-control characters, or null to clear.
    pub device_label: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    session_id: String,
    created_at: String,
    expires_at: String,
    revoked_at: Option<String>,
    status: &'static str,
    current: bool,
    device_label: Option<String>,
}

fn error(code: ErrorCode, request_id: &str) -> Response {
    (code.status(), Json(serde_json::json!({"error":{"code":code.as_str(),"message":code.as_str(),"request_id":request_id,"details":{}}}))).into_response()
}
fn port_error(value: IdentityPortError, id: &str) -> Response {
    error(
        match value {
            IdentityPortError::InvalidRequest => ErrorCode::InvalidRequest,
            IdentityPortError::NotFound => ErrorCode::ResourceNotFound,
            IdentityPortError::Conflict => ErrorCode::ResourceConflict,
            IdentityPortError::Unavailable => ErrorCode::ServiceUnavailable,
            IdentityPortError::DataCorruption => ErrorCode::InternalError,
        },
        id,
    )
}
fn session_id(value: &str) -> Result<AuthSessionId, ErrorCode> {
    AuthSessionId::parse(value).map_err(|_| ErrorCode::InvalidRequest)
}
fn user_id(value: &str) -> Result<UserId, ErrorCode> {
    UserId::parse(value).map_err(|_| ErrorCode::InvalidRequest)
}
fn timestamp(value: Timestamp) -> String {
    value
        .as_offset_date_time()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
fn response(value: SessionView, current: AuthSessionId, now: Timestamp) -> SessionResponse {
    SessionResponse {
        session_id: value.id.to_string(),
        created_at: timestamp(value.created_at),
        expires_at: timestamp(value.expires_at),
        revoked_at: value.revoked_at.map(timestamp),
        status: if value.status == AuthSessionStatus::Revoked {
            "revoked"
        } else if now >= value.expires_at {
            "expired"
        } else {
            "active"
        },
        current: value.id == current,
        device_label: value.device_label,
    }
}
async fn access(
    state: &HttpState,
    context: &RequestContext,
    headers: &HeaderMap,
    owner: Option<UserId>,
    permission: Option<&str>,
) -> Result<(SessionAccess, AuthSessionId), Response> {
    let id = context.request_id();
    let (actor, current, now) = crate::authenticate(state, headers, id).await?;
    if let Some(permission) = permission {
        crate::authorize(state, headers, id, permission).await?;
    }
    Ok((
        SessionAccess {
            actor_id: actor,
            user_id: owner.unwrap_or(actor),
            administrator: permission.is_some(),
            request_id: RequestId::new(id).map_err(|_| error(ErrorCode::InternalError, id))?,
            source_ip: context.source_ip().map(str::to_owned),
            now,
        },
        current,
    ))
}
async fn list(
    state: &HttpState,
    access: SessionAccess,
    current: AuthSessionId,
    query: SessionListQuery,
) -> Response {
    let id = access.request_id.as_str();
    let Some(store) = state.session_management() else {
        return error(ErrorCode::ServiceUnavailable, id);
    };
    let limit = match state.page_limit(query.limit) {
        Ok(v) if v <= 200 => v,
        _ => return error(ErrorCode::InvalidRequest, id),
    };
    if query.cursor.as_ref().is_some_and(|v| v.len() > 4096) {
        return error(ErrorCode::InvalidRequest, id);
    }
    match store.list(&access, &PageRequest { limit: u32::from(limit), after: query.cursor.map(QueryCursor) }).await {
        Ok(page) => (StatusCode::OK, [(header::CACHE_CONTROL,"no-store")], Json(serde_json::json!({"items":page.items.into_iter().map(|v| response(v,current,access.now)).collect::<Vec<_>>(),"next_cursor":page.next.map(|v|v.0)}))).into_response(),
        Err(e) => port_error(e,id),
    }
}
async fn revoke(state: &HttpState, access: SessionAccess, selection: SessionSelection) -> Response {
    let id = access.request_id.as_str();
    let Some(store) = state.session_management() else {
        return error(ErrorCode::ServiceUnavailable, id);
    };
    match store.revoke(&access, selection).await {
        Ok(_) if matches!(selection, SessionSelection::One(_)) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(count) => (
            StatusCode::OK,
            Json(serde_json::json!({"revoked_count":count})),
        )
            .into_response(),
        Err(e) => port_error(e, id),
    }
}

/// GET /v1/auth/sessions (only the authenticated user's inventory).
pub async fn list_self(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    query: Result<Query<SessionListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let query = match query {
        Ok(Query(v)) => v,
        Err(_) => return error(ErrorCode::InvalidRequest, context.request_id()),
    };
    match access(&state, &context, &headers, None, None).await {
        Ok((access, current)) => list(&state, access, current, query).await,
        Err(e) => e,
    }
}
/// DELETE /v1/auth/sessions/{session_id}.
pub async fn revoke_one(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    let sid = match session_id(&raw) {
        Ok(v) => v,
        Err(e) => return error(e, context.request_id()),
    };
    match access(&state, &context, &headers, None, None).await {
        Ok((access, _)) => revoke(&state, access, SessionSelection::One(sid)).await,
        Err(e) => e,
    }
}
/// POST /v1/auth/sessions/revoke-others. Keeps the requesting session active.
pub async fn revoke_others(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    match access(&state, &context, &headers, None, None).await {
        Ok((access, current)) => revoke(&state, access, SessionSelection::OtherThan(current)).await,
        Err(e) => e,
    }
}
/// PATCH /v1/auth/sessions/{session_id} (self-only advisory label).
pub async fn label_one(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Result<Json<SessionLabelRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let sid = match session_id(&raw) {
        Ok(v) => v,
        Err(e) => return error(e, context.request_id()),
    };
    let label = match body {
        Ok(Json(v)) => match v.device_label {
            serde_json::Value::Null => None,
            serde_json::Value::String(v) => Some(v),
            _ => return error(ErrorCode::InvalidRequest, context.request_id()),
        },
        Err(_) => return error(ErrorCode::InvalidRequest, context.request_id()),
    };
    if validate_device_label(label.as_deref()).is_err() {
        return error(ErrorCode::InvalidRequest, context.request_id());
    }
    let (access, _) = match access(&state, &context, &headers, None, None).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(store) = state.session_management() else {
        return error(ErrorCode::ServiceUnavailable, context.request_id());
    };
    match store.set_label(&access, sid, label).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => port_error(e, context.request_id()),
    }
}
/// GET /v1/users/{user_id}/sessions, gated by admin.sessions.read.
pub async fn admin_list(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    query: Result<Query<SessionListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let uid = match user_id(&raw) {
        Ok(v) => v,
        Err(e) => return error(e, context.request_id()),
    };
    let query = match query {
        Ok(Query(v)) => v,
        Err(_) => return error(ErrorCode::InvalidRequest, context.request_id()),
    };
    match access(
        &state,
        &context,
        &headers,
        Some(uid),
        Some("admin.sessions.read"),
    )
    .await
    {
        Ok((access, current)) => list(&state, access, current, query).await,
        Err(e) => e,
    }
}
/// DELETE /v1/users/{user_id}/sessions/{session_id}, gated by admin.sessions.revoke.
pub async fn admin_revoke_one(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path((raw_user, raw_session)): Path<(String, String)>,
) -> Response {
    let uid = match user_id(&raw_user) {
        Ok(v) => v,
        Err(e) => return error(e, context.request_id()),
    };
    let sid = match session_id(&raw_session) {
        Ok(v) => v,
        Err(e) => return error(e, context.request_id()),
    };
    match access(
        &state,
        &context,
        &headers,
        Some(uid),
        Some("admin.sessions.revoke"),
    )
    .await
    {
        Ok((access, _)) => revoke(&state, access, SessionSelection::One(sid)).await,
        Err(e) => e,
    }
}
/// DELETE /v1/users/{user_id}/sessions, gated by admin.sessions.revoke.
pub async fn admin_revoke_all(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    let uid = match user_id(&raw) {
        Ok(v) => v,
        Err(e) => return error(e, context.request_id()),
    };
    match access(
        &state,
        &context,
        &headers,
        Some(uid),
        Some("admin.sessions.revoke"),
    )
    .await
    {
        Ok((access, _)) => revoke(&state, access, SessionSelection::All).await,
        Err(e) => e,
    }
}
