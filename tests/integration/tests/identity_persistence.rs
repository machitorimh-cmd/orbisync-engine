//! Real PostgreSQL tests for the Milestone 1 persistence layer.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use orbisync_application::{
    EncryptedResponse, ExtensionEvent, IdempotencyClaim as ApplicationIdempotencyClaim,
    IdempotencyClaimCommand, IdempotencyCompletion as ApplicationIdempotencyCompletion,
    IdempotencyStore as ApplicationIdempotencyStore, IdentityAdministrationStore as _,
    IdentityAuditEvent, IdentityMutation, RefreshRotationResult, RefreshTokenReplacement,
    RefreshTokenRotationStore, RequestId, RotateRefreshTokenCommand,
};
use orbisync_domain::{
    Credential, LoginId, PasswordHash, Permission, Role, RoleId, Timestamp, User, UserId,
};
use orbisync_storage_postgres::{
    AuditEvent, IdempotencyClaim, IdempotencyStore, IdentityAdministrationStore, NewRefreshToken,
    NewUser, RefreshRotation,
};
use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

mod common;

async fn database() -> Option<PgPool> {
    common::pool_or_skip().await
}

fn audit(action: &str, target_id: Uuid, now: OffsetDateTime) -> AuditEvent {
    AuditEvent {
        id: Uuid::now_v7(),
        occurred_at: now,
        actor_user_id: None,
        action: action.to_owned(),
        target_type: Some("user".to_owned()),
        target_id: Some(target_id.to_string()),
        request_id: Some(format!("req_{}", Uuid::now_v7())),
        source_ip: Some("127.0.0.1".to_owned()),
        result: "success".to_owned(),
        metadata: json!({"changed_fields": ["status"]}),
    }
}

async fn insert_user(pool: &PgPool, user_id: Uuid, now: OffsetDateTime) {
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, 'Test', 'active', true, 1, $3, $3)")
        .bind(user_id)
        .bind(format!("login-{user_id}"))
        .bind(now)
        .execute(pool)
        .await
        .expect("user inserted");
}

#[tokio::test]
async fn constraints_are_enforced_by_postgresql() {
    let Some(pool) = database().await else { return };
    let now = OffsetDateTime::now_utc();
    let user_id = Uuid::now_v7();
    insert_user(&pool, user_id, now).await;

    let duplicate = sqlx::query("INSERT INTO users (id, login_id, display_name, status, revision, created_at, updated_at) VALUES ($1, $2, 'Other', 'active', 1, $3, $3)")
        .bind(Uuid::now_v7())
        .bind(format!("login-{user_id}"))
        .bind(now)
        .execute(&pool)
        .await;
    assert!(duplicate.is_err(), "login_id must be unique");

    let bad_check = sqlx::query("UPDATE users SET status = 'unknown' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await;
    assert!(
        bad_check.is_err(),
        "user status check must reject unknown values"
    );

    let bad_fk = sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, 'hash', $2)")
        .bind(Uuid::now_v7())
        .bind(now)
        .execute(&pool)
        .await;
    assert!(bad_fk.is_err(), "credential FK must reject an unknown user");
}

#[tokio::test]
async fn concurrent_refresh_consume_detects_reuse_and_revokes_session() {
    let Some(pool) = database().await else { return };
    let now = OffsetDateTime::now_utc();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let family_id = Uuid::now_v7();
    insert_user(&pool, user_id, now).await;
    sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1, $2, 'active', $3, $4, 1)")
        .bind(session_id).bind(user_id).bind(now).bind(now + Duration::days(30))
        .execute(&pool).await.expect("session inserted");
    let original_digest = Uuid::now_v7().as_bytes().to_vec();
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(Uuid::now_v7()).bind(session_id).bind(family_id).bind(&original_digest)
        .bind(now).bind(now + Duration::days(30)).execute(&pool).await.expect("token inserted");

    let store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    let first = NewRefreshToken {
        id: Uuid::now_v7(),
        token_digest: Uuid::now_v7().as_bytes().to_vec(),
        issued_at: now,
        expires_at: now + Duration::days(30),
    };
    let second = NewRefreshToken {
        id: Uuid::now_v7(),
        token_digest: Uuid::now_v7().as_bytes().to_vec(),
        issued_at: now,
        expires_at: now + Duration::days(30),
    };
    let left = {
        let store = Arc::clone(&store);
        let digest = original_digest.clone();
        let request_id = format!("req_{}", Uuid::now_v7());
        tokio::spawn(async move {
            store
                .rotate_refresh_token(&digest, &first, now, &request_id)
                .await
        })
    };
    let right = {
        let store = Arc::clone(&store);
        let digest = original_digest.clone();
        let request_id = format!("req_{}", Uuid::now_v7());
        tokio::spawn(async move {
            store
                .rotate_refresh_token(&digest, &second, now, &request_id)
                .await
        })
    };
    let outcomes = [
        left.await
            .expect("task completes")
            .expect("rotation succeeds"),
        right
            .await
            .expect("task completes")
            .expect("rotation succeeds"),
    ];
    assert!(
        outcomes
            .iter()
            .any(|r| matches!(r, RefreshRotation::Rotated { .. }))
    );
    assert!(outcomes.contains(&RefreshRotation::ReuseDetected));
    let status: String = sqlx::query_scalar("SELECT status FROM auth_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .expect("session exists");
    assert_eq!(status, "revoked");
}

#[tokio::test]
async fn concurrent_login_failures_do_not_lose_updates() {
    let Some(pool) = database().await else { return };
    let now = OffsetDateTime::now_utc();
    let user_id = Uuid::now_v7();
    insert_user(&pool, user_id, now).await;
    sqlx::query("INSERT INTO user_credentials (user_id, password_hash, password_changed_at) VALUES ($1, 'argon2id-hash', $2)")
        .bind(user_id).bind(now).execute(&pool).await.expect("credential inserted");
    let store = Arc::new(IdentityAdministrationStore::new(pool.clone()));
    for _ in 0..4 {
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let store = Arc::clone(&store);
            let now_copy = now;
            tasks.push(tokio::spawn(async move {
                store.record_login_failure(user_id, now_copy).await
            }));
        }
        for task in tasks {
            task.await
                .expect("task completes")
                .expect("increment succeeds");
        }
    }
    let count: i32 =
        sqlx::query_scalar("SELECT failed_login_count FROM user_credentials WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("credential exists");
    assert_eq!(count, 20);
}

#[tokio::test]
async fn disabling_user_revokes_every_session() {
    let Some(pool) = database().await else { return };
    let now = OffsetDateTime::now_utc();
    let user_id = Uuid::now_v7();
    insert_user(&pool, user_id, now).await;
    for _ in 0..3 {
        sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1, $2, 'active', $3, $4, 1)")
            .bind(Uuid::now_v7()).bind(user_id).bind(now).bind(now + Duration::days(1))
            .execute(&pool).await.expect("session inserted");
    }
    let store = IdentityAdministrationStore::new(pool.clone());
    assert!(
        store
            .disable_user_with_audit(user_id, 1, now, &audit("user.disabled", user_id, now))
            .await
            .expect("disable succeeds")
    );
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auth_sessions WHERE user_id = $1 AND status = 'active'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("count succeeds");
    assert_eq!(active, 0);
}

#[tokio::test]
async fn simultaneous_idempotency_retries_have_one_owner() {
    let Some(pool) = database().await else { return };
    let store = Arc::new(IdempotencyStore::new(pool));
    let key = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let hash = vec![4_u8; 32];
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let hash = hash.clone();
        tasks.push(tokio::spawn(async move {
            store.claim(key, None, "users.create", &hash, now).await
        }));
    }
    let mut acquired = 0;
    for task in tasks {
        match task.await.expect("task completes").expect("claim succeeds") {
            IdempotencyClaim::Acquired { .. } => acquired += 1,
            IdempotencyClaim::InProgress { .. } => {}
            IdempotencyClaim::Existing(_) => {}
            IdempotencyClaim::Reused => panic!("identical request must not be treated as reuse"),
        }
    }
    assert_eq!(acquired, 1);
    let reused = store
        .claim(key, None, "users.create", &[5; 32], now)
        .await
        .expect("claim succeeds");
    assert!(matches!(reused, IdempotencyClaim::Reused));
}

#[tokio::test]
async fn runtime_role_cannot_update_or_delete_audit_events() {
    let Some(pool) = database().await else { return };
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO audit_events (id, occurred_at, action, result) VALUES ($1, now(), 'test', 'success')")
        .bind(id).execute(&pool).await.expect("audit inserted");
    let mut connection = pool.acquire().await.expect("connection acquired");
    sqlx::query("SET ROLE orbisync_runtime")
        .execute(&mut *connection)
        .await
        .expect("runtime role selected");
    assert!(
        sqlx::query("UPDATE audit_events SET action = 'tampered' WHERE id = $1")
            .bind(id)
            .execute(&mut *connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM audit_events WHERE id = $1")
            .bind(id)
            .execute(&mut *connection)
            .await
            .is_err()
    );
    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("role reset");
}

#[tokio::test]
async fn audit_failure_rolls_back_identity_mutation_without_leaking_secret() {
    let Some(pool) = database().await else { return };
    let now = OffsetDateTime::now_utc();
    let user_id = Uuid::now_v7();
    let secret_hash = "$argon2id$secret-material-must-not-leak";
    let user = NewUser {
        id: user_id,
        login_id: format!("login-{user_id}"),
        display_name: "Rollback".to_owned(),
        password_hash: secret_hash.to_owned(),
        occurred_at: now,
    };
    let mut event = audit("user.created", user_id, now);
    event.source_ip = Some("not-an-ip-address".to_owned());
    let error = IdentityAdministrationStore::new(pool.clone())
        .create_user_with_audit(&user, &event)
        .await
        .expect_err("invalid audit insert must fail");
    assert!(!format!("{error:?}").contains(secret_hash));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("count succeeds");
    assert_eq!(
        count, 0,
        "identity mutation must roll back with audit insert"
    );
}

#[tokio::test]
async fn application_administration_port_maps_every_mutation() {
    let Some(pool) = database().await else { return };
    let now = Timestamp::from_offset_date_time(OffsetDateTime::now_utc());
    let actor_id = UserId::generate();
    let user = User::new(
        UserId::generate(),
        LoginId::new(format!("port-{}", Uuid::now_v7())).expect("valid login"),
        "Port User",
        now,
    )
    .expect("valid user");
    let credential = Credential::new(
        user.id(),
        PasswordHash::new("$argon2id$v=19$test").expect("valid PHC marker"),
        now,
    );
    let store = IdentityAdministrationStore::new(pool.clone());
    store
        .apply_with_event(
            IdentityMutation::CreateUser {
                user: user.clone(),
                credential,
            },
            port_audit("user.created", user.id().to_string(), actor_id, now),
            ExtensionEvent::UserCreated { user_id: user.id() },
        )
        .await
        .expect("user mutation applies");

    let created_event: (Uuid, String, serde_json::Value) = sqlx::query_as(
        "SELECT event_id, event_kind, payload FROM outbox_events WHERE event_kind = 'user.created' AND payload->>'user_id' = $1",
    )
    .bind(user.id().to_string())
    .fetch_one(&pool)
    .await
    .expect("user.created outbox event exists");
    assert_eq!(created_event.1, "user.created");
    assert_eq!(created_event.2["user_id"], user.id().to_string());

    let mut disabled_user = user.clone();
    assert!(disabled_user.disable(now).expect("user disables"));
    store
        .apply_with_event(
            IdentityMutation::StoreUserStatus {
                user: disabled_user.clone(),
            },
            port_audit("user.disabled", user.id().to_string(), actor_id, now),
            ExtensionEvent::UserDisabled { user_id: user.id() },
        )
        .await
        .expect("user disable mutation applies");
    let outbox_rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT event_id, event_kind FROM outbox_events WHERE event_id IN ($1, (SELECT event_id FROM outbox_events WHERE event_kind = 'user.disabled' AND payload->>'user_id' = $2)) ORDER BY event_kind",
    )
    .bind(created_event.0)
    .bind(user.id().to_string())
    .fetch_all(&pool)
    .await
    .expect("identity outbox rows query");
    assert_eq!(outbox_rows.len(), 2);
    assert_eq!(outbox_rows[0].1, "user.created");
    assert_eq!(outbox_rows[1].1, "user.disabled");
    assert_ne!(outbox_rows[0].0, outbox_rows[1].0);

    let permission = Permission::new("admin.users.read").expect("valid permission");
    let role = Role::new(
        RoleId::generate(),
        format!("Reader-{}", Uuid::now_v7()),
        None,
        [permission.clone()],
    )
    .expect("valid role");
    store
        .apply(
            IdentityMutation::StoreRole { role: role.clone() },
            port_audit("role.created", role.id().to_string(), actor_id, now),
        )
        .await
        .expect("role mutation applies");
    store
        .apply(
            IdentityMutation::ReplaceUserRoles {
                user_id: user.id(),
                role_ids: [role.id()].into_iter().collect(),
            },
            port_audit("user.roles_replaced", user.id().to_string(), actor_id, now),
        )
        .await
        .expect("role assignment applies");

    let session_id = Uuid::now_v7();
    sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1, $2, 'active', $3, $4, 0)")
        .bind(session_id)
        .bind(user.id().as_uuid())
        .bind(now.as_offset_date_time())
        .bind(now.as_offset_date_time() + Duration::days(1))
        .execute(&pool)
        .await
        .expect("session inserted");
    store
        .apply(
            IdentityMutation::RevokeUserSessions {
                user_id: user.id(),
                occurred_at: now,
            },
            port_audit("session.revoked", session_id.to_string(), actor_id, now),
        )
        .await
        .expect("session revocation applies");

    let role_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_roles ur JOIN role_permissions rp ON rp.role_id = ur.role_id WHERE ur.user_id = $1 AND rp.permission_name = $2",
    )
    .bind(user.id().as_uuid())
    .bind(permission.to_string())
    .fetch_one(&pool)
    .await
    .expect("mapping query succeeds");
    assert_eq!(role_count, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM auth_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .expect("session exists");
    assert_eq!(status, "revoked");
}

fn port_audit(
    action: &'static str,
    resource_id: String,
    actor_id: UserId,
    occurred_at: Timestamp,
) -> IdentityAuditEvent {
    IdentityAuditEvent {
        occurred_at,
        actor_id: Some(actor_id),
        action,
        resource_id: Some(resource_id),
        request_id: RequestId::new(format!("req_{}", UserId::generate()))
            .expect("valid request id"),
        succeeded: true,
    }
}

#[tokio::test]
async fn application_idempotency_and_refresh_ports_map_without_raw_secrets() {
    let Some(pool) = database().await else { return };
    let now = Timestamp::from_offset_date_time(OffsetDateTime::now_utc());
    let key = Uuid::now_v7();
    let idempotency = IdempotencyStore::new(pool.clone());
    let claim = IdempotencyClaimCommand {
        key: key.to_string(),
        actor_user_id: None,
        operation: "users.create".to_owned(),
        request_hash: [21; 32],
        now,
    };
    let claim_result = ApplicationIdempotencyStore::claim(&idempotency, claim.clone())
        .await
        .expect("claim");
    let owner = match claim_result {
        ApplicationIdempotencyClaim::Acquired { owner, .. } => owner,
        other => panic!("expected Acquired, got {other:?}"),
    };
    assert!(owner.len() > 10);
    ApplicationIdempotencyStore::complete(
        &idempotency,
        key.to_string(),
        owner,
        ApplicationIdempotencyCompletion {
            status_code: 201,
            response_content_type: "application/json".to_owned(),
            response: EncryptedResponse::new(vec![0xA5; 48]),
        },
    )
    .await
    .expect("completion");
    assert!(matches!(
        ApplicationIdempotencyStore::claim(&idempotency, claim)
            .await
            .expect("replay"),
        ApplicationIdempotencyClaim::Completed(_)
    ));

    let user_id = Uuid::now_v7();
    insert_user(&pool, user_id, now.as_offset_date_time()).await;
    let session_id = Uuid::now_v7();
    let family_id = Uuid::now_v7();
    sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1, $2, 'active', $3, $4, 0)")
        .bind(session_id).bind(user_id).bind(now.as_offset_date_time())
        .bind(now.as_offset_date_time() + Duration::days(30)).execute(&pool).await.expect("session");
    let mut digest = [31_u8; 32];
    digest[..16].copy_from_slice(Uuid::now_v7().as_bytes());
    let mut replacement_digest = [32_u8; 32];
    replacement_digest[..16].copy_from_slice(Uuid::now_v7().as_bytes());
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(Uuid::now_v7()).bind(session_id).bind(family_id).bind(digest.as_slice())
        .bind(now.as_offset_date_time()).bind(now.as_offset_date_time() + Duration::days(30))
        .execute(&pool).await.expect("refresh token");
    let rotation = IdentityAdministrationStore::new(pool);
    let outcome = RefreshTokenRotationStore::rotate(
        &rotation,
        RotateRefreshTokenCommand {
            presented_digest: digest,
            replacement: RefreshTokenReplacement {
                token_id: Uuid::now_v7().to_string(),
                digest: replacement_digest,
                issued_at: now,
                expires_at: Timestamp::from_offset_date_time(
                    now.as_offset_date_time() + Duration::days(30),
                ),
            },
            now,
            request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("request id"),
        },
    )
    .await
    .expect("rotation");
    assert!(matches!(outcome, RefreshRotationResult::Rotated { .. }));
}
