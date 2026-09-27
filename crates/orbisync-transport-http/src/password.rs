//! Password change HTTP handler (`POST /v1/auth/change-password`).

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use orbisync_application::{
    ApplicationErrorKind, IdempotencyClaim, IdempotencyClaimCommand, SecretString,
};
use serde::Deserialize;

use crate::{ErrorCode, HttpState, RequestContext, parse_uuid_v7};
use orbisync_identity::PasswordIdempotencyContext;

/// Request body for `POST /v1/auth/change-password`.
#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    /// Current password for verification.
    pub current_password: String,
    /// New password to set.
    pub new_password: String,
}

#[derive(Debug, serde::Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, serde::Serialize)]
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

/// `POST /v1/auth/change-password`.
///
/// Authenticated self-service password change. Validates `current_password`
/// against the stored hash, hashes `new_password` through `PasswordService`
/// policy, updates user/credential atomically and revokes other sessions.
/// Requires `Idempotency-Key` (UUIDv7) per contract; same key with same
/// actor and request body replays the stored 204, differing actor/operation/body
/// is 409 `IDEMPOTENCY_KEY_REUSED` (ADR-003). Returns 204 on success.
/// Even when `must_change_password` is true this and logout are allowed
/// (advisory enforcement, `auth-authorization.md` §2).
pub async fn change_password(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordRequest>,
) -> Response {
    let request_id_raw = context.request_id().to_owned();

    // Idempotency-Key required per OpenAPI (UuidV7 canonical). Validate strictly.
    let idempotency_key = match headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()) {
        Some(v) => match parse_uuid_v7(v) {
            Ok(_) => v.to_owned(),
            Err(_) => {
                return error_response(
                    ErrorCode::InvalidRequest,
                    "invalid Idempotency-Key".to_owned(),
                    request_id_raw,
                );
            }
        },
        None => {
            return error_response(
                ErrorCode::InvalidRequest,
                "missing Idempotency-Key".to_owned(),
                request_id_raw,
            );
        }
    };

    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id_raw).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Idempotency: fail-closed when store is not wired (V-11 / IDEM-1).
    let Some(idempotency_store) = state.idempotency_store() else {
        return error_response(
            ErrorCode::InternalError,
            "idempotency store unavailable".to_owned(),
            request_id_raw,
        );
    };

    // Build request_hash via HMAC-SHA-256 with length-prefixed canonical encoding (P2-C1).
    // Fail-closed if dedicated idempotency key is not wired.
    if state.idempotency_hmac_key().is_empty() {
        return error_response(
            ErrorCode::InternalError,
            "idempotency service unavailable".to_owned(),
            request_id_raw,
        );
    }
    let idempotency_owner: String = {
        let request_hash = crate::idempotency_request_digest(
            state.idempotency_hmac_key(),
            "changePassword", // allow-hardcoded-secret: operation name, not a credential
            Some(actor_id),
            &[
                body.current_password.as_bytes(),
                body.new_password.as_bytes(),
            ],
        );
        let now = state.clock().now();
        let command = IdempotencyClaimCommand {
            key: idempotency_key.clone(),
            actor_user_id: Some(actor_id),
            operation: "changePassword".to_owned(),
            request_hash,
            now,
        };
        match idempotency_store.claim(command).await {
            Ok(IdempotencyClaim::Acquired { owner, .. }) => owner,
            Ok(IdempotencyClaim::Reused) => {
                return error_response(
                    ErrorCode::IdempotencyKeyReused,
                    "idempotency key reused".to_owned(),
                    request_id_raw,
                );
            }
            Ok(IdempotencyClaim::InProgress { retry_after_secs }) => {
                let mut resp = error_response(
                    ErrorCode::IdempotencyInProgress,
                    "idempotency key in progress".to_owned(),
                    request_id_raw,
                );
                resp.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_str(&retry_after_secs.to_string())
                        .unwrap_or(axum::http::HeaderValue::from_static("30")),
                );
                return resp;
            }
            Ok(IdempotencyClaim::Completed(comp)) => {
                let body_bytes = comp.response.as_bytes();
                let resp = Response::builder()
                    .status(comp.status_code)
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        comp.response_content_type.clone(),
                    )
                    .body(axum::body::Body::from(body_bytes.to_owned()))
                    .unwrap_or_else(|_| StatusCode::NO_CONTENT.into_response());
                return resp;
            }
            Err(_) => {
                return error_response(
                    ErrorCode::InternalError,
                    "internal error".to_owned(),
                    request_id_raw,
                );
            }
        }
    };

    let Some(repo) = state.identity_repository() else {
        let _abandon_result = idempotency_store
            .abandon(idempotency_key.clone(), idempotency_owner.clone())
            .await;
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id_raw,
        );
    };

    let Some(admin) = state.identity_admin_service() else {
        let _abandon_result = idempotency_store
            .abandon(idempotency_key.clone(), idempotency_owner.clone())
            .await;
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id_raw,
        );
    };

    let account = match repo.find_account(actor_id).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::ResourceNotFound,
                "account not found".to_owned(),
                request_id_raw,
            );
        }
        Err(_) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id_raw,
            );
        }
    };

    let mut user = account.user;
    let mut credential = account.credential;

    let app_request_id = match orbisync_application::RequestId::new(request_id_raw.clone()) {
        Ok(v) => v,
        Err(_) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id_raw,
            );
        }
    };

    // Basic length validation early (service will also validate via policy)
    if body.current_password.is_empty() || body.current_password.chars().count() > 128 {
        let _abandon_result = idempotency_store
            .abandon(idempotency_key.clone(), idempotency_owner.clone())
            .await;
        return error_response(
            ErrorCode::InvalidRequest,
            "current_password must contain 1 through 128 characters".to_owned(),
            request_id_raw,
        );
    }

    let current = SecretString::new(body.current_password);
    let new = SecretString::new(body.new_password);

    // AUD-C2: atomic transaction – password change + idempotency in one TX.
    // P2-C2: owner-checked + abandon on failure.
    let result = admin
        .change_password_with_idempotency_and_source_ip(
            &mut user,
            &mut credential,
            current,
            new,
            PasswordIdempotencyContext::new(
                app_request_id,
                idempotency_key.clone(),
                idempotency_owner.clone(),
                context.source_ip().map(str::to_owned),
            ),
        )
        .await;

    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            let code = map_application_error(error.kind());
            let message = if code == ErrorCode::InternalError {
                "internal error".to_owned()
            } else {
                error.detail().to_owned()
            };
            error_response(code, message, request_id_raw)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod idempotency_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::{IdentityRepository, RequestId};
    use orbisync_domain::{LoginId, Timestamp};
    use orbisync_identity::{
        DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
        PasswordService, token::AccessTokenService,
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
    async fn change_password_same_key_is_idempotent() {
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
                LoginId::new("admin3").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        // Use unified store for atomic claim+completion (AUD-C2)
        let idem_port: Arc<dyn orbisync_application::IdempotencyStore> =
            store.clone() as Arc<dyn orbisync_application::IdempotencyStore>;
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
        .with_idempotency_store(idem_port)
        .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_login_service(login_service);
        // create bob
        let bob_login = LoginId::new("bob3").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, bob_temp) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        // login bob to get token
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "bob3", "password": bob_temp.expose_secret() });
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
        let bob_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let key = Uuid::now_v7().to_string();
        let current = bob_temp.expose_secret().to_owned();
        let new_pass = "NewSecurePass123!@#";
        // first change
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": current, "new_password": new_pass });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NO_CONTENT,
            "first change must be 204"
        );
        // Login with new password to get fresh token (previous token may be revoked by StorePasswordChange fake)
        let app = router(state.clone());
        let body_login = serde_json::json!({ "login_id": "bob3", "password": new_pass });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body_login.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "login with new password must succeed"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let bob_token2 = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        // second change same key same body should be idempotent 204 (replay, not error)
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": current, "new_password": new_pass });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token2}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NO_CONTENT,
            "second same key must be 204 idempotent (M4)"
        );
        // third with same key but different new_password should be 409
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": current, "new_password": "DifferentPass123!@#" });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token2}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(code, "IDEMPOTENCY_KEY_REUSED");
        let _ = bob_user;
        let _ = admin_user;
        let _ = admin_pw;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod p2_c2_change_password_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::{IdentityRepository, RequestId};
    use orbisync_domain::{LoginId, Timestamp};
    use orbisync_identity::{
        DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
        PasswordService, token::AccessTokenService,
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
    async fn change_password_mismatch_retryable_same_key() {
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
                LoginId::new("admin_mismatch").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn orbisync_application::IdempotencyStore> =
            store.clone() as Arc<dyn orbisync_application::IdempotencyStore>;
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
        .with_idempotency_store(idem_port)
        .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_login_service(login_service);
        let bob_login = LoginId::new("bob_mismatch").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, bob_temp) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "bob_mismatch", "password": bob_temp.expose_secret() });
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
        let bob_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let key = Uuid::now_v7().to_string();
        // wrong current password -> 401
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": "WrongPass123!@#", "new_password": "NewSecurePass123!@#" });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // retry same key same wrong password -> still 401, not 409
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": "WrongPass123!@#", "new_password": "NewSecurePass123!@#" });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "mismatch retry must be 401 not 409"
        );
        let _ = bob_user;
        let _ = admin_user;
        let _ = admin_pw;
    }

    #[tokio::test]
    async fn change_password_strength_violation_retryable_same_key() {
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
                LoginId::new("admin_strength").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn orbisync_application::IdempotencyStore> =
            store.clone() as Arc<dyn orbisync_application::IdempotencyStore>;
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
        .with_idempotency_store(idem_port)
        .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_login_service(login_service);
        let bob_login = LoginId::new("bob_strength").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, bob_temp) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "bob_strength", "password": bob_temp.expose_secret() });
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
        let bob_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let current = bob_temp.expose_secret().to_owned();
        let key = Uuid::now_v7().to_string();
        // weak new password -> 400
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": current, "new_password": "short" });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // retry same key same weak password -> still 400 not 409
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": current, "new_password": "short" });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "strength violation retry must be 400 not 409"
        );
        let _ = bob_user;
        let _ = admin_user;
        let _ = admin_pw;
    }

    #[tokio::test]
    async fn change_password_in_progress_has_retry_after() {
        // Verify InProgress returns 409 with retryable code and Retry-After
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
                LoginId::new("admin_ip").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn orbisync_application::IdempotencyStore> =
            store.clone() as Arc<dyn orbisync_application::IdempotencyStore>;
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
        .with_idempotency_store(idem_port.clone())
        .with_idempotency_hmac_key(b"test-idempotency-hmac-key-32b!!-P2-C1".to_vec())
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_login_service(login_service);
        let bob_login = LoginId::new("bob_ip").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, bob_temp) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "bob_ip", "password": bob_temp.expose_secret() });
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
        let bob_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        // Manually claim to simulate InProgress
        let key = Uuid::now_v7().to_string();
        let now = orbisync_domain::Clock::now(&*clock);
        // Actually claim via store directly to create InProgress
        let cmd = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: Some(bob_user.id()),
            operation: "changePassword".to_owned(),
            request_hash: crate::idempotency_request_digest(
                b"test-idempotency-hmac-key-32b!!-P2-C1",
                "changePassword",
                Some(bob_user.id()),
                &[bob_temp.expose_secret().as_bytes(), b"NewSecurePass123!@#"],
            ),
            now,
        };
        let claim = idem_port.claim(cmd).await.expect("claim");
        assert!(matches!(
            claim,
            orbisync_application::IdempotencyClaim::Acquired { .. }
        ));
        // Now call via HTTP with same key same body -> should be InProgress with Retry-After
        let app = router(state.clone());
        let body = serde_json::json!({ "current_password": bob_temp.expose_secret(), "new_password": "NewSecurePass123!@#" });
        let req = Request::builder()
            .uri("/v1/auth/change-password")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bob_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let code = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            !code.is_empty(),
            "Retry-After must be present for InProgress"
        );
        let json_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
        let err_code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(
            err_code, "IDEMPOTENCY_IN_PROGRESS",
            "InProgress must be distinct code"
        );
        let _ = admin_user;
        let _ = admin_pw;
        let _ = now;
    }
}
