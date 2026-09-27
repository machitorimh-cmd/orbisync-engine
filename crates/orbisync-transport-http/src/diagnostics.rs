//! Protected operational diagnostics endpoint.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::{ErrorCode, HttpState, RequestContext};

/// Permission required by `GET /v1/admin/diagnostics`.
pub const PERMISSION_READ_DIAGNOSTICS: &str = "admin.diagnostics.read";

#[derive(Debug, Serialize)]
struct CheckpointResponse {
    latest_at: Option<String>,
    save_failures_total: u64,
    restore_failures_total: u64,
    save_rejected_too_large_total: u64,
    restore_rejected_too_large_total: u64,
}

#[derive(Debug, Serialize)]
struct MailboxQueueResponse {
    last_observed_depth: i64,
    capacity_per_instance: u64,
}

#[derive(Debug, Serialize)]
struct OutboundQueueResponse {
    last_observed_depth: i64,
    depth_high_water: i64,
    capacity_per_connection: u64,
    last_observed_bytes: i64,
    bytes_high_water: i64,
}

#[derive(Debug, Serialize)]
struct QueueResponse {
    control: MailboxQueueResponse,
    transform: MailboxQueueResponse,
    entity: MailboxQueueResponse,
    outbound: OutboundQueueResponse,
}

#[derive(Debug, Serialize)]
struct RateLimitResponse {
    connection: u64,
    user: u64,
    ip: u64,
    password_hash: u64,
    instance: u64,
}

#[derive(Debug, Serialize)]
struct ExtensionResponse {
    outbox_pending: i64,
    deliveries_in_flight: i64,
    dead_letters: u64,
}

#[derive(Debug, Serialize)]
struct RetentionResponse {
    realtime_tickets_last_success_at: Option<String>,
    idempotency_records_last_success_at: Option<String>,
    audit_source_ips_last_success_at: Option<String>,
    extension_outbox_last_success_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiagnosticsResponse {
    collected_at: String,
    checkpoint: CheckpointResponse,
    queues: QueueResponse,
    active_connections: i64,
    rate_limit_rejections_total: RateLimitResponse,
    extensions: ExtensionResponse,
    retention: RetentionResponse,
}

fn timestamp(value: Option<orbisync_domain::Timestamp>) -> Option<String> {
    value.and_then(|value| value.to_rfc3339().ok())
}

fn to_response(snapshot: orbisync_application::OperationalDiagnostics) -> DiagnosticsResponse {
    DiagnosticsResponse {
        collected_at: snapshot.collected_at.to_string(),
        checkpoint: CheckpointResponse {
            latest_at: timestamp(snapshot.latest_checkpoint_at),
            save_failures_total: snapshot.checkpoint_save_failures_total,
            restore_failures_total: snapshot.checkpoint_restore_failures_total,
            save_rejected_too_large_total: snapshot.checkpoint_save_rejected_total,
            restore_rejected_too_large_total: snapshot.checkpoint_restore_rejected_total,
        },
        queues: QueueResponse {
            control: MailboxQueueResponse {
                last_observed_depth: snapshot.queues.control,
                capacity_per_instance: snapshot.queue_limits.control_per_instance,
            },
            transform: MailboxQueueResponse {
                last_observed_depth: snapshot.queues.transform,
                capacity_per_instance: snapshot.queue_limits.transform_per_instance,
            },
            entity: MailboxQueueResponse {
                last_observed_depth: snapshot.queues.entity,
                capacity_per_instance: snapshot.queue_limits.entity_per_instance,
            },
            outbound: OutboundQueueResponse {
                last_observed_depth: snapshot.queues.outbound,
                depth_high_water: snapshot.queues.outbound_high_water,
                capacity_per_connection: snapshot.queue_limits.outbound_per_connection,
                last_observed_bytes: snapshot.queues.outbound_bytes,
                bytes_high_water: snapshot.queues.outbound_bytes_high_water,
            },
        },
        active_connections: snapshot.active_connections,
        rate_limit_rejections_total: RateLimitResponse {
            connection: snapshot.rate_limit_rejections.connection,
            user: snapshot.rate_limit_rejections.user,
            ip: snapshot.rate_limit_rejections.ip,
            password_hash: snapshot.rate_limit_rejections.password_hash,
            instance: snapshot.rate_limit_rejections.instance,
        },
        extensions: ExtensionResponse {
            outbox_pending: snapshot.extension_outbox_pending,
            deliveries_in_flight: snapshot.extension_deliveries_in_flight,
            dead_letters: snapshot.extension_dead_letters,
        },
        retention: RetentionResponse {
            realtime_tickets_last_success_at: timestamp(snapshot.retention.realtime_tickets),
            idempotency_records_last_success_at: timestamp(snapshot.retention.idempotency_records),
            audit_source_ips_last_success_at: timestamp(snapshot.retention.audit_source_ips),
            extension_outbox_last_success_at: timestamp(snapshot.retention.extension_outbox),
        },
    }
}

/// `GET /v1/admin/diagnostics`.
pub async fn get_operational_diagnostics(
    State(state): State<HttpState>,
    axum::extract::Extension(context): axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(response) =
        crate::authorize(&state, &headers, &request_id, PERMISSION_READ_DIAGNOSTICS).await
    {
        return response;
    }
    let Some(diagnostics) = state.operational_diagnostics() else {
        return crate::auth_error_response(
            ErrorCode::InternalError,
            "operational diagnostics unavailable".to_owned(),
            request_id,
        );
    };
    match diagnostics.snapshot().await {
        Ok(snapshot) => (StatusCode::OK, Json(to_response(snapshot))).into_response(),
        Err(error) => {
            tracing::warn!(
                event = "operational_diagnostics.collection_failed",
                error_kind = %error.kind(),
                request_id,
                "protected operational diagnostics collection failed"
            );
            crate::auth_error_response(
                ErrorCode::ServiceUnavailable,
                "operational diagnostics unavailable".to_owned(),
                request_id,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use orbisync_application::{
        ApplicationError, IdentityRepository, OperationalDiagnostics, OperationalDiagnosticsPort,
        OperationalQueueLimits, OperationalQueueSnapshot, OperationalRateLimitSnapshot,
        OperationalRetentionSnapshot,
    };
    use orbisync_domain::{
        AuthSession, AuthSessionId, Permission, Role, RoleId, Timestamp, UserId,
    };
    use orbisync_identity::token::AccessTokenService;
    use orbisync_testkit::{FakeIdentityStore, FixedClock};
    use tower::ServiceExt as _;

    use super::to_response;
    use crate::{HttpState, router};

    const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
    const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

    fn snapshot(at: Timestamp) -> OperationalDiagnostics {
        OperationalDiagnostics {
            collected_at: at,
            latest_checkpoint_at: Some(at),
            checkpoint_save_failures_total: 2,
            checkpoint_restore_failures_total: 3,
            checkpoint_save_rejected_total: 1,
            checkpoint_restore_rejected_total: 1,
            queues: OperationalQueueSnapshot {
                control: 4,
                transform: 5,
                entity: 6,
                outbound: 7,
                outbound_high_water: 8,
                outbound_bytes: 9,
                outbound_bytes_high_water: 10,
            },
            queue_limits: OperationalQueueLimits {
                control_per_instance: 64,
                transform_per_instance: 256,
                entity_per_instance: 256,
                outbound_per_connection: 256,
            },
            active_connections: 11,
            rate_limit_rejections: OperationalRateLimitSnapshot {
                connection: 12,
                user: 13,
                ip: 14,
                password_hash: 15,
                instance: 16,
            },
            extension_outbox_pending: 17,
            extension_deliveries_in_flight: 18,
            extension_dead_letters: 19,
            retention: OperationalRetentionSnapshot {
                realtime_tickets: Some(at),
                idempotency_records: None,
                audit_source_ips: Some(at),
                extension_outbox: None,
            },
        }
    }

    struct FakeDiagnostics(OperationalDiagnostics);

    #[async_trait::async_trait]
    impl OperationalDiagnosticsPort for FakeDiagnostics {
        async fn snapshot(&self) -> Result<OperationalDiagnostics, ApplicationError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn response_contains_summary_without_identifiers_or_secrets() {
        let at = Timestamp::from_unix_millis(1_700_000_000_000).expect("timestamp");
        let response = to_response(snapshot(at));
        let value = serde_json::to_value(response).expect("serialize");
        assert_eq!(value["active_connections"], 11);
        assert_eq!(value["extensions"]["dead_letters"], 19);
        assert_eq!(
            value["retention"]["idempotency_records_last_success_at"],
            serde_json::Value::Null
        );
        let encoded = value.to_string();
        assert!(!encoded.contains("access_token"));
        assert!(!encoded.contains("refresh_token"));
        assert!(!encoded.contains("temporary_password"));
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("user_id"));
        assert!(!encoded.contains("instance_id"));
    }

    #[tokio::test]
    async fn endpoint_requires_the_diagnostics_permission() {
        let at = Timestamp::from_unix_millis(1_700_000_000_000).expect("timestamp");
        let clock = Arc::new(FixedClock::new(at));
        let store = Arc::new(FakeIdentityStore::new());
        let user_id = UserId::generate();
        let session_id = AuthSessionId::generate();
        let session = AuthSession::new(
            session_id,
            user_id,
            at,
            at.checked_add_millis(60_000).expect("expiry"),
        )
        .expect("session");
        let repo: Arc<dyn IdentityRepository> = store.clone();
        repo.save_session(&session).await.expect("save session");
        let allowed_role = Role::new(
            RoleId::generate(),
            "Diagnostics Reader",
            None,
            [Permission::new(super::PERMISSION_READ_DIAGNOSTICS).expect("permission")],
        )
        .expect("role");

        let tokens = Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test-key",
            )
            .expect("token service"),
        );
        let token = tokens.issue(user_id, session_id, at).expect("access token");
        let state = HttpState::new(
            vec![],
            "orbisync",
            "test",
            1,
            clock as Arc<dyn orbisync_domain::Clock>,
            900,
            2_592_000,
            b"test-refresh-key".to_vec(),
        )
        .with_token_service(tokens)
        .with_identity_repository(repo)
        .with_operational_diagnostics(Arc::new(FakeDiagnostics(snapshot(at))));
        let app = router(state);

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::get("/v1/admin/diagnostics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let forbidden = app
            .clone()
            .oneshot(
                Request::get("/v1/admin/diagnostics")
                    .header("authorization", format!("Bearer {}", token.expose_secret()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        store.set_roles(user_id, vec![allowed_role]);

        let authorized = app
            .oneshot(
                Request::get("/v1/admin/diagnostics")
                    .header("authorization", format!("Bearer {}", token.expose_secret()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(authorized.status(), StatusCode::OK);
    }
}
