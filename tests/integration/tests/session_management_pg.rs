//! Session inventory/revocation through HTTP and real PostgreSQL, with scoped fixtures.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt as _;
use orbisync_application::{
    CreateRefreshTokenCommand, IdentityPortError, IdentityRepository, PageRequest,
    RefreshRotationResult, RefreshTokenCreationStore, RefreshTokenReplacement,
    RefreshTokenRotationStore, RequestId, RotateRefreshTokenCommand, SessionAccess,
    SessionManagementPort, SessionSelection, pagination::CursorCodec,
};
use orbisync_domain::{AuthSession, AuthSessionId, Clock, Timestamp, UserId};
use orbisync_identity::token::AccessTokenService;
use orbisync_storage_postgres::{
    IdentityAdministrationStore, PgIdentityRepository, PgSessionManagementStore,
};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt as _;
use uuid::Uuid;

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

async fn user(pool: &PgPool, now: Timestamp) -> UserId {
    let id = UserId::generate();
    sqlx::query("INSERT INTO users(id,login_id,display_name,status,must_change_password,revision,created_at,updated_at) VALUES($1,$2,'Session test','active',false,1,$3,$3)")
        .bind(id.as_uuid()).bind(format!("session-test-{id}")).bind(now.as_offset_date_time()).execute(pool).await.unwrap();
    id
}
fn access(actor: UserId, owner: UserId, administrator: bool, now: Timestamp) -> SessionAccess {
    SessionAccess {
        actor_id: actor,
        user_id: owner,
        administrator,
        now,
        request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).unwrap(),
        source_ip: Some("192.0.2.1".into()),
    }
}
async fn session(
    repo: &PgIdentityRepository,
    owner: UserId,
    now: Timestamp,
    lifetime: i64,
) -> AuthSessionId {
    let session = AuthSession::new(
        AuthSessionId::generate(),
        owner,
        Timestamp::from_offset_date_time(now.as_offset_date_time() - time::Duration::hours(1)),
        Timestamp::from_offset_date_time(
            now.as_offset_date_time() + time::Duration::seconds(lifetime),
        ),
    )
    .unwrap();
    repo.save_session(&session).await.unwrap();
    session.id()
}
async fn request(
    state: &HttpState,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(body.map_or_else(Body::empty, |v| Body::from(v.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn session_inventory_and_revocation_enforce_scope_audit_and_all_credentials() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let db_now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let clock = Arc::new(FixedClock::new(Timestamp::from_offset_date_time(db_now)));
    let now = clock.now();
    let a = user(&pool, now).await;
    let b = user(&pool, now).await;
    let admin = user(&pool, now).await;
    let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
    let current = session(&repo, a, now, 3600).await;
    let other = session(&repo, a, now, 3600).await;
    let third = session(&repo, a, now, 3600).await;
    let expired = session(&repo, a, now, -1).await;
    let foreign = session(&repo, b, now, 3600).await;
    let foreign_second = session(&repo, b, now, 3600).await;
    let admin_session = session(&repo, admin, now, 3600).await;
    let store = Arc::new(PgSessionManagementStore::new(
        pool.clone(),
        CursorCodec::new(b"test-pagination-independent-key".to_vec()).unwrap(),
    ));
    let tokens = Arc::new(
        AccessTokenService::from_ed25519_pem(
            PRIVATE_PEM,
            PUBLIC_PEM,
            "orbisync",
            "orbisync-api",
            "test",
        )
        .unwrap(),
    );
    let token = tokens.issue(a, current, now).unwrap();
    let token_other = tokens.issue(a, other, now).unwrap();
    let admin_token = tokens.issue(admin, admin_session, now).unwrap();
    let state = HttpState::new(
        vec![],
        "orbisync",
        "test",
        1,
        clock,
        900,
        2592000,
        b"refresh-test-key".to_vec(),
    )
    .with_token_service(tokens)
    .with_identity_repository(repo.clone())
    .with_session_management(store.clone());
    let own = access(a, a, false, now);
    // The storage boundary independently enforces scope and signs owner-bound cursors.
    assert_eq!(
        store
            .list(
                &access(a, b, false, now),
                &PageRequest {
                    limit: 2,
                    after: None
                }
            )
            .await
            .unwrap_err(),
        IdentityPortError::NotFound
    );
    let page = store
        .list(
            &own,
            &PageRequest {
                limit: 2,
                after: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    let cursor = page.next.clone().expect("continuation");
    let second = store
        .list(
            &own,
            &PageRequest {
                limit: 2,
                after: Some(cursor.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(
        page.items
            .iter()
            .all(|a| second.items.iter().all(|b| a.id != b.id))
    );
    assert!(second.next.is_none());
    assert_eq!(
        store
            .list(
                &access(b, b, false, now),
                &PageRequest {
                    limit: 2,
                    after: Some(cursor)
                }
            )
            .await
            .unwrap_err(),
        IdentityPortError::InvalidRequest
    );
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "DELETE",
            &format!("/v1/auth/sessions/{foreign}"),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "GET",
            &format!("/v1/users/{b}/sessions"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "GET",
            "/v1/auth/sessions?user_id=foreign",
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let label = r#"{"device_label":"Office laptop"}"#;
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "PATCH",
            &format!("/v1/auth/sessions/{other}"),
            Some(label)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "PATCH",
            &format!("/v1/auth/sessions/{foreign}"),
            Some(label)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "PATCH",
            &format!("/v1/auth/sessions/{other}"),
            Some(r#"{"device_label":"\n"}"#)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, inventory) = request(
        &state,
        token.expose_secret(),
        "GET",
        "/v1/auth/sessions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = inventory["items"].as_array().unwrap();
    assert_eq!(items.iter().filter(|v| v["current"] == true).count(), 1);
    assert!(
        items
            .iter()
            .any(|v| v["session_id"] == expired.to_string() && v["status"] == "expired")
    );
    assert!(items.iter().any(|v| v["device_label"] == "Office laptop"));
    for item in items {
        assert_eq!(item.as_object().unwrap().len(), 7);
    }
    let text = inventory.to_string();
    for secret_field in [
        "access_token",
        "refresh_token",
        "resume_token",
        "digest",
        "password",
    ] {
        assert!(!text.contains(secret_field));
    }

    // Audit insertion failure must roll back a session mutation.
    let mut bad_audit = own.clone();
    bad_audit.source_ip = Some("invalid-ip".into());
    assert!(
        store
            .revoke(&bad_audit, SessionSelection::One(third))
            .await
            .is_err()
    );
    assert!(
        repo.find_session(third)
            .await
            .unwrap()
            .unwrap()
            .is_active_at(now)
    );

    let refresh_store = IdentityAdministrationStore::new(pool.clone());
    let digest = *Uuid::now_v7().as_bytes();
    let digest: [u8; 32] = [digest.as_slice(), digest.as_slice()]
        .concat()
        .try_into()
        .unwrap();
    let expires = Timestamp::from_offset_date_time(db_now + time::Duration::hours(1));
    RefreshTokenCreationStore::create(
        &refresh_store,
        CreateRefreshTokenCommand {
            token_id: Uuid::now_v7().to_string(),
            family_id: Uuid::now_v7().to_string(),
            session_id: other,
            digest,
            issued_at: now,
            expires_at: expires,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "DELETE",
            &format!("/v1/auth/sessions/{other}"),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &state,
            token.expose_secret(),
            "DELETE",
            &format!("/v1/auth/sessions/{other}"),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT,
        "retry is idempotent"
    );
    assert_eq!(
        request(
            &state,
            token_other.expose_secret(),
            "GET",
            "/v1/auth/sessions",
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED,
        "old signed JWT must fail immediately"
    );
    assert_eq!(
        RefreshTokenRotationStore::rotate(
            &refresh_store,
            RotateRefreshTokenCommand {
                presented_digest: digest,
                replacement: RefreshTokenReplacement {
                    token_id: Uuid::now_v7().to_string(),
                    digest: [17; 32],
                    issued_at: now,
                    expires_at: expires
                },
                now,
                request_id: own.request_id.clone()
            }
        )
        .await
        .unwrap(),
        RefreshRotationResult::Rejected
    );
    let (status, result) = request(
        &state,
        token.expose_secret(),
        "POST",
        "/v1/auth/sessions/revoke-others",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["revoked_count"], 2);
    assert!(
        repo.find_session(current)
            .await
            .unwrap()
            .unwrap()
            .is_active_at(now)
    );
    assert!(
        !repo
            .find_session(third)
            .await
            .unwrap()
            .unwrap()
            .is_active_at(now)
    );
    assert!(
        repo.find_session(foreign)
            .await
            .unwrap()
            .unwrap()
            .is_active_at(now)
    );

    // Grant only read first: viewing inventory must not imply force-revoke.
    let role = Uuid::now_v7();
    sqlx::query("INSERT INTO roles(id,name,revision) VALUES($1,$2,1)")
        .bind(role)
        .bind(format!("session-admin-{role}"))
        .execute(&pool)
        .await
        .unwrap();
    for permission in ["admin.sessions.read", "admin.sessions.revoke"] {
        sqlx::query("INSERT INTO permissions(name) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(permission)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO user_roles(user_id,role_id) VALUES($1,$2)")
        .bind(admin.as_uuid())
        .bind(role)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions(role_id,permission_name) VALUES($1,'admin.sessions.read')",
    )
    .bind(role)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        request(
            &state,
            admin_token.expose_secret(),
            "GET",
            &format!("/v1/users/{b}/sessions"),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &state,
            admin_token.expose_secret(),
            "DELETE",
            &format!("/v1/users/{b}/sessions"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query(
        "INSERT INTO role_permissions(role_id,permission_name) VALUES($1,'admin.sessions.revoke')",
    )
    .bind(role)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        request(
            &state,
            admin_token.expose_secret(),
            "DELETE",
            &format!("/v1/users/{b}/sessions/{current}"),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &state,
            admin_token.expose_secret(),
            "DELETE",
            &format!("/v1/users/{b}/sessions/{foreign_second}"),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (status, result) = request(
        &state,
        admin_token.expose_secret(),
        "DELETE",
        &format!("/v1/users/{b}/sessions"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["revoked_count"], 1);
    assert!(
        !repo
            .find_session(foreign)
            .await
            .unwrap()
            .unwrap()
            .is_active_at(now)
    );
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE actor_user_id=ANY($1) AND action LIKE 'session.%'",
    )
    .bind(vec![a.as_uuid(), admin.as_uuid()])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(audit_count >= 8);
    let labels_in_audit:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=ANY($1) AND metadata::text LIKE '%Office laptop%'").bind(vec![a.as_uuid(),admin.as_uuid()]).fetch_one(&pool).await.unwrap();
    assert_eq!(
        labels_in_audit, 0,
        "advisory labels are excluded from audit payloads"
    );
}

#[tokio::test]
async fn builtin_capability_upgrade_is_conservative_idempotent_and_advances_revision() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let mut tx = pool.begin().await.unwrap();
    let schema = format!("session_upgrade_{}", Uuid::now_v7().simple());
    sqlx::raw_sql(&format!("CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}; CREATE TABLE roles(id uuid PRIMARY KEY,name text UNIQUE,description text,revision bigint NOT NULL); CREATE TABLE permissions(name text PRIMARY KEY); CREATE TABLE role_permissions(role_id uuid REFERENCES roles(id),permission_name text REFERENCES permissions(name),PRIMARY KEY(role_id,permission_name));")).execute(&mut *tx).await.unwrap();
    let builtin = Uuid::now_v7();
    let custom = Uuid::now_v7();
    let world = Uuid::now_v7();
    for (id, name, description) in [
        (builtin, "Administrator", "Initial administration role"),
        (
            custom,
            "Custom Administrator",
            "Initial administration role",
        ),
        (
            world,
            "World Administrator",
            "Initial world administration role",
        ),
    ] {
        sqlx::query("INSERT INTO roles(id,name,description,revision) VALUES($1,$2,$3,1)")
            .bind(id)
            .bind(name)
            .bind(description)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for permission in [
        "admin.users.read",
        "admin.users.create",
        "admin.users.update",
        "admin.users.status",
        "admin.users.import",
        "admin.users.credentials.reset",
        "admin.roles.read",
        "admin.roles.create",
        "admin.roles.update",
        "admin.roles.delete",
        "admin.roles.assign",
        "admin.audit.read",
        "admin.diagnostics.read",
        "admin.worlds.create",
        "admin.worlds.read",
        "admin.worlds.update",
        "admin.worlds.archive",
    ] {
        sqlx::query("INSERT INTO permissions(name) VALUES($1)")
            .bind(permission)
            .execute(&mut *tx)
            .await
            .unwrap();
        for role in [builtin, custom, world] {
            sqlx::query("INSERT INTO role_permissions(role_id,permission_name) VALUES($1,$2)")
                .bind(role)
                .bind(permission)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
    }
    let migration = include_str!("../../../migrations/0026_builtin_admin_capabilities.sql");
    for _ in 0..2 {
        sqlx::raw_sql(migration).execute(&mut *tx).await.unwrap();
    }
    let new_permissions:Vec<(Uuid,i64)>=sqlx::query_as("SELECT role_id,count(*) FROM role_permissions WHERE permission_name IN ('admin.sessions.read','admin.sessions.revoke','admin.auth.keys.reload') GROUP BY role_id").fetch_all(&mut *tx).await.unwrap();
    assert_eq!(
        new_permissions,
        vec![(builtin, 3)],
        "custom and world roles must not be elevated"
    );
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM roles WHERE id=$1")
        .bind(builtin)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        revision, 2,
        "all grants advance revision once; replay does not"
    );
    sqlx::query("DELETE FROM role_permissions WHERE role_id=$1 AND permission_name IN ('admin.roles.assign','admin.sessions.read','admin.sessions.revoke','admin.auth.keys.reload')").bind(builtin).execute(&mut *tx).await.unwrap();
    sqlx::raw_sql(migration).execute(&mut *tx).await.unwrap();
    let grants:i64=sqlx::query_scalar("SELECT count(*) FROM role_permissions WHERE permission_name IN ('admin.sessions.read','admin.sessions.revoke','admin.auth.keys.reload')").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(
        grants, 0,
        "a deliberately restricted builtin role needs an explicit grant"
    );
    tx.rollback().await.unwrap();
}
