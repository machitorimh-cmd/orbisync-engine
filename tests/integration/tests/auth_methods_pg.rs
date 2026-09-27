//! ADR-026 behaviour that only real PostgreSQL can demonstrate.
//!
//! These cover the claims that rest on SQL the compiler cannot check: the
//! refresh cap that stops a temporary subject renewing itself, the tombstone
//! that keeps checkpoint restore working after revocation, and the unique
//! constraint that separates two issuers using the same subject string.
//!
//! Every test skips when `DATABASE_URL` is unset. A skipped test is *not* a
//! pass — the suite has to be run against a database for these to mean
//! anything.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::OnceLock;
use tokio::sync::Mutex;

use orbisync_application::{
    CheckpointStore, EphemeralSubjectRecord, EphemeralSubjectScope, ExternalIdentityRecord,
    ExternalIdentityStore, LoginAuditEvent, LoginRefreshRecord, LoginSessionRecord,
    LoginTransactionStore, NewSubjectUser, RequestId, ScopeDecision, SubjectCommit,
};
use orbisync_domain::{AuthMethod, LoginId, RoleId, Timestamp, UserId, UserKind};
use orbisync_storage_postgres::{
    NewRefreshToken, PgEphemeralSubjectScope, PgExternalIdentityStore, PgLoginStore,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Serializes the tests: they all truncate the same tables.
fn db_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<PgPool> {
    common::pool_or_skip().await
}

async fn clean(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE users, user_credentials, roles, permissions, role_permissions, user_roles, \
         auth_sessions, refresh_tokens, audit_events, idempotency_records, realtime_tickets, \
         ephemeral_subjects, external_identities, instance_checkpoints, world_instances, \
         world_definitions, persistent_entities CASCADE",
    )
    .execute(pool)
    .await
    .expect("TRUNCATE should succeed");
}

fn ts(millis: i64) -> Timestamp {
    Timestamp::from_unix_millis(millis).expect("valid timestamp")
}

const NOW_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;
const THIRTY_DAYS_MS: i64 = 2_592_000_000;

fn audit(now: Timestamp, actor: UserId) -> LoginAuditEvent {
    LoginAuditEvent {
        occurred_at: now,
        request_id: RequestId::new(format!("req_{}", Uuid::now_v7())).expect("request id"),
        source_ip: None,
        action: "auth.guest",
        actor_id: Some(actor),
        succeeded: true,
    }
}

fn session_record(id: UserId, now: Timestamp, expires: Timestamp) -> LoginSessionRecord {
    LoginSessionRecord {
        id: orbisync_domain::AuthSessionId::generate(),
        user_id: id,
        created_at: now,
        expires_at: expires,
    }
}

fn refresh_record(now: Timestamp, expires: Timestamp, seed: u8) -> LoginRefreshRecord {
    LoginRefreshRecord {
        token_id: Uuid::now_v7().to_string(),
        family_id: Uuid::now_v7().to_string(),
        digest: [seed; 32],
        issued_at: now,
        expires_at: expires,
    }
}

/// Inserts a temporary subject the way `SessionIssuer` does, and returns its
/// ids so a test can inspect what the server actually wrote.
async fn issue_guest(
    pool: &PgPool,
    now: Timestamp,
    deadline: Timestamp,
    allowed_worlds: Vec<Uuid>,
    digest_seed: u8,
) -> (UserId, orbisync_domain::AuthSessionId, String) {
    let store = PgLoginStore::new(pool.clone());
    let user_id = UserId::generate();
    let login_id = LoginId::for_generated_subject(AuthMethod::Guest, user_id).expect("login id");
    let new_user = NewSubjectUser {
        login_id,
        display_name: "Guest-TEST".to_owned(),
        kind: UserKind::Guest,
        created_at: now,
    };
    let ephemeral = EphemeralSubjectRecord {
        method: AuthMethod::Guest,
        created_at: now,
        expires_at: deadline,
        allowed_worlds,
    };
    // The session is clamped to the deadline at issue time, as SessionIssuer
    // does; issuing at the global lifetime would leave a token outliving the
    // subject even once rotation clamps the rest.
    let session = session_record(user_id, now, deadline);
    let refresh = refresh_record(now, deadline, digest_seed);
    let audit = audit(now, user_id);
    store
        .commit_subject_success(SubjectCommit {
            user_id,
            new_user: Some(&new_user),
            grant_roles: &[],
            ephemeral: Some(&ephemeral),
            external: None,
            session: &session,
            refresh: &refresh,
            audit: &audit,
        })
        .await
        .expect("guest issue must commit");
    (user_id, session.id, refresh.token_id.clone())
}

// ---------------------------------------------------------------------------
// T20 — migration and constraints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_0018_applies_and_enforces_its_constraints() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;

    // Existing rows default to 'account', so the local path is untouched.
    let kind_default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'users' AND column_name = 'kind'",
    )
    .fetch_one(&pool)
    .await
    .expect("users.kind exists");
    assert!(kind_default.contains("account"), "got {kind_default}");

    let user_id = UserId::generate();
    sqlx::query(
        "INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) \
         VALUES ($1, $2, 'Guest', 'active', FALSE, 1, now(), now())",
    )
    .bind(user_id.as_uuid())
    .bind(format!("guest:{user_id}"))
    .execute(&pool)
    .await
    .expect("insert user");

    // An empty world boundary must be impossible to store: "no worlds" must
    // never be readable as "every world".
    let empty = sqlx::query(
        "INSERT INTO ephemeral_subjects (user_id, method, created_at, expires_at, allowed_worlds) \
         VALUES ($1, 'guest', now(), now() + interval '1 hour', '{}')",
    )
    .bind(user_id.as_uuid())
    .execute(&pool)
    .await;
    assert!(empty.is_err(), "an empty allowed_worlds must be rejected");

    // An unknown method name must be impossible too.
    let bad_method = sqlx::query(
        "INSERT INTO ephemeral_subjects (user_id, method, created_at, expires_at, allowed_worlds) \
         VALUES ($1, 'sso', now(), now() + interval '1 hour', ARRAY[gen_random_uuid()])",
    )
    .bind(user_id.as_uuid())
    .execute(&pool)
    .await;
    assert!(bad_method.is_err(), "an unknown method must be rejected");
}

// ---------------------------------------------------------------------------
// T18 — external identity mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_issuer_and_subject_pair_maps_to_one_user_and_issuers_stay_separate() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let store = PgLoginStore::new(pool.clone());
    let lookup = PgExternalIdentityStore::new(pool.clone());
    let now = ts(NOW_MS);

    let mut issued = Vec::new();
    for (index, issuer) in ["https://idp-a.test", "https://idp-b.test"]
        .into_iter()
        .enumerate()
    {
        let user_id = UserId::generate();
        let login_id =
            LoginId::for_generated_subject(AuthMethod::External, user_id).expect("login id");
        let new_user = NewSubjectUser {
            login_id,
            display_name: "shared-subject".to_owned(),
            kind: UserKind::External,
            created_at: now,
        };
        // Both issuers name the same subject string. Only the pair is unique.
        let external = ExternalIdentityRecord {
            issuer: issuer.to_owned(),
            subject: "shared-subject".to_owned(),
            created_at: now,
        };
        let session = session_record(user_id, now, ts(NOW_MS + THIRTY_DAYS_MS));
        let refresh = refresh_record(now, ts(NOW_MS + THIRTY_DAYS_MS), 100 + index as u8);
        let audit = audit(now, user_id);
        store
            .commit_subject_success(SubjectCommit {
                user_id,
                new_user: Some(&new_user),
                grant_roles: &[],
                ephemeral: None,
                external: Some(&external),
                session: &session,
                refresh: &refresh,
                audit: &audit,
            })
            .await
            .expect("external issue must commit");
        issued.push((issuer, user_id));
    }

    let (issuer_a, user_a) = issued[0];
    let (issuer_b, user_b) = issued[1];
    assert_ne!(
        user_a, user_b,
        "the same subject from two issuers must be two users"
    );
    assert_eq!(
        lookup
            .find_user(issuer_a, "shared-subject")
            .await
            .expect("lookup"),
        Some(user_a),
        "the same pair must resolve to the same user every time"
    );
    assert_eq!(
        lookup
            .find_user(issuer_b, "shared-subject")
            .await
            .expect("lookup"),
        Some(user_b)
    );
    assert_eq!(
        lookup
            .find_user("https://idp-c.test", "shared-subject")
            .await
            .expect("lookup"),
        None,
        "an unknown issuer must not inherit another issuer's user"
    );
}

#[tokio::test]
async fn an_external_subject_holds_no_credential_row() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let now = ts(NOW_MS);
    let (user_id, _, _) =
        issue_guest(&pool, now, ts(NOW_MS + HOUR_MS), vec![Uuid::now_v7()], 7).await;

    // No credential row means `find_login` and `find_account`, which INNER
    // JOIN that table, cannot resolve the subject at all: the password path is
    // closed by the schema rather than by a check somewhere.
    let credentials: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_credentials WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(credentials, 0);
}

// ---------------------------------------------------------------------------
// T25 — the refresh cap (D-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotation_still_extends_a_permanent_subject_to_the_configured_lifetime() {
    // The regression that matters most: the cap must not change how an
    // ordinary account behaves. A local session has no ledger row, so the
    // LEFT JOIN yields NULL and the extension has to run unclamped.
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let store = orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone());
    let now = ts(NOW_MS);

    let user_id = UserId::generate();
    sqlx::query(
        "INSERT INTO users (id, login_id, display_name, status, must_change_password, kind, revision, created_at, updated_at) \
         VALUES ($1, 'ada', 'Ada', 'active', FALSE, 'account', 1, now(), now())",
    )
    .bind(user_id.as_uuid())
    .execute(&pool)
    .await
    .expect("insert account");

    let session_id = orbisync_domain::AuthSessionId::generate();
    let short_expiry =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(NOW_MS + HOUR_MS) * 1_000_000)
            .expect("timestamp");
    sqlx::query(
        "INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) \
         VALUES ($1, $2, 'active', $3, $4, 0)",
    )
    .bind(session_id.as_uuid())
    .bind(user_id.as_uuid())
    .bind(now.as_offset_date_time())
    .bind(short_expiry)
    .execute(&pool)
    .await
    .expect("insert session");

    let original = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO refresh_tokens (id, session_id, family_id, token_digest, issued_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(original)
    .bind(session_id.as_uuid())
    .bind(Uuid::now_v7())
    .bind(vec![1_u8; 32])
    .bind(now.as_offset_date_time())
    .bind(short_expiry)
    .execute(&pool)
    .await
    .expect("insert refresh token");

    let far_future =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(NOW_MS + THIRTY_DAYS_MS) * 1_000_000)
            .expect("timestamp");
    let replacement = NewRefreshToken {
        id: Uuid::now_v7(),
        token_digest: vec![2_u8; 32],
        issued_at: now.as_offset_date_time(),
        expires_at: far_future,
    };
    let outcome = store
        .rotate_refresh_token(
            &[1_u8; 32],
            &replacement,
            now.as_offset_date_time(),
            &Uuid::now_v7().to_string(),
        )
        .await
        .expect("rotation must succeed for a permanent subject");
    assert!(
        matches!(
            outcome,
            orbisync_storage_postgres::RefreshRotation::Rotated { .. }
        ),
        "expected a rotation, got {outcome:?}"
    );

    let session_expiry: OffsetDateTime =
        sqlx::query_scalar("SELECT expires_at FROM auth_sessions WHERE id = $1")
            .bind(session_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("read session");
    assert_eq!(
        session_expiry, far_future,
        "a permanent subject's session must still extend to the full lifetime"
    );
}

#[tokio::test]
async fn repeated_rotation_cannot_push_a_temporary_subject_past_its_deadline() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let store = orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone());
    let now = ts(NOW_MS);
    let deadline = ts(NOW_MS + HOUR_MS);
    let (user_id, session_id, _) = issue_guest(&pool, now, deadline, vec![Uuid::now_v7()], 1).await;

    let deadline_at = deadline.as_offset_date_time();
    let far_future =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(NOW_MS + THIRTY_DAYS_MS) * 1_000_000)
            .expect("timestamp");

    // Rotate repeatedly, each time asking for the full global lifetime.
    let mut digest = [1_u8; 32];
    for round in 0..3_u8 {
        let next_digest = [200 + round; 32];
        let replacement = NewRefreshToken {
            id: Uuid::now_v7(),
            token_digest: next_digest.to_vec(),
            issued_at: now.as_offset_date_time(),
            expires_at: far_future,
        };
        store
            .rotate_refresh_token(
                &digest,
                &replacement,
                now.as_offset_date_time(),
                &Uuid::now_v7().to_string(),
            )
            .await
            .expect("rotation within the deadline must succeed");
        digest = next_digest;

        let session_expiry: OffsetDateTime =
            sqlx::query_scalar("SELECT expires_at FROM auth_sessions WHERE id = $1")
                .bind(session_id.as_uuid())
                .fetch_one(&pool)
                .await
                .expect("read session");
        assert_eq!(
            session_expiry, deadline_at,
            "round {round}: the session must not move past the subject's deadline"
        );

        let token_expiry: OffsetDateTime =
            sqlx::query_scalar("SELECT expires_at FROM refresh_tokens WHERE token_digest = $1")
                .bind(next_digest.to_vec())
                .fetch_one(&pool)
                .await
                .expect("read token");
        assert_eq!(
            token_expiry, deadline_at,
            "round {round}: the replacement token must not outlive the subject"
        );

        // The deadline itself is written once and never rewritten.
        let ledger: OffsetDateTime =
            sqlx::query_scalar("SELECT expires_at FROM ephemeral_subjects WHERE user_id = $1")
                .bind(user_id.as_uuid())
                .fetch_one(&pool)
                .await
                .expect("read ledger");
        assert_eq!(
            ledger, deadline_at,
            "round {round}: rotation must not move the deadline"
        );
    }
}

#[tokio::test]
async fn refresh_is_refused_past_the_deadline_before_any_cleanup_runs() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let store = orbisync_storage_postgres::IdentityAdministrationStore::new(pool.clone());
    let issued_at = ts(NOW_MS);
    let deadline = ts(NOW_MS + HOUR_MS);
    let (user_id, _, _) = issue_guest(&pool, issued_at, deadline, vec![Uuid::now_v7()], 1).await;

    // One second past the deadline, and deliberately *before* the revocation
    // pass has run: the session row is still 'active' and the roles are still
    // in place, so only the deadline check can refuse this.
    let after = ts(NOW_MS + HOUR_MS + 1_000);
    let status: String = sqlx::query_scalar("SELECT status FROM auth_sessions WHERE user_id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("read session");
    assert_eq!(status, "active", "the cleanup pass must not have run yet");

    let replacement = NewRefreshToken {
        id: Uuid::now_v7(),
        token_digest: vec![9_u8; 32],
        issued_at: after.as_offset_date_time(),
        expires_at: OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(NOW_MS + THIRTY_DAYS_MS) * 1_000_000,
        )
        .expect("timestamp"),
    };
    let outcome = store
        .rotate_refresh_token(
            &[1_u8; 32],
            &replacement,
            after.as_offset_date_time(),
            &Uuid::now_v7().to_string(),
        )
        .await
        .expect("rotation call itself must not error");
    assert!(
        matches!(
            outcome,
            orbisync_storage_postgres::RefreshRotation::Rejected
        ),
        "an expired subject must be refused, got {outcome:?}"
    );

    let issued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM refresh_tokens WHERE token_digest = $1")
            .bind(vec![9_u8; 32])
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(issued, 0, "no replacement token may be minted");
}

// ---------------------------------------------------------------------------
// T9 (storage half) — the participation boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_boundary_allows_permitted_worlds_and_refuses_the_rest() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let now = ts(NOW_MS);
    let permitted = Uuid::now_v7();
    let other = Uuid::now_v7();
    let (user_id, _, _) = issue_guest(&pool, now, ts(NOW_MS + HOUR_MS), vec![permitted], 1).await;
    let scope = PgEphemeralSubjectScope::new(pool.clone());

    assert_eq!(
        scope.decide(user_id, permitted, now).await.expect("decide"),
        ScopeDecision::Allowed
    );
    assert_eq!(
        scope.decide(user_id, other, now).await.expect("decide"),
        ScopeDecision::Denied
    );

    // Past the deadline even a permitted world is refused, independently of
    // the revocation pass.
    assert_eq!(
        scope
            .decide(user_id, permitted, ts(NOW_MS + HOUR_MS + 1))
            .await
            .expect("decide"),
        ScopeDecision::Denied
    );

    // A subject with no ledger row is a permanent account, which this boundary
    // does not govern -- otherwise local users would be locked out.
    assert_eq!(
        scope
            .decide(UserId::generate(), permitted, now)
            .await
            .expect("decide"),
        ScopeDecision::NotEphemeral
    );
}

// ---------------------------------------------------------------------------
// T16 / T24 — revocation, the tombstone, and checkpoint restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revocation_strips_access_but_keeps_the_subject_row() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let now = ts(NOW_MS);
    let deadline = ts(NOW_MS + HOUR_MS);
    let (user_id, _, _) = issue_guest(&pool, now, deadline, vec![Uuid::now_v7()], 1).await;

    // Give the subject a role, so the test can prove the grant is removed.
    let role_id = RoleId::generate();
    sqlx::query("INSERT INTO roles (id, name, description, revision) VALUES ($1, $2, NULL, 1)")
        .bind(role_id.as_uuid())
        .bind(format!("role-{role_id}"))
        .execute(&pool)
        .await
        .expect("insert role");
    sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
        .bind(user_id.as_uuid())
        .bind(role_id.as_uuid())
        .execute(&pool)
        .await
        .expect("grant role");

    let after_grace = OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(NOW_MS + HOUR_MS + 2 * HOUR_MS) * 1_000_000,
    )
    .expect("timestamp");
    let outcome = orbisync_storage_postgres::revoke_expired_ephemeral_subjects(
        &pool,
        after_grace,
        time::Duration::seconds(0),
        64,
    )
    .await
    .expect("revocation must succeed");
    assert_eq!(outcome.revoked, 1);
    assert_eq!(outcome.roles_removed, 1);

    let status: String = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("read user");
    assert_eq!(status, "disabled");

    let session_status: String =
        sqlx::query_scalar("SELECT status FROM auth_sessions WHERE user_id = $1")
            .bind(user_id.as_uuid())
            .fetch_one(&pool)
            .await
            .expect("read session");
    assert_eq!(session_status, "revoked");

    // No role assignment is left, so the authorizer has nothing to grant: that
    // is what actually removes the permissions.
    let roles: i64 = sqlx::query_scalar("SELECT count(*) FROM user_roles WHERE user_id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("count roles");
    assert_eq!(roles, 0);

    // The users row survives: entities and checkpoints reference it.
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("count users");
    assert_eq!(remaining, 1, "the reference subject must be kept");
}

#[tokio::test]
async fn a_checkpoint_owned_by_a_revoked_guest_still_saves_and_restores() {
    // Session 25's finding: instance_checkpoints stores owner ids inside JSONB
    // with no foreign key, and PgCheckpointStore validates them on save *and*
    // on load. Deleting the guest row would therefore break restore for the
    // instance it built things in -- so this drives the real store rather than
    // asserting on the owner query alone.
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let now = ts(NOW_MS);
    let deadline = ts(NOW_MS + HOUR_MS);
    let world_uuid = Uuid::now_v7();
    let (user_id, _, _) = issue_guest(&pool, now, deadline, vec![world_uuid], 1).await;

    let instance_uuid = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO world_definitions (id, name, description, status, default_spawn, capacity, revision, created_at, updated_at) \
         VALUES ($1, 'w', NULL, 'active', '{\"position\":{\"x\":0,\"y\":0,\"z\":0},\"rotation\":{\"x\":0,\"y\":0,\"z\":0,\"w\":1}}', 8, 1, now(), now())",
    )
    .bind(world_uuid)
    .execute(&pool)
    .await
    .expect("insert world");
    sqlx::query(
        "INSERT INTO world_instances (id, world_id, lifecycle, capacity, revision, created_at) \
         VALUES ($1, $2, 'running', 8, 1, now())",
    )
    .bind(instance_uuid)
    .bind(world_uuid)
    .execute(&pool)
    .await
    .expect("insert instance");

    let instance_id = orbisync_domain::InstanceId::new(instance_uuid).expect("instance id");
    let entity_id = orbisync_domain::EntityId::generate();
    let transform = orbisync_domain::transform::Transform::new(
        orbisync_domain::transform::Vec3::new(1.5, 2.5, 3.5).expect("position"),
        orbisync_domain::transform::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("rotation"),
        orbisync_domain::transform::Vec3::new(1.0, 1.0, 1.0).expect("scale"),
    )
    .expect("transform");
    // An entity the guest owns, exactly as a note placed on the whiteboard
    // would be.
    let entity = orbisync_domain::Entity::new(
        entity_id,
        instance_id,
        orbisync_domain::EntityKind::Object,
        Some(user_id),
        Some(transform),
        orbisync_domain::VisibilityPolicy::Global,
        now,
    );
    // Real wall-clock time, not the fixed instant the auth timeline uses: the
    // store drops checkpoints older than 30 days, so a fixed past timestamp
    // would be deleted by that retention the moment it was written. The
    // payload timestamp and the row timestamp must agree, so both use it.
    let checkpoint_at = Timestamp::from_unix_millis(
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("timestamp fits"),
    )
    .expect("valid timestamp");
    let runtime_checkpoint = orbisync_world_runtime::Checkpoint {
        instance_id,
        revision: orbisync_domain::Revision::from_u64(1),
        entities: vec![entity],
        timestamp: checkpoint_at,
        dedup: Vec::new(),
    };
    // The store takes the serialized payload the runtime produces, so the test
    // goes through the same encode step production does.
    let checkpoint = orbisync_application::AppCheckpoint {
        instance_id,
        revision: orbisync_domain::Revision::from_u64(1),
        payload: runtime_checkpoint
            .to_json_bytes()
            .expect("encode checkpoint"),
        created_at: checkpoint_at,
    };

    let store = orbisync_storage_postgres::PgCheckpointStore::new(pool.clone());
    // Saving while the guest is live must work; this is the baseline the
    // post-revocation load is compared against.
    store
        .save_checkpoint(checkpoint.clone())
        .await
        .expect("saving a checkpoint owned by a live guest must succeed");

    // Now revoke the guest and confirm the row survives as a reference subject.
    let after_grace =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(NOW_MS + 3 * HOUR_MS) * 1_000_000)
            .expect("timestamp");
    let outcome = orbisync_storage_postgres::revoke_expired_ephemeral_subjects(
        &pool,
        after_grace,
        time::Duration::seconds(0),
        64,
    )
    .await
    .expect("revocation must succeed");
    assert_eq!(outcome.revoked, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(user_id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("read user");
    assert_eq!(status, "disabled");

    // The load path runs validate_owner_references, which is the call that
    // would fail if the row had been deleted.
    let stored = store
        .load_latest(instance_id)
        .await
        .expect("restoring after revocation must succeed")
        .expect("a checkpoint must be present");
    let restored = orbisync_world_runtime::Checkpoint::from_json_bytes(&stored.payload)
        .expect("the restored payload must decode");

    assert_eq!(restored.revision, orbisync_domain::Revision::from_u64(1));
    assert_eq!(restored.entities.len(), 1);
    let restored_entity = &restored.entities[0];
    assert_eq!(restored_entity.id(), entity_id);
    assert_eq!(
        restored_entity.owner(),
        Some(user_id),
        "ownership must survive revocation, or the note loses its owner"
    );
    let restored_transform = restored_entity.transform().expect("transform is restored");
    assert!(
        (restored_transform.position().x() - 1.5).abs() < f32::EPSILON
            && (restored_transform.position().z() - 3.5).abs() < f32::EPSILON,
        "entity state must round-trip, got {:?}",
        restored_transform.position()
    );

    // A further save must also still succeed: the instance keeps checkpointing
    // after its guest is gone.
    let next_revision = orbisync_domain::Revision::from_u64(2);
    let next_runtime = orbisync_world_runtime::Checkpoint {
        revision: next_revision,
        ..runtime_checkpoint
    };
    store
        .save_checkpoint(orbisync_application::AppCheckpoint {
            instance_id,
            revision: next_revision,
            payload: next_runtime.to_json_bytes().expect("encode checkpoint"),
            created_at: checkpoint_at,
        })
        .await
        .expect("saving after revocation must succeed");

    // Meanwhile the subject itself can do nothing: the participation boundary
    // refuses it in the same world its entity lives in.
    let scope = PgEphemeralSubjectScope::new(pool.clone());
    assert_eq!(
        scope
            .decide(user_id, world_uuid, ts(NOW_MS + 3 * HOUR_MS))
            .await
            .expect("decide"),
        ScopeDecision::Denied,
        "a revoked subject must not be able to act again"
    );

    // And its session is gone, so it cannot mint a ticket either.
    let active_sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auth_sessions WHERE user_id = $1 AND status = 'active'",
    )
    .bind(user_id.as_uuid())
    .fetch_one(&pool)
    .await
    .expect("count sessions");
    assert_eq!(active_sessions, 0);
}

/// Every contender deliberately holds a stale first-login decision. The DB
/// must roll back losing users, grants and token material, not just the mapping.
#[tokio::test]
async fn concurrent_external_candidates_roll_back_every_losing_transaction() {
    let Some(pool) = pool().await else { return };
    let _guard = db_guard().lock().await;
    clean(&pool).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(24));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..24_u8 {
        let pool = pool.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            let store = PgLoginStore::new(pool);
            let now = ts(NOW_MS);
            let user_id = UserId::generate();
            let user = NewSubjectUser {
                login_id: LoginId::for_generated_subject(AuthMethod::External, user_id).unwrap(),
                display_name: "concurrent".to_owned(),
                kind: UserKind::External,
                created_at: now,
            };
            let external = ExternalIdentityRecord {
                issuer: "https://concurrent.test".to_owned(),
                subject: "first-login".to_owned(),
                created_at: now,
            };
            let session = session_record(user_id, now, ts(NOW_MS + HOUR_MS));
            let refresh = refresh_record(now, ts(NOW_MS + HOUR_MS), index);
            let audit = audit(now, user_id);
            barrier.wait().await;
            (
                user_id,
                store
                    .commit_subject_success(SubjectCommit {
                        user_id,
                        new_user: Some(&user),
                        grant_roles: &[],
                        ephemeral: None,
                        external: Some(&external),
                        session: &session,
                        refresh: &refresh,
                        audit: &audit,
                    })
                    .await,
            )
        });
    }
    let mut winners = Vec::new();
    let mut conflicts = 0;
    while let Some(result) = tasks.join_next().await {
        let (user, result) = result.unwrap();
        match result {
            Ok(()) => winners.push(user),
            Err(orbisync_application::IdentityPortError::Conflict) => conflicts += 1,
            Err(error) => panic!("unexpected commit failure: {error}"),
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(conflicts, 23);
    let mapped = PgExternalIdentityStore::new(pool.clone())
        .find_user("https://concurrent.test", "first-login")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mapped, winners[0]);
    for table in [
        "users",
        "external_identities",
        "auth_sessions",
        "refresh_tokens",
        "audit_events",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "losing transaction left a row in {table}");
    }
    let session_user: Uuid = sqlx::query_scalar("SELECT user_id FROM auth_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(session_user, mapped.as_uuid());
}
