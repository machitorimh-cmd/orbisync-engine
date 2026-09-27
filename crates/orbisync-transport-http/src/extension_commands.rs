//! Separate extension-only HTTP boundary. User JWTs and stub bearer are never accepted.
use crate::{
    ErrorCode, HttpState, RequestContext,
    worlds::{error_response, map_application_error},
};
use axum::{
    Json,
    extract::{Extension, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use orbisync_application::{SecretString, extension_command::ExtensionCommand};
use orbisync_domain::{EntityId, InstanceId};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "command", deny_unknown_fields)]
pub(crate) enum CommandBody {
    #[serde(rename = "entity.get")]
    EntityGet {
        instance_id: String,
        entity_id: String,
    },
    #[serde(rename = "audit.get")]
    AuditGet { event_id: String },
}

pub(crate) async fn execute(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    body: Result<Json<CommandBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let failure = |code, message: &str| {
        error_response(code, message.to_owned(), context.request_id().to_owned())
    };
    let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return failure(
            ErrorCode::AuthenticationRequired,
            "extension authentication required",
        );
    };
    let Ok(Json(body)) = body else {
        return failure(ErrorCode::InvalidRequest, "invalid extension command");
    };
    let command = match body {
        CommandBody::EntityGet {
            instance_id,
            entity_id,
        } => match (InstanceId::parse(&instance_id), EntityId::parse(&entity_id)) {
            (Ok(instance_id), Ok(entity_id)) => ExtensionCommand::EntityGet {
                instance_id,
                entity_id,
            },
            _ => return failure(ErrorCode::InvalidRequest, "invalid resource identifier"),
        },
        CommandBody::AuditGet { event_id } => match crate::parse_uuid_v7(&event_id) {
            Ok(event_id) => ExtensionCommand::AuditGet { event_id },
            Err(_) => return failure(ErrorCode::InvalidRequest, "invalid resource identifier"),
        },
    };
    let Some(gateway) = state.extension_commands.as_ref() else {
        return failure(
            ErrorCode::ServiceUnavailable,
            "extension gateway unavailable",
        );
    };
    match gateway.execute(SecretString::new(token), command).await {
        Ok(result) => Json(serde_json::json!({"result":result})).into_response(),
        Err(error) => failure(
            map_application_error(error.kind()),
            "extension command failed",
        ),
    }
}
