//! Instance membership HTTP handlers (`GET .../members`, `POST .../kick/{user_id}`).
//!
//! Mirrors the `worlds::WorldDirectory` adapter pattern: an object-safe port
//! trait implemented for `InstanceMembershipUseCase`, blanket-injected into
//! `HttpState` by the composition root (`orbisync-server`), which owns the
//! runtime registry this port is ultimately backed by.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use orbisync_application::{
    ApplicationError, InstanceMembershipStore, InstanceMembershipUseCase, Page, PageRequest,
    QueryCursor, WorldAuthorizer, WorldDirectoryStore,
};
use orbisync_domain::{InstanceId, UserId};
use serde::{Deserialize, Serialize};

use crate::worlds::{build_timestamp, error_response, map_application_error, require_bearer};
use crate::{ErrorCode, RequestContext, parse_uuid_v7};

/// Object-safe instance-membership port used by the HTTP adapter.
#[async_trait::async_trait]
pub trait InstanceMembership: Send + Sync + 'static {
    /// Lists the live members of an instance.
    async fn list_members(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
        page: PageRequest,
    ) -> Result<Page<UserId>, ApplicationError>;

    /// Removes every live presence held by `target_user_id`.
    async fn kick_member(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
        target_user_id: UserId,
    ) -> Result<(), ApplicationError>;
}

#[async_trait::async_trait]
impl<S, M, A> InstanceMembership for InstanceMembershipUseCase<S, M, A>
where
    S: WorldDirectoryStore,
    M: InstanceMembershipStore,
    A: WorldAuthorizer,
{
    async fn list_members(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
        page: PageRequest,
    ) -> Result<Page<UserId>, ApplicationError> {
        InstanceMembershipUseCase::list_members(self, actor_id, instance_id, page).await
    }

    async fn kick_member(
        &self,
        actor_id: UserId,
        instance_id: InstanceId,
        target_user_id: UserId,
    ) -> Result<(), ApplicationError> {
        let now = build_timestamp();
        let request_id =
            orbisync_application::RequestId::new(format!("req_{}", uuid::Uuid::now_v7()))
                .map_err(|e| ApplicationError::port_failure(e.to_string()))?;
        InstanceMembershipUseCase::kick_member(
            self,
            actor_id,
            instance_id,
            target_user_id,
            now,
            request_id,
        )
        .await
    }
}

#[derive(Debug, Serialize)]
struct MemberResponse {
    user_id: String,
}

#[derive(Debug, Serialize)]
struct MemberPageResponse {
    items: Vec<MemberResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Query params for `GET /v1/instances/{instance_id}/members`.
#[derive(Debug, Deserialize)]
pub struct MemberListQuery {
    /// Opaque pagination cursor.
    pub cursor: Option<String>,
    /// Page size (1..200, default 50).
    pub limit: Option<u16>,
}

fn parse_instance_id(raw: &str) -> Result<InstanceId, (ErrorCode, String)> {
    match parse_uuid_v7(raw) {
        Ok(u) => InstanceId::new(u).map_err(|e| (ErrorCode::InvalidRequest, e.to_string())),
        Err(code) => Err((code, "invalid instance_id".to_owned())),
    }
}

fn parse_user_id(raw: &str) -> Result<UserId, (ErrorCode, String)> {
    match parse_uuid_v7(raw) {
        Ok(u) => UserId::new(u).map_err(|e| (ErrorCode::InvalidRequest, e.to_string())),
        Err(code) => Err((code, "invalid user_id".to_owned())),
    }
}

/// `GET /v1/instances/{instance_id}/members` handler.
pub async fn list_members(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(instance_id_raw): Path<String>,
    Query(q): Query<MemberListQuery>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let actor_id = match require_bearer(&headers, &state, &request_id).await {
        Ok(id) => id,
        Err(code) => return error_response(code, "authentication required".to_owned(), request_id),
    };

    let Some(membership) = state.instance_membership() else {
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

    let limit = match state.page_limit(q.limit) {
        Ok(v) => v,
        Err(e) => return error_response(e, "invalid limit".to_owned(), request_id),
    };
    let after = q.cursor.map(QueryCursor);
    let page_req = PageRequest {
        limit: u32::from(limit),
        after,
    };

    match membership
        .list_members(actor_id, instance_id, page_req)
        .await
    {
        Ok(page) => {
            let items = page
                .items
                .into_iter()
                .map(|id| MemberResponse {
                    user_id: id.to_string(),
                })
                .collect::<Vec<_>>();
            let next_cursor = page.next.map(|c| c.0);
            let body = MemberPageResponse { items, next_cursor };
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

/// `POST /v1/instances/{instance_id}/kick/{user_id}` handler.
pub async fn kick_member(
    State(state): State<crate::HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path((instance_id_raw, user_id_raw)): Path<(String, String)>,
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

    let Some(membership) = state.instance_membership() else {
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
    let target_user_id = match parse_user_id(&user_id_raw) {
        Ok(id) => id,
        Err((code, message)) => return error_response(code, message, request_id),
    };

    match membership
        .kick_member(actor_id, instance_id, target_user_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fix74_membership_unavailable_maps_to_retryable_http_status() {
        let code = map_application_error(orbisync_application::ApplicationErrorKind::Unavailable);
        assert_eq!(code, ErrorCode::ServiceUnavailable);
        let response = error_response(code, "membership unavailable".into(), "request".into());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("error body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("error JSON");
        assert_eq!(body["error"]["code"], "SERVICE_UNAVAILABLE");
    }
}
