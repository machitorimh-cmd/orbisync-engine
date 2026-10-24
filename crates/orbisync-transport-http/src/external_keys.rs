//! Explicit, authorized reload of the configured local verification keys.
use crate::{ErrorCode, HttpState, RequestContext};
use axum::{
    Json,
    extract::{Extension, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
fn error(code: ErrorCode, request_id: &str, message: &str) -> Response {
    (code.status(), Json(serde_json::json!({"error":{"code":code.as_str(),"message":message,"request_id":request_id,"details":{}}}))).into_response()
}
/// Reloads only the configured local file. No request-supplied path is accepted.
pub async fn reload(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    let request_id = context.request_id();
    let actor = match crate::authorize(&state, &headers, request_id, "admin.auth.keys.reload").await
    {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Some((provider, audit)) = state.external_key_management() else {
        return error(
            ErrorCode::AuthMethodDisabled,
            request_id,
            "external key management is not enabled",
        );
    };
    let Ok(correlation) = orbisync_application::RequestId::new(request_id) else {
        return error(
            ErrorCode::InternalError,
            request_id,
            "invalid request context",
        );
    };
    let now = state.clock().now();
    // Publication and mandatory audit complete even if the client disconnects
    // after submitting an authorized operation.
    match tokio::spawn(async move {
        provider
            .reload_keys(audit.as_ref(), actor, correlation, now)
            .await
    })
    .await
    {
        Ok(Ok(summary)) => Json(summary).into_response(),
        Ok(Err(e)) if e.kind() == orbisync_application::ApplicationErrorKind::DomainRule => error(
            ErrorCode::InvalidRequest,
            request_id,
            "configured external key file is invalid; current keys retained",
        ),
        _ => error(
            ErrorCode::ServiceUnavailable,
            request_id,
            "key reload unavailable; inspect audit before retrying",
        ),
    }
}
