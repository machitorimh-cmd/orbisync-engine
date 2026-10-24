//! Bounded audit search and page exports, protected by admin.audit.read.

use crate::{ErrorCode, HttpState, RequestContext};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use orbisync_application::{AuditFilter, PageRequest, QueryCursor};
use serde::{Deserialize, Serialize};

fn error_response(code: ErrorCode, message: &str, request_id: &str) -> Response {
    (
        code.status(),
        Json(serde_json::json!({"error": {
            "code": code.as_str(), "message": message, "request_id": request_id, "details": {}
        }})),
    )
        .into_response()
}
fn map_port_error(err: orbisync_application::IdentityPortError) -> ErrorCode {
    use orbisync_application::IdentityPortError as E;
    match err {
        E::InvalidRequest => ErrorCode::InvalidRequest,
        E::DataCorruption => ErrorCode::InternalError,
        E::Unavailable => ErrorCode::ServiceUnavailable,
        E::NotFound => ErrorCode::ResourceNotFound,
        E::Conflict => ErrorCode::ResourceConflict,
    }
}
/// Exact, composable search parameters; every field is cursor-bound.
#[derive(Debug, Deserialize, Default)]
pub struct AuditListQuery {
    /// Opaque continuation cursor.
    pub cursor: Option<String>,
    /// Page size, from 1 through 200.
    #[serde(default, deserialize_with = "parse_query_limit")]
    pub limit: Option<u16>,
    /// Inclusive RFC3339 lower bound.
    pub from: Option<String>,
    /// Inclusive RFC3339 upper bound.
    pub to: Option<String>,
    /// Canonical UUIDv7 actor.
    pub actor_id: Option<String>,
    /// Exact action.
    pub action: Option<String>,
    /// Exact resource type.
    pub resource_type: Option<String>,
    /// Exact resource identifier.
    pub resource_id: Option<String>,
    /// Exact request correlation identifier.
    pub request_id: Option<String>,
}
fn parse_query_limit<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Option<u16>, D::Error> {
    // Flattened URL query maps carry string values, including numeric limits.
    Option::<String>::deserialize(input)?
        .map(|value| value.parse().map_err(serde::de::Error::custom))
        .transpose()
}
/// One bounded export page, using the same search contract.
#[derive(Debug, Deserialize)]
pub struct AuditExportQuery {
    /// Search filters and continuation.
    #[serde(flatten)]
    pub search: AuditListQuery,
    /// json (default) or csv.
    pub format: Option<String>,
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
fn redact_details(value: serde_json::Value, depth: usize) -> serde_json::Value {
    if depth > 16 {
        return serde_json::Value::Null;
    }
    match value {
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .into_iter()
                .filter(|(key, _)| {
                    let key = key.to_ascii_lowercase().replace('-', "_");
                    !matches!(
                        key.as_str(),
                        "password"
                            | "password_hash"
                            | "authorization"
                            | "cookie"
                            | "token"
                            | "secret"
                            | "source_ip"
                            | "remote_ip"
                            | "ip_address"
                    ) && !key.ends_with("_token")
                        && !key.ends_with("_secret")
                        && !key.ends_with("_password")
                })
                .map(|(key, value)| (key, redact_details(value, depth + 1)))
                .collect(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|v| redact_details(v, depth + 1))
                .collect(),
        ),
        value => value,
    }
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
        details: redact_details(view.details, 0),
    }
}
fn parse_filter(q: &AuditListQuery) -> Result<AuditFilter, ()> {
    let timestamp = |s: &Option<String>| {
        s.as_deref()
            .map(|s| {
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                    .map(orbisync_domain::Timestamp::from_offset_date_time)
                    .map_err(|_| ())
            })
            .transpose()
    };
    let from = timestamp(&q.from)?;
    let to = timestamp(&q.to)?;
    if from.zip(to).is_some_and(|(from, to)| from > to) {
        return Err(());
    }
    for field in [&q.action, &q.resource_type, &q.resource_id, &q.request_id] {
        if field
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(());
        }
    }
    if q.cursor.as_ref().is_some_and(|s| s.len() > 4096) {
        return Err(());
    }
    let actor_id = q
        .actor_id
        .as_deref()
        .map(|s| {
            crate::parse_uuid_v7(s)
                .map_err(|_| ())
                .and_then(|id| orbisync_domain::UserId::new(id).map_err(|_| ()))
        })
        .transpose()?;
    Ok(AuditFilter {
        from,
        to,
        actor_id,
        action: q.action.clone(),
        resource_type: q.resource_type.clone(),
        resource_id: q.resource_id.clone(),
        request_id: q.request_id.clone(),
    })
}
async fn search(
    state: &HttpState,
    headers: &HeaderMap,
    request_id: &str,
    q: AuditListQuery,
) -> Result<AuditPageResponse, Response> {
    crate::authorize(state, headers, request_id, "admin.audit.read").await?;
    let port = state.audit_query().ok_or_else(|| {
        error_response(
            ErrorCode::ServiceUnavailable,
            "audit query unavailable",
            request_id,
        )
    })?;
    let limit = state
        .page_limit(q.limit)
        .map_err(|e| error_response(e, "invalid limit", request_id))?;
    let filter = parse_filter(&q).map_err(|()| {
        error_response(
            ErrorCode::InvalidRequest,
            "invalid audit filters",
            request_id,
        )
    })?;
    let page = port
        .search(
            filter,
            PageRequest {
                limit: u32::from(limit),
                after: q.cursor.map(QueryCursor),
            },
        )
        .await
        .map_err(|e| error_response(map_port_error(e), "audit query failed", request_id))?;
    Ok(AuditPageResponse {
        items: page.items.into_iter().map(audit_view_to_response).collect(),
        next_cursor: page.next.map(|c| c.0),
    })
}
/// Lists one audit page, with compound filtering.
pub async fn list_audit_events(
    State(state): State<HttpState>,
    axum::extract::Extension(context): axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<AuditListQuery>,
) -> Response {
    match search(&state, &headers, context.request_id(), q).await {
        Ok(page) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(page),
        )
            .into_response(),
        Err(response) => response,
    }
}
fn csv_cell(text: &str) -> String {
    let lead =
        text.trim_start_matches(|c: char| c.is_whitespace() || c.is_control() || c == '\u{feff}');
    let dangerous = lead.starts_with(['=', '+', '-', '@']) || text.starts_with(['\t', '\r', '\n']);
    format!(
        "\"{}{}\"",
        if dangerous { "'" } else { "" },
        text.replace('"', "\"\"")
    )
}
fn csv_page(page: &AuditPageResponse) -> String {
    let mut text = String::from(
        "audit_id,timestamp,actor_type,actor_id,action,resource_type,resource_id,result,error_code,request_id,details\r\n",
    );
    for event in &page.items {
        let details = event.details.to_string();
        let fields = [
            &event.audit_id,
            &event.timestamp,
            &event.actor_type,
            event.actor_id.as_deref().unwrap_or(""),
            &event.action,
            &event.resource_type,
            event.resource_id.as_deref().unwrap_or(""),
            &event.result,
            event.error_code.as_deref().unwrap_or(""),
            &event.request_id,
            &details,
        ];
        text.push_str(
            &fields
                .into_iter()
                .map(csv_cell)
                .collect::<Vec<_>>()
                .join(","),
        );
        text.push_str("\r\n");
    }
    text
}
/// Exports one page only. CSV continuation is returned as X-Next-Cursor.
pub async fn export_audit_events(
    State(state): State<HttpState>,
    axum::extract::Extension(context): axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<AuditExportQuery>,
) -> Response {
    let request_id = context.request_id();
    let format = q.format.as_deref().unwrap_or("json");
    if !matches!(format, "json" | "csv") {
        return error_response(
            ErrorCode::InvalidRequest,
            "unsupported export format",
            request_id,
        );
    }
    let page = match search(&state, &headers, request_id, q.search).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    let next = page.next_cursor.clone();
    let mut response = if format == "csv" {
        (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"audit-events.csv\"",
                ),
            ],
            csv_page(&page),
        )
            .into_response()
    } else {
        (
            [(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"audit-events.json\"",
            )],
            Json(page),
        )
            .into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(cursor) = next {
        let Ok(cursor) = HeaderValue::from_str(&cursor) else {
            return error_response(ErrorCode::InternalError, "invalid continuation", request_id);
        };
        response.headers_mut().insert("x-next-cursor", cursor);
    }
    response
}
/// Fetches an audit event with the same redaction as search and export.
pub async fn get_audit_event(
    State(state): State<HttpState>,
    axum::extract::Extension(context): axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    let request_id = context.request_id();
    if let Err(response) = crate::authorize(&state, &headers, request_id, "admin.audit.read").await
    {
        return response;
    }
    let Some(port) = state.audit_query() else {
        return error_response(
            ErrorCode::ServiceUnavailable,
            "audit query unavailable",
            request_id,
        );
    };
    let id = match crate::parse_uuid_v7(&raw) {
        Ok(id) => id,
        Err(e) => return error_response(e, "invalid event_id", request_id),
    };
    match port.event(id).await {
        Ok(Some(view)) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(audit_view_to_response(view)),
        )
            .into_response(),
        Ok(None) => error_response(
            ErrorCode::ResourceNotFound,
            "audit event not found",
            request_id,
        ),
        Err(e) => error_response(map_port_error(e), "audit query failed", request_id),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_neutralizes_formulae_and_quotes_delimiters() {
        for input in [
            "=2+2",
            "+cmd",
            "-2+3",
            "@sum(1)",
            " \t=1",
            "\r=1",
            "\u{feff}=1",
        ] {
            assert!(csv_cell(input).starts_with("\"'"), "{input:?}");
        }
        assert_eq!(csv_cell("a,\"b\"\nc"), "\"a,\"\"b\"\"\nc\"");
        assert_eq!(csv_cell("plain"), "\"plain\"");
    }
    #[test]
    fn metadata_redaction_is_recursive_and_keeps_safe_evidence() {
        let result = redact_details(
            serde_json::json!({"nested": [{"access_token": "secret", "source_ip":"192.0.2.1", "count":3}], "Password":"secret", "request_id":"safe"}),
            0,
        );
        assert_eq!(
            result,
            serde_json::json!({"nested":[{"count":3}],"request_id":"safe"})
        );
    }
    #[test]
    fn invalid_filter_ranges_and_unbounded_values_are_refused() {
        assert!(
            parse_filter(&AuditListQuery {
                from: Some("2026-10-24T00:00:00Z".into()),
                to: Some("2026-10-23T00:00:00Z".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            parse_filter(&AuditListQuery {
                action: Some("a".repeat(257)),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            parse_filter(&AuditListQuery {
                request_id: Some("request".into()),
                resource_type: Some("user".into()),
                ..Default::default()
            })
            .is_ok()
        );
    }
}
