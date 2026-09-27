//! User administration HTTP handler (`POST /v1/users`).

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use orbisync_application::{ApplicationErrorKind, CreateUserCommand, PageRequest, QueryCursor};
use orbisync_domain::{LoginId, UserStatus};
use serde::{Deserialize, Serialize};

use crate::{ErrorCode, HttpState, RequestContext, parse_uuid_v7};
use orbisync_application::{
    EncryptedResponse, IdempotencyClaim, IdempotencyClaimCommand, IdempotencyCompletion,
};
use orbisync_identity::PasswordIdempotencyContext;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/users`.
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    /// Login identifier 1..128 non-control.
    pub login_id: String,
    /// Display name 1..128 non-control.
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize)]
struct UserResponse {
    id: String,
    login_id: String,
    display_name: String,
    enabled: bool,
    revision: u64,
}

#[derive(Debug, Serialize)]
struct CreatedUserCredentialResponse {
    user: UserResponse,
    temporary_password: String,
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

fn wants_credential(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(axum::http::header::ACCEPT) else {
        return false;
    };
    let Ok(text) = value.to_str() else {
        return false;
    };
    text.contains("application/vnd.orbisync.user-credential+json")
}

/// `POST /v1/users`.
///
/// Validates `Authorization: Bearer <access token>` via `AccessTokenService::validate`
/// (same steps as `create_realtime_ticket`), reads server-owned roles via
/// `IdentityRepository::roles_for_user`, then delegates to
/// `IdentityAdministrationService::create_user` which enforces
/// `admin.users.create`. Returns 201 with `User` by default, or with
/// `CreatedUserCredential` when `Accept: application/vnd.orbisync.user-credential+json`.
pub async fn create_user(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();

    let Some(repo) = state.identity_repository() else {
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id,
        );
    };
    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // 2. Load server-owned roles – never trust client-supplied roles.
    let roles = match repo.roles_for_user(actor_id).await {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    // 3. Admin service must be wired.
    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };

    // 4. Build domain command.
    let login_id = match LoginId::new(body.login_id) {
        Ok(value) => value,
        Err(error) => {
            return error_response(ErrorCode::InvalidRequest, error.to_string(), request_id);
        }
    };
    // ADR-026: the server derives login identifiers for guest, name-only and
    // external subjects from their own user id. Letting an administrator create
    // an account under one of those prefixes would let it collide with, or pass
    // for, a generated subject -- and `LoginId::new` accepts any non-control
    // text, so nothing else rejects it.
    if login_id.is_reserved() {
        return error_response(
            ErrorCode::InvalidRequest,
            format!(
                "login_id must not begin with a server-reserved prefix ({})",
                orbisync_domain::RESERVED_LOGIN_ID_PREFIXES.join(", ")
            ),
            request_id,
        );
    }
    let display_name = body.display_name;
    // Validate display_name early to return 400 instead of 500; service also validates.
    if display_name.is_empty()
        || display_name.chars().count() > 128
        || display_name.chars().any(char::is_control)
    {
        return error_response(
            ErrorCode::InvalidRequest,
            "display_name must contain 1 through 128 non-control characters".to_owned(),
            request_id,
        );
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
    let command = CreateUserCommand {
        login_id,
        display_name,
        actor_id,
        request_id: app_request_id,
    };

    // 5. Delegate to service (RBAC inside).
    match admin
        .create_user_with_source_ip(command, &roles, context.source_ip().map(str::to_owned))
        .await
    {
        Ok((user, temporary_password)) => {
            let user_response = UserResponse {
                id: user.id().to_string(),
                login_id: user.login_id().as_str().to_owned(),
                display_name: user.display_name().to_owned(),
                enabled: user.status() == UserStatus::Active,
                revision: user.revision().as_u64(),
            };
            if wants_credential(&headers) {
                let body = CreatedUserCredentialResponse {
                    user: user_response,
                    temporary_password: temporary_password.expose_secret().to_owned(),
                };
                let mut response = (StatusCode::CREATED, Json(body)).into_response();
                // Explicit media type per OpenAPI – caller explicitly asked for credential.
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static(
                        "application/vnd.orbisync.user-credential+json",
                    ),
                );
                response
            } else {
                (StatusCode::CREATED, Json(user_response)).into_response()
            }
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
/// Query params for `GET /v1/users`.
#[derive(Debug, Deserialize)]
pub struct UserListQuery {
    /// Opaque pagination cursor.
    pub cursor: Option<String>,
    /// Page size (1..200, default 50).
    pub limit: Option<u16>,
}

#[derive(Debug, Serialize)]
struct UserPageResponse {
    items: Vec<UserResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

fn map_query_error(err: orbisync_application::IdentityPortError) -> ErrorCode {
    match err {
        orbisync_application::IdentityPortError::InvalidRequest => ErrorCode::InvalidRequest,
        orbisync_application::IdentityPortError::DataCorruption => ErrorCode::InternalError,
        orbisync_application::IdentityPortError::Unavailable => ErrorCode::ServiceUnavailable,
        orbisync_application::IdentityPortError::NotFound => ErrorCode::ResourceNotFound,
        orbisync_application::IdentityPortError::Conflict => ErrorCode::ResourceConflict,
    }
}

/// `GET /v1/users`
pub async fn list_users(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Query(q): Query<UserListQuery>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.users.read").await {
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
    let after = q.cursor.map(QueryCursor);
    let page_req = PageRequest {
        limit: u32::from(limit),
        after,
    };
    let page = match query.users(page_req).await {
        Ok(p) => p,
        Err(e) => return error_response(map_query_error(e), "query failed".to_owned(), request_id),
    };
    let items = page
        .items
        .into_iter()
        .map(|u| UserResponse {
            id: u.id.to_string(),
            login_id: u.login_id.as_str().to_owned(),
            display_name: u.display_name.to_owned(),
            enabled: u.status == UserStatus::Active,
            revision: u.revision,
        })
        .collect::<Vec<_>>();
    let next_cursor = page.next.map(|c| c.0);
    let body = UserPageResponse { items, next_cursor };
    (StatusCode::OK, Json(body)).into_response()
}

/// `GET /v1/users/{user_id}`
pub async fn get_user(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(user_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.users.read").await {
        return resp;
    }
    let Some(query) = state.identity_query() else {
        return error_response(
            ErrorCode::InternalError,
            "identity query unavailable".to_owned(),
            request_id,
        );
    };
    let user_id = match parse_uuid_v7(&user_id_raw) {
        Ok(u) => match orbisync_domain::UserId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid user_id".to_owned(), request_id),
    };
    let user = match query.user(user_id).await {
        Ok(v) => v,
        Err(e) => return error_response(map_query_error(e), "query failed".to_owned(), request_id),
    };
    match user {
        Some(u) => {
            let body = UserResponse {
                id: u.id.to_string(),
                login_id: u.login_id.as_str().to_owned(),
                display_name: u.display_name.to_owned(),
                enabled: u.status == UserStatus::Active,
                revision: u.revision,
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        None => error_response(
            ErrorCode::ResourceNotFound,
            "user not found".to_owned(),
            request_id,
        ),
    }
}

#[derive(Debug, Serialize)]
struct TemporaryPasswordResponse {
    temporary_password: String,
    must_change_password: bool,
}

/// `POST /v1/users/{user_id}/reset-password` (PW-1)
///
/// 管理者によるパスワードリセット。サーバ側で暗号論的乱数 (20 bytes -> 27 chars base64url) から
/// 一時パスワードを生成し、`Credential::replace_password` で置き換え、`must_change_password` を true にする。
/// 失敗回数とロック状態 (`failed_login_count` / `locked_until`) はリセット時にクリアするのが妥当と判断した
/// 理由は `crates/orbisync-identity/src/admin.rs` の `reset_password` コメントに記載。
/// 既存セッションの扱い: 攻撃者に乗っ取られた後にリセットしても追い出せないことを防ぐため、
/// 対象ユーザの全 `auth_sessions` (および紐づく `refresh_tokens`) を同一トランザクションで失効させる (ADR-002)。
/// 自分自身に対する実行は**許す** (admin.rs の判断コメント参照)。
/// 認証は `crate::authenticate` を経由し、認可は `admin.users.credentials.reset` で行う (CR-13)。
/// `Idempotency-Key` は既存基盤 `crates/orbisync-storage-postgres/src/idempotency.rs` に揃えて扱い、
/// 同じキーで2回呼んでも新しい一時パスワードを発行しない。監査は `password.reset` で成功/失敗を分け、
/// 一時パスワードの値自体は監査行・ログ・出力に絶対に出さない。
pub async fn reset_user_password(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    Path(user_id_raw): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = context.request_id().to_owned();

    // Idempotency-Key は契約上必須 (UuidV7 canonical)
    let idempotency_key = match headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()) {
        Some(v) => match parse_uuid_v7(v) {
            Ok(_) => v.to_owned(),
            Err(_) => {
                return error_response(
                    ErrorCode::InvalidRequest,
                    "invalid Idempotency-Key".to_owned(),
                    request_id,
                );
            }
        },
        None => {
            return error_response(
                ErrorCode::InvalidRequest,
                "missing Idempotency-Key".to_owned(),
                request_id,
            );
        }
    };

    let target_user_id = match parse_uuid_v7(&user_id_raw) {
        Ok(u) => match orbisync_domain::UserId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid user_id".to_owned(), request_id),
    };

    // 認証・認可は crate::authenticate を使う (CR-13)。権限は admin.users.credentials.reset に揃える。
    // 既存の管理者権限名に揃えること – bootstrap では "admin.users.credentials.reset" が付与される。
    // 自分自身に対する実行を許すかどうか: 許す (理由は admin.rs の reset_password コメント参照)。
    let actor_id = match crate::authorize(
        &state,
        &headers,
        &request_id,
        "admin.users.credentials.reset",
    )
    .await
    {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // 冪等性: 既存の作法に揃えて IdempotencyStore を使う (ADR-003 / ADR-017)。
    // 同じ Idempotency-Key で2回呼んだとき、2回目が新しい一時パスワードを発行しないこと。
    // request_hash は対象 user_id から決定的に算出する。
    // V-11 と同様に未配線は 500 で fail-closed（素通りさせない）。
    // P2-C2: claim は owner token と lease を持つ。失敗時は abandon して再試行を妨げない。
    let Some(idempotency_store) = state.idempotency_store() else {
        return error_response(
            ErrorCode::InternalError,
            "idempotency store unavailable".to_owned(),
            request_id,
        );
    };
    let idempotency_owner: String = {
        let store = &idempotency_store;
        if state.idempotency_hmac_key().is_empty() {
            return error_response(
                ErrorCode::InternalError,
                "idempotency service unavailable".to_owned(),
                request_id,
            );
        }
        let request_hash = crate::idempotency_request_digest(
            state.idempotency_hmac_key(),
            "resetUserPassword", // allow-hardcoded-secret: operation name, not a credential
            Some(actor_id),
            &[target_user_id.to_string().as_bytes()],
        );
        let now = state.clock().now();
        let command = IdempotencyClaimCommand {
            key: idempotency_key.clone(),
            actor_user_id: Some(actor_id),
            operation: "resetUserPassword".to_owned(), // allow-hardcoded-secret: operation name, not a credential
            request_hash,
            now,
        };
        match store.claim(command).await {
            Ok(IdempotencyClaim::Acquired { owner, .. }) => owner,
            Ok(IdempotencyClaim::Reused) => {
                return error_response(
                    ErrorCode::IdempotencyKeyReused,
                    "idempotency key reused".to_owned(),
                    request_id,
                );
            }
            Ok(IdempotencyClaim::InProgress { retry_after_secs }) => {
                let mut resp = error_response(
                    ErrorCode::IdempotencyInProgress,
                    "idempotency key in progress".to_owned(),
                    request_id,
                );
                resp.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_str(&retry_after_secs.to_string())
                        .unwrap_or(axum::http::HeaderValue::from_static("30")),
                );
                return resp;
            }
            Ok(IdempotencyClaim::Completed(comp)) => {
                // 監査やログに一時パスワードを出さず、保存済みレスポンスをそのまま返す。
                let body_bytes = comp.response.as_bytes();
                let resp = Response::builder()
                    .status(comp.status_code)
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        comp.response_content_type.clone(),
                    )
                    .body(axum::body::Body::from(body_bytes.to_owned()))
                    .unwrap_or_else(|_| {
                        (StatusCode::OK, Json(serde_json::json!({}))).into_response()
                    });
                // request_id ヘッダは middleware が付与済みだが、明示的に上書きしない。
                return resp;
            }
            Err(_) => {
                return error_response(
                    ErrorCode::InternalError,
                    "internal error".to_owned(),
                    request_id,
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
            request_id,
        );
    };
    let Some(admin) = state.identity_admin_service() else {
        let _abandon_result = idempotency_store
            .abandon(idempotency_key.clone(), idempotency_owner.clone())
            .await;
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };

    // 対象ユーザの取得 (404 ならそのまま返す。P2-C2 で abandon して再試行を妨げない)
    let account = match repo.find_account(target_user_id).await {
        Ok(Some(acc)) => acc,
        Ok(None) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::ResourceNotFound,
                "user not found".to_owned(),
                request_id,
            );
        }
        Err(_) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    let mut user = account.user;
    let mut credential = account.credential;

    let app_request_id = match orbisync_application::RequestId::new(request_id.clone()) {
        Ok(v) => v,
        Err(_) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };

    // セッション失効の検証が必要なため、失効処理は admin.reset_password 内の
    // ResetPassword トランザクションで auth_sessions を 'revoked' に更新する。
    let actor_roles = match repo.roles_for_user(actor_id).await {
        Ok(r) => r,
        Err(_) => {
            let _abandon_result = idempotency_store
                .abandon(idempotency_key.clone(), idempotency_owner.clone())
                .await;
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    // AUD-C2: atomic transaction – password mutation + idempotency completion in one TX.
    // AUD-C1: completion body must NOT contain temporary_password (no secret persistence).
    // P2-C2: owner-checked completion, failure triggers abandon so same key retry is not blocked.
    let temp = match admin
        .reset_password_with_idempotency_and_source_ip(
            &mut user,
            &mut credential,
            actor_id,
            &actor_roles,
            PasswordIdempotencyContext::new(
                app_request_id,
                idempotency_key.clone(),
                idempotency_owner.clone(),
                context.source_ip().map(str::to_owned),
            ),
        )
        .await
    {
        Ok(v) => v,
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
            return error_response(code, message, request_id);
        }
    };

    // 監査イベントは admin.reset_password 内で password.reset / success として記録される。
    // 一時パスワードの値を監査行・ログに出さないこと – action/details には含めない。

    let body = TemporaryPasswordResponse {
        temporary_password: temp.expose_secret().to_owned(),
        must_change_password: true,
    };

    // 202 + TemporaryPassword。must_change_password は true (const)。
    // 一時パスワードの値はレスポンスボディ以外に出さない。
    // Idempotency completion is already done atomically inside the admin call
    // with a body that does NOT contain the secret (AUD-C1).
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// `GET /v1/users/{user_id}/roles` (V-05)
pub async fn get_user_roles(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(user_id_raw): Path<String>,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Err(resp) = crate::authorize(&state, &headers, &request_id, "admin.users.read").await {
        return resp;
    }
    let Some(repo) = state.identity_repository() else {
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id,
        );
    };
    let user_id = match parse_uuid_v7(&user_id_raw) {
        Ok(u) => match orbisync_domain::UserId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid user_id".to_owned(), request_id),
    };
    // Verify user exists via query port (optional)
    let Some(query) = state.identity_query() else {
        return error_response(
            ErrorCode::InternalError,
            "identity query unavailable".to_owned(),
            request_id,
        );
    };
    let exists = match query.user(user_id).await {
        Ok(v) => v.is_some(),
        Err(e) => return error_response(map_query_error(e), "query failed".to_owned(), request_id),
    };
    if !exists {
        return error_response(
            ErrorCode::ResourceNotFound,
            "user not found".to_owned(),
            request_id,
        );
    }
    let roles = match repo.roles_for_user(user_id).await {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let role_ids = roles
        .into_iter()
        .map(|r| r.id().to_string())
        .collect::<Vec<_>>();
    let body = serde_json::json!({ "user_id": user_id.to_string(), "role_ids": role_ids });
    (StatusCode::OK, Json(body)).into_response()
}

/// Request body for `PATCH /v1/users/{user_id}` (`application/merge-patch+json`).
///
/// Both fields are optional; a field absent from the JSON object means "leave
/// unchanged" per merge-patch semantics (the schema does not allow `null`).
#[derive(Debug, Deserialize, Default)]
pub struct UpdateUserRequest {
    /// New display name, when present.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New enabled state, when present.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PATCH /v1/users/{user_id}`.
///
/// `display_name` requires `admin.users.update`; `enabled` additionally
/// requires `admin.users.status` (checked inside
/// `IdentityAdministrationService::update_user`). `If-Match` is a required
/// optimistic-concurrency precondition, matching the `DeleteRole` precedent
/// (`crate::parse_if_match`); a mismatch is reported as `409
/// RESOURCE_CONFLICT` for the same reason `DeleteRole` chose that code over
/// `412 REVISION_MISMATCH` (see its doc comment).
pub async fn update_user(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(user_id_raw): Path<String>,
    Json(body): Json<UpdateUserRequest>,
) -> Response {
    let request_id = context.request_id().to_owned();
    let if_match_raw = match headers.get("if-match").and_then(|v| v.to_str().ok()) {
        Some(v) => v.to_owned(),
        None => {
            return error_response(
                ErrorCode::InvalidRequest,
                "If-Match header is required".to_owned(),
                request_id,
            );
        }
    };
    let expected_revision = match crate::parse_if_match(&if_match_raw) {
        Ok(v) => v,
        Err(code) => {
            return error_response(code, "invalid If-Match header".to_owned(), request_id);
        }
    };
    let target_user_id = match parse_uuid_v7(&user_id_raw) {
        Ok(u) => match orbisync_domain::UserId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid user_id".to_owned(), request_id),
    };
    if let Some(name) = &body.display_name
        && (name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control))
    {
        return error_response(
            ErrorCode::InvalidRequest,
            "display_name must contain 1 through 128 non-control characters".to_owned(),
            request_id,
        );
    }
    let Some(repo) = state.identity_repository() else {
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id,
        );
    };
    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
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
    let account = match repo.find_account(target_user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return error_response(
                ErrorCode::ResourceNotFound,
                "user not found".to_owned(),
                request_id,
            );
        }
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let mut user = account.user;
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
        .update_user(
            &mut user,
            body.display_name,
            body.enabled,
            expected_revision,
            actor_id,
            app_request_id,
            &actor_roles,
        )
        .await
    {
        Ok(()) => {
            let response = UserResponse {
                id: user.id().to_string(),
                login_id: user.login_id().as_str().to_owned(),
                display_name: user.display_name().to_owned(),
                enabled: user.status() == UserStatus::Active,
                revision: user.revision().as_u64(),
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

/// Returns `Some(error response)` when the header is missing or malformed.
fn require_idempotency_key(headers: &HeaderMap, request_id: &str) -> Option<Response> {
    match headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()) {
        Some(v) => match parse_uuid_v7(v) {
            Ok(_) => None,
            Err(_) => Some(error_response(
                ErrorCode::InvalidRequest,
                "invalid Idempotency-Key".to_owned(),
                request_id.to_owned(),
            )),
        },
        None => Some(error_response(
            ErrorCode::InvalidRequest,
            "missing Idempotency-Key".to_owned(),
            request_id.to_owned(),
        )),
    }
}

/// Shared implementation for `POST /v1/users/{user_id}/disable` and `/enable`.
///
/// `Idempotency-Key` is required by the OpenAPI contract but is checked only
/// for well-formedness (UUIDv7), not routed through `IdempotencyStore`: the
/// underlying `set_user_enabled` transition is already state-idempotent
/// (repeated calls with the same target state return the same representation
/// without a second revision bump), so a replay-cache is unnecessary here –
/// unlike `resetUserPassword`, which mints a new secret every time and must
/// use the store to avoid handing out two different temporary passwords for
/// one client retry.
async fn set_user_enabled_handler(
    state: HttpState,
    context: RequestContext,
    headers: HeaderMap,
    user_id_raw: String,
    enabled: bool,
) -> Response {
    let request_id = context.request_id().to_owned();
    if let Some(resp) = require_idempotency_key(&headers, &request_id) {
        return resp;
    }
    let target_user_id = match parse_uuid_v7(&user_id_raw) {
        Ok(u) => match orbisync_domain::UserId::new(u) {
            Ok(id) => id,
            Err(e) => return error_response(ErrorCode::InvalidRequest, e.to_string(), request_id),
        },
        Err(e) => return error_response(e, "invalid user_id".to_owned(), request_id),
    };
    let Some(repo) = state.identity_repository() else {
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id,
        );
    };
    let (actor_id, _, _) = match crate::authenticate(&state, &headers, &request_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
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
    let account = match repo.find_account(target_user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return error_response(
                ErrorCode::ResourceNotFound,
                "user not found".to_owned(),
                request_id,
            );
        }
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let mut user = account.user;
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
        .set_user_enabled(&mut user, enabled, actor_id, app_request_id, &actor_roles)
        .await
    {
        Ok(()) => {
            let response = UserResponse {
                id: user.id().to_string(),
                login_id: user.login_id().as_str().to_owned(),
                display_name: user.display_name().to_owned(),
                enabled: user.status() == UserStatus::Active,
                revision: user.revision().as_u64(),
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

/// `POST /v1/users/{user_id}/disable`.
pub async fn disable_user(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(user_id_raw): Path<String>,
) -> Response {
    set_user_enabled_handler(state, context, headers, user_id_raw, false).await
}

/// `POST /v1/users/{user_id}/enable`.
pub async fn enable_user(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    Path(user_id_raw): Path<String>,
) -> Response {
    set_user_enabled_handler(state, context, headers, user_id_raw, true).await
}

#[derive(Debug, Serialize, Clone)]
struct UserImportRowResultResponse {
    row: usize,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<UserResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporary_password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn failed_row(
    row: usize,
    error_code: &'static str,
    message: impl Into<String>,
) -> UserImportRowResultResponse {
    UserImportRowResultResponse {
        row,
        status: "failed",
        user: None,
        temporary_password: None,
        error_code: Some(error_code),
        message: Some(message.into()),
    }
}

#[derive(Debug, Serialize)]
struct UserImportResultResponse {
    total: usize,
    succeeded: usize,
    failed: usize,
    results: Vec<UserImportRowResultResponse>,
}

/// `POST /v1/users/import` (ADR-016).
///
/// UTF-8 `text/csv`, `login_id,display_name` header, max 1,000 rows / 1 MiB
/// (`state.decode_user_import_body`, already shared with the CSV parser
/// unit tests). Each row is created through the same one-transaction-per-row
/// path as `POST /v1/users` (`create_user_with_source_ip`), so a failure on
/// one row cannot roll back rows already committed – partial success, per
/// `auth-authorization.md` §2 / ADR-016. A duplicate `login_id`, whether
/// against an existing account or another row earlier in the same file, is
/// reported as `RESOURCE_CONFLICT` on that row without touching the
/// database, both to avoid relying on parsing a driver-specific unique-
/// violation error and to keep concurrent imports racing the same login_id
/// see the well-known `RESOURCE_CONFLICT` code rather than an opaque 500 in
/// the rare case a duplicate slips past this pre-check.
///
/// `Idempotency-Key` guards the whole batch: on replay the stored response
/// omits every row's `temporary_password` (AUD-C1). Because rows are
/// independently committed, a failure to persist the idempotency completion
/// record after rows succeeded is logged but does not change the response —
/// unlike the single-mutation endpoints, there is nothing left to roll back.
pub async fn import_users(
    State(state): State<HttpState>,
    Extension(context): Extension<RequestContext>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let request_id = context.request_id().to_owned();
    let idempotency_key = match headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()) {
        Some(v) => match parse_uuid_v7(v) {
            Ok(_) => v.to_owned(),
            Err(_) => {
                return error_response(
                    ErrorCode::InvalidRequest,
                    "invalid Idempotency-Key".to_owned(),
                    request_id,
                );
            }
        },
        None => {
            return error_response(
                ErrorCode::InvalidRequest,
                "missing Idempotency-Key".to_owned(),
                request_id,
            );
        }
    };
    let actor_id = match crate::authorize(&state, &headers, &request_id, "admin.users.import").await
    {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let rows = match state.decode_user_import_body(&body) {
        Ok(v) => v,
        Err(code) => return error_response(code, "invalid CSV body".to_owned(), request_id),
    };
    let Some(repo) = state.identity_repository() else {
        return error_response(
            ErrorCode::InternalError,
            "identity service unavailable".to_owned(),
            request_id,
        );
    };
    let Some(admin) = state.identity_admin_service() else {
        return error_response(
            ErrorCode::InternalError,
            "identity administration unavailable".to_owned(),
            request_id,
        );
    };
    let Some(idempotency_store) = state.idempotency_store() else {
        return error_response(
            ErrorCode::InternalError,
            "idempotency store unavailable".to_owned(),
            request_id,
        );
    };
    if state.idempotency_hmac_key().is_empty() {
        return error_response(
            ErrorCode::InternalError,
            "idempotency service unavailable".to_owned(),
            request_id,
        );
    }
    let request_hash = crate::idempotency_request_digest(
        state.idempotency_hmac_key(),
        "importUsers", // allow-hardcoded-secret: operation name, not a credential
        Some(actor_id),
        &[body.as_ref()],
    );
    let now = state.clock().now();
    let claim_command = IdempotencyClaimCommand {
        key: idempotency_key.clone(),
        actor_user_id: Some(actor_id),
        operation: "importUsers".to_owned(), // allow-hardcoded-secret: operation name, not a credential
        request_hash,
        now,
    };
    let owner = match idempotency_store.claim(claim_command).await {
        Ok(IdempotencyClaim::Acquired { owner, .. }) => owner,
        Ok(IdempotencyClaim::Reused) => {
            return error_response(
                ErrorCode::IdempotencyKeyReused,
                "idempotency key reused".to_owned(),
                request_id,
            );
        }
        Ok(IdempotencyClaim::InProgress { retry_after_secs }) => {
            let mut resp = error_response(
                ErrorCode::IdempotencyInProgress,
                "idempotency key in progress".to_owned(),
                request_id,
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
            return Response::builder()
                .status(comp.status_code)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    comp.response_content_type.clone(),
                )
                .body(axum::body::Body::from(body_bytes.to_owned()))
                .unwrap_or_else(|_| (StatusCode::OK, Json(serde_json::json!({}))).into_response());
        }
        Err(_) => {
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let actor_roles = match repo.roles_for_user(actor_id).await {
        Ok(v) => v,
        Err(_) => {
            let _abandon_result = idempotency_store.abandon(idempotency_key, owner).await;
            return error_response(
                ErrorCode::InternalError,
                "internal error".to_owned(),
                request_id,
            );
        }
    };
    let source_ip = context.source_ip().map(str::to_owned);
    let mut seen_login_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results = Vec::with_capacity(rows.len());
    let mut succeeded = 0_usize;
    let mut failed = 0_usize;
    for row in &rows {
        if !seen_login_ids.insert(row.login_id.clone()) {
            failed += 1;
            results.push(failed_row(
                row.row,
                "RESOURCE_CONFLICT",
                "duplicate login_id in the same import",
            ));
            continue;
        }
        let login_id = match LoginId::new(row.login_id.clone()) {
            Ok(v) => v,
            Err(e) => {
                failed += 1;
                results.push(failed_row(row.row, "INVALID_REQUEST", e.to_string()));
                continue;
            }
        };
        if row.display_name.is_empty()
            || row.display_name.chars().count() > 128
            || row.display_name.chars().any(char::is_control)
        {
            failed += 1;
            results.push(failed_row(
                row.row,
                "INVALID_REQUEST",
                "display_name must contain 1 through 128 non-control characters",
            ));
            continue;
        }
        match repo.find_login(&login_id).await {
            Ok(Some(_)) => {
                failed += 1;
                results.push(failed_row(
                    row.row,
                    "RESOURCE_CONFLICT",
                    "login_id already exists",
                ));
                continue;
            }
            Ok(None) => {}
            Err(_) => {
                failed += 1;
                results.push(failed_row(
                    row.row,
                    "INTERNAL_ERROR",
                    "login_id lookup failed",
                ));
                continue;
            }
        }
        let app_request_id =
            match orbisync_application::RequestId::new(format!("req_{}", uuid::Uuid::now_v7())) {
                Ok(v) => v,
                Err(_) => {
                    failed += 1;
                    results.push(failed_row(row.row, "INTERNAL_ERROR", "internal error"));
                    continue;
                }
            };
        let command = CreateUserCommand {
            login_id,
            display_name: row.display_name.clone(),
            actor_id,
            request_id: app_request_id,
        };
        match admin
            .create_user_with_source_ip(command, &actor_roles, source_ip.clone())
            .await
        {
            Ok((user, temporary_password)) => {
                succeeded += 1;
                results.push(UserImportRowResultResponse {
                    row: row.row,
                    status: "created",
                    user: Some(UserResponse {
                        id: user.id().to_string(),
                        login_id: user.login_id().as_str().to_owned(),
                        display_name: user.display_name().to_owned(),
                        enabled: user.status() == UserStatus::Active,
                        revision: user.revision().as_u64(),
                    }),
                    temporary_password: Some(temporary_password.expose_secret().to_owned()),
                    error_code: None,
                    message: None,
                });
            }
            Err(error) => {
                failed += 1;
                let code = map_application_error(error.kind());
                results.push(failed_row(
                    row.row,
                    code.as_str(),
                    error.detail().to_owned(),
                ));
            }
        }
    }

    let response_body = UserImportResultResponse {
        total: rows.len(),
        succeeded,
        failed,
        results,
    };
    // AUD-C1: the replayable completion body must never contain a secret.
    let redacted_results: Vec<UserImportRowResultResponse> = response_body
        .results
        .iter()
        .cloned()
        .map(|mut r| {
            r.temporary_password = None;
            r
        })
        .collect();
    let redacted_body = UserImportResultResponse {
        total: response_body.total,
        succeeded: response_body.succeeded,
        failed: response_body.failed,
        results: redacted_results,
    };
    let redacted_bytes = serde_json::to_vec(&redacted_body).unwrap_or_else(|_| b"{}".to_vec());
    let completion = IdempotencyCompletion {
        status_code: 202,
        response_content_type: "application/json".to_owned(),
        response: EncryptedResponse::new(redacted_bytes),
    };
    if let Err(err) = idempotency_store
        .complete(idempotency_key, owner, completion)
        .await
    {
        tracing::warn!(
            error = %err,
            request_id = %request_id,
            "failed to persist import idempotency completion (rows already committed; response is unaffected)"
        );
    }
    (StatusCode::ACCEPTED, Json(response_body)).into_response()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod idempotency_fail_closed_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::{IdempotencyStore, IdentityRepository, RequestId};
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

    #[tokio::test]
    async fn reset_without_idempotency_store_is_500() {
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
        // Bootstrap admin
        let (admin_user, admin_pw) = admin
            .bootstrap_administrator(
                LoginId::new("admin").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        // Build state WITHOUT idempotency_store (fail-closed expected)
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
        .with_identity_repository(repo)
        .with_password_service(Arc::clone(&passwords))
        .with_identity_admin_service(Arc::clone(&admin))
        .with_refresh_creation_store(Arc::new(NopRefreshCreationStore)
            as Arc<dyn orbisync_application::RefreshTokenCreationStore>)
        .with_login_service(login_service);
        // Login to get token
        let app = router(state.clone());
        let body = serde_json::json!({ "login_id": "admin", "password": admin_pw.expose_secret() });
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
        let admin_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();

        // Create bob via service
        let bob_login = LoginId::new("bob").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let repo2: Arc<dyn IdentityRepository> = store.clone() as Arc<dyn IdentityRepository>;
        let roles = repo2.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, _bob_temp) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let bob_id = bob_user.id().to_string();
        let key = Uuid::now_v7().to_string();
        // Call reset without idempotency_store – must be 500 fail-closed
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "without idempotency_store must be 500 fail-closed (IDEM-1 M2)"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(code, "INTERNAL_ERROR");
    }

    #[tokio::test]
    async fn reset_same_key_different_user_is_idempotency_key_reused() {
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
                LoginId::new("admin2").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        // Use unified store for idempotency so that atomic claim+completion share same map (AUD-C2)
        let idem_port: Arc<dyn IdempotencyStore> = store.clone() as Arc<dyn IdempotencyStore>;
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
        // login
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admin2", "password": admin_pw.expose_secret() });
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
        let admin_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        // create bob and carol
        let bob_login = LoginId::new("bob2").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, _) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let carol_login = LoginId::new("carol2").expect("login");
        let carol_cmd = orbisync_application::CreateUserCommand {
            login_id: carol_login,
            display_name: "Carol".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let (carol_user, _) = admin
            .create_user(carol_cmd, &roles)
            .await
            .expect("create carol");
        let bob_id = bob_user.id().to_string();
        let carol_id = carol_user.id().to_string();
        let key = Uuid::now_v7().to_string();
        // first reset bob with key
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        // second request same key but different user (carol) => 409 IDEMPOTENCY_KEY_REUSED
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{carol_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "same key different user must be 409"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_eq!(
            code, "IDEMPOTENCY_KEY_REUSED",
            "must be IDEMPOTENCY_KEY_REUSED not RESOURCE_CONFLICT (M3)"
        );
    }

    #[tokio::test]
    async fn reset_atomic_failure_rolls_back_and_returns_500() {
        // AUD-C2: force idempotency completion to fail, verify password/session/audit rolled back and 5xx.
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
                LoginId::new("admin_atomic").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn IdempotencyStore> = store.clone() as Arc<dyn IdempotencyStore>;
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
        // login admin
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admin_atomic", "password": admin_pw.expose_secret() });
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
        let admin_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        // create bob
        let bob_login = LoginId::new("bob_atomic").expect("login");
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
        let bob_id = bob_user.id().to_string();
        // Capture password hash before reset
        let before = repo
            .find_account(bob_user.id())
            .await
            .expect("find")
            .unwrap()
            .credential
            .password_hash()
            .expose_phc()
            .to_owned();
        // Force next atomic completion to fail
        store.fail_next_atomic_completion();
        let key = Uuid::now_v7().to_string();
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "completion failure must be 5xx not 202 (AUD-C2 defect 1)"
        );
        // Verify password not changed (rollback)
        let after = repo
            .find_account(bob_user.id())
            .await
            .expect("find")
            .unwrap()
            .credential
            .password_hash()
            .expose_phc()
            .to_owned();
        assert_eq!(
            before, after,
            "password must be rolled back when idempotency completion fails"
        );
        // Verify old temp still works for login (since password not changed)
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "bob_atomic", "password": bob_temp.expose_secret() });
        let req = Request::builder()
            .uri("/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "old password must still work after rollback"
        );
    }

    #[tokio::test]
    async fn reset_replay_does_not_contain_password() {
        // AUD-C1: first 202 contains temporary_password, replay 202 does not, and DB body has no secret.
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
                LoginId::new("admin_replay").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn IdempotencyStore> = store.clone() as Arc<dyn IdempotencyStore>;
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
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admin_replay", "password": admin_pw.expose_secret() });
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
        let admin_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let bob_login = LoginId::new("bob_replay").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, _bob_temp) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let bob_id = bob_user.id().to_string();
        let key = Uuid::now_v7().to_string();
        // first reset
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let first_pw = json
            .get("temporary_password")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        assert_eq!(first_pw.chars().count(), 27);
        // second replay with same key
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json2: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json2.get("must_change_password").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            json2.get("temporary_password").is_none(),
            "replay must not contain temporary_password (AUD-C1)"
        );
        // DB body must not contain secret
        let map = store.idempotency_results();
        let (_cmd, comp, _, _) = map.get(&key).expect("idempotency record must exist");
        let body_bytes = comp.as_ref().expect("completed").response.as_bytes();
        let body_str = String::from_utf8_lossy(body_bytes);
        assert!(
            !body_str.contains(&first_pw),
            "persisted body must not contain temporary_password"
        );
        assert!(
            !body_str.contains("temporary_password"),
            "persisted body must not contain key"
        );
    }

    #[tokio::test]
    async fn complete_zero_rows_is_not_success() {
        // AUD-C2: 0 rows affected must not be treated as success.
        let store = Arc::new(orbisync_testkit::FakeIdempotencyStore::default());
        let clock = Timestamp::from_unix_millis(1_700_000_000_000).expect("valid");
        let key = Uuid::now_v7().to_string();
        let cmd = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: None,
            operation: "testOp".to_owned(),
            request_hash: [0u8; 32],
            now: clock,
        };
        let claim = store.claim(cmd).await.expect("claim");
        let owner = match claim {
            orbisync_application::IdempotencyClaim::Acquired { owner, .. } => owner,
            other => panic!("expected Acquired, got {other:?}"),
        };
        let comp = orbisync_application::IdempotencyCompletion {
            status_code: 202,
            response_content_type: "application/json".to_owned(),
            response: orbisync_application::EncryptedResponse::new(b"{}".to_vec()),
        };
        // first complete succeeds
        store
            .complete(key.clone(), owner.clone(), comp.clone())
            .await
            .expect("first complete ok");
        // second complete on same key should fail (0 rows -> Unavailable)
        let err = store.complete(key, owner, comp).await.unwrap_err();
        assert_eq!(
            err,
            orbisync_application::IdentityPortError::Unavailable,
            "second complete must be Unavailable (0 rows)"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod p2_c2_regression_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use orbisync_application::{IdempotencyClaim, IdempotencyStore, IdentityRepository, RequestId};
    use orbisync_domain::{LoginId, Timestamp};
    use orbisync_identity::{
        DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
        PasswordService, token::AccessTokenService,
    };
    use orbisync_testkit::{FakeIdempotencyStore, FakeIdentityStore, FixedClock};
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
    async fn reset_404_retryable_same_key() {
        // P2-C2: 404直後に同じkeyで再試行できる
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
                LoginId::new("admin404").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn IdempotencyStore> = store.clone() as Arc<dyn IdempotencyStore>;
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
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admin404", "password": admin_pw.expose_secret() });
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
        let admin_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let _ = admin_user;
        let fake_user_id = Uuid::now_v7().to_string();
        let key = Uuid::now_v7().to_string();
        // first reset non-existent user -> 404
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{fake_user_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // second retry same key same non-existent user -> still 404, not 409
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{fake_user_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "404 retry with same key must be 404 not 409"
        );
    }

    #[tokio::test]
    async fn reset_db_temp_failure_retryable_same_key() {
        // P2-C2: DB一時障害直後に同じkeyで再試行できる
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
                LoginId::new("admindb").expect("login"),
                "Admin".to_owned(),
                RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
            )
            .await
            .expect("bootstrap");
        let idem_port: Arc<dyn IdempotencyStore> = store.clone() as Arc<dyn IdempotencyStore>;
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
        let app = router(state.clone());
        let body =
            serde_json::json!({ "login_id": "admindb", "password": admin_pw.expose_secret() });
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
        let admin_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let bob_login = LoginId::new("bobdb").expect("login");
        let bob_cmd = orbisync_application::CreateUserCommand {
            login_id: bob_login,
            display_name: "Bob".to_owned(),
            actor_id: admin_user.id(),
            request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        };
        let roles = repo.roles_for_user(admin_user.id()).await.expect("roles");
        let (bob_user, _) = admin
            .create_user(bob_cmd, &roles)
            .await
            .expect("create bob");
        let bob_id = bob_user.id().to_string();
        let key = Uuid::now_v7().to_string();
        store.fail_next_atomic_completion();
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "first attempt with injected DB failure must be 500"
        );
        // retry same key should not be 409, should be 202 (or 500 if still failing but not 409)
        let app = router(state.clone());
        let req = Request::builder()
            .uri(format!("/v1/users/{bob_id}/reset-password"))
            .method("POST")
            .header("authorization", format!("Bearer {admin_token}"))
            .header("Idempotency-Key", key.clone())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "retry after DB temp failure with same key must succeed (not 409)"
        );
    }

    #[tokio::test]
    async fn in_progress_has_retry_after_and_old_owner_cannot_complete() {
        let store = Arc::new(FakeIdempotencyStore::default());
        let now = Timestamp::from_unix_millis(1_700_000_000_000).expect("valid");
        let later = Timestamp::from_unix_millis(1_700_000_005_000).expect("valid");
        let expired = Timestamp::from_unix_millis(1_700_000_040_000).expect("valid");
        let key = Uuid::now_v7().to_string();
        let cmd = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: None,
            operation: "testOp".to_owned(),
            request_hash: [9u8; 32],
            now,
        };
        let claim1 = store.claim(cmd.clone()).await.expect("claim1");
        let owner1 = match claim1 {
            IdempotencyClaim::Acquired { owner, .. } => owner,
            other => panic!("expected Acquired got {other:?}"),
        };
        // second claim same payload before lease expiry -> InProgress with retry_after
        let cmd2 = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: None,
            operation: "testOp".to_owned(),
            request_hash: [9u8; 32],
            now: later,
        };
        let claim2 = store.claim(cmd2).await.expect("claim2");
        match claim2 {
            IdempotencyClaim::InProgress { retry_after_secs } => {
                assert!(
                    retry_after_secs > 0 && retry_after_secs <= 30,
                    "retry_after must be 1..30"
                );
            }
            other => panic!("expected InProgress got {other:?}"),
        }
        // different payload with same key while InProgress -> Reused (permanent 409)
        let cmd_diff = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: None,
            operation: "testOp".to_owned(),
            request_hash: [8u8; 32],
            now: later,
        };
        let claim_diff = store.claim(cmd_diff).await.expect("claim diff");
        assert_eq!(
            claim_diff,
            IdempotencyClaim::Reused,
            "different payload must be Reused"
        );

        // lease expiry: after 40s, same payload can be re-acquired with new owner
        let cmd3 = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: None,
            operation: "testOp".to_owned(),
            request_hash: [9u8; 32],
            now: expired,
        };
        let claim3 = store.claim(cmd3).await.expect("claim3");
        let owner2 = match claim3 {
            IdempotencyClaim::Acquired { owner, .. } => owner,
            other => panic!("expected Acquired after lease expiry got {other:?}"),
        };
        assert_ne!(owner1, owner2, "new lease must have different owner");
        // old owner cannot complete new lease
        let comp = orbisync_application::IdempotencyCompletion {
            status_code: 200,
            response_content_type: "application/json".to_owned(),
            response: orbisync_application::EncryptedResponse::new(b"{}".to_vec()),
        };
        let err = store
            .complete(key.clone(), owner1.clone(), comp.clone())
            .await
            .unwrap_err();
        assert_eq!(err, orbisync_application::IdentityPortError::Unavailable);
        // new owner can complete
        store
            .complete(key.clone(), owner2.clone(), comp.clone())
            .await
            .expect("new owner complete ok");
        // old owner cannot abandon new lease (already completed)
        let err2 = store
            .abandon(key.clone(), owner1.clone())
            .await
            .unwrap_err();
        assert_eq!(err2, orbisync_application::IdentityPortError::Unavailable);
        // completed replay
        let cmd4 = orbisync_application::IdempotencyClaimCommand {
            key: key.clone(),
            actor_user_id: None,
            operation: "testOp".to_owned(),
            request_hash: [9u8; 32],
            now: expired,
        };
        let claim4 = store.claim(cmd4).await.expect("claim4");
        assert!(matches!(claim4, IdempotencyClaim::Completed(_)));
    }
}
