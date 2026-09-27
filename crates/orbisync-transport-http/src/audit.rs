//! Audit read handlers (`GET /v1/audit-events`, `GET /v1/audit-events/{event_id}`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use orbisync_application::{AuditFilter, PageRequest, QueryCursor};
use serde::{Deserialize, Serialize};

use crate::{ErrorCode, HttpState, RequestContext};

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

fn map_port_error(err: orbisync_application::IdentityPortError) -> ErrorCode {
    match err {
        orbisync_application::IdentityPortError::InvalidRequest => ErrorCode::InvalidRequest,
        orbisync_application::IdentityPortError::DataCorruption => ErrorCode::InternalError,
        orbisync_application::IdentityPortError::Unavailable => ErrorCode::ServiceUnavailable,
        orbisync_application::IdentityPortError::NotFound => ErrorCode::ResourceNotFound,
        orbisync_application::IdentityPortError::Conflict => ErrorCode::ResourceConflict,
    }
}

/// Query params for `GET /v1/audit-events`.
#[derive(Debug, Deserialize)]
pub struct AuditListQuery {
    /// Opaque pagination cursor.
    pub cursor: Option<String>,
    /// Page size (1..200, default 50).
    pub limit: Option<u16>,
    /// Filter from timestamp (RFC3339).
    pub from: Option<String>,
    /// Filter to timestamp (RFC3339).
    pub to: Option<String>,
    /// Filter by actor user id (UUIDv7).
    pub actor_id: Option<String>,
    /// Filter by exact action.
    pub action: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuditEventResponse {
    audit_id: String,
    timestamp: String,
    actor_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor_id: Option<String>,
    action: String,
    resource_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_id: Option<String>,
    result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    request_id: String,
    details: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AuditPageResponse {
    items: Vec<AuditEventResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

fn audit_view_to_response(view: orbisync_application::AuditView) -> AuditEventResponse {
    AuditEventResponse {
        audit_id: view.id,
        timestamp: view
            .occurred_at
            .as_offset_date_time()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        actor_type: view.actor_type,
        actor_id: view.actor_id.map(|id| id.to_string()),
        action: view.action,
        resource_type: view.resource_type,
        resource_id: view.resource_id,
        result: view.result,
        error_code: view.error_code,
        request_id: view.request_id,
        details: view.details,
    }
}

/// `GET /v1/audit-events`
pub async fn list_audit_events(
    State(state): State<HttpState>,
    axum::extract::Extension(context): axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<AuditListQuery>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.audit.read").await {
        return resp;
    }
    let Some(audit_query) = state.audit_query() else {
        return error_response(
            ErrorCode::InternalError,
            "audit query unavailable".to_owned(),
            request_id,
        );
    };
    // limit
    let limit = match state.page_limit(q.limit) {
        Ok(v) => v,
        Err(e) => return error_response(e, "invalid limit".to_owned(), request_id),
    };
    // parse filter
    let from = if let Some(s) = q.from {
        match time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339) {
            Ok(dt) => Some(orbisync_domain::Timestamp::from_offset_date_time(dt)),
            Err(_) => {
                return error_response(
                    ErrorCode::InvalidRequest,
                    "invalid from timestamp".to_owned(),
                    request_id,
                );
            }
        }
    } else {
        None
    };
    let to = if let Some(s) = q.to {
        match time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339) {
            Ok(dt) => Some(orbisync_domain::Timestamp::from_offset_date_time(dt)),
            Err(_) => {
                return error_response(
                    ErrorCode::InvalidRequest,
                    "invalid to timestamp".to_owned(),
                    request_id,
                );
            }
        }
    } else {
        None
    };
    let actor_id = if let Some(s) = q.actor_id {
        match crate::parse_uuid_v7(&s) {
            Ok(u) => match orbisync_domain::UserId::new(u) {
                Ok(id) => Some(id),
                Err(e) => {
                    return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id);
                }
            },
            Err(e) => return error_response(e, "invalid actor_id".to_owned(), request_id),
        }
    } else {
        None
    };
    let filter = AuditFilter {
        from,
        to,
        actor_id,
        action: q.action.clone(),
    };
    let after = q.cursor.map(QueryCursor);
    let page_req = PageRequest {
        limit: u32::from(limit),
        after,
    };
    let page = match audit_query.search(filter, page_req).await {
        Ok(p) => p,
        Err(e) => return error_response(map_port_error(e), "query failed".to_owned(), request_id),
    };
    let items = page
        .items
        .into_iter()
        .map(audit_view_to_response)
        .collect::<Vec<_>>();
    let next_cursor = page.next.map(|c| c.0);
    let body = AuditPageResponse { items, next_cursor };
    (StatusCode::OK, Json(body)).into_response()
}

/// `GET /v1/audit-events/{event_id}`
pub async fn get_audit_event(
    State(state): State<HttpState>,
    axum::extract::Extension(context): axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    Path(event_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.audit.read").await {
        return resp;
    }
    let Some(audit_query) = state.audit_query() else {
        return error_response(
            ErrorCode::InternalError,
            "audit query unavailable".to_owned(),
            request_id,
        );
    };
    let event_id = match crate::parse_uuid_v7(&event_id_raw) {
        Ok(v) => v,
        Err(e) => return error_response(e, "invalid event_id".to_owned(), request_id),
    };
    match audit_query.event(event_id).await {
        Ok(Some(view)) => {
            let body = audit_view_to_response(view);
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(None) => error_response(
            ErrorCode::ResourceNotFound,
            "audit event not found".to_owned(),
            request_id,
        ),
        Err(e) => error_response(map_port_error(e), "query failed".to_owned(), request_id),
    }
}
