//! Real PostgreSQL -> extension gateway -> application -> actor/HTTP contract.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use orbisync_application::{
    ExtensionRegistration, ExtensionRegistrationStore, ExtensionStatus,
    extension_command::{
        AUDIT_READ, ENTITY_READ, ExtensionCommand, ExtensionCommandApi, ExtensionTokenStore,
    },
};
use orbisync_config::{Config, MapEnv};
use orbisync_domain::{
    EntityId, EntityKind, InstanceId, Revision, Timestamp, UserId, VisibilityPolicy,
};
use orbisync_extensions::command::ExtensionGateway;
use orbisync_server::extension_reads::ExtensionReads;
use orbisync_storage_postgres::{
    PgExtensionRegistrationStore, PgExtensionTokenStore, PgIdentityQueryStore,
};
use orbisync_transport_http::{HttpState, router};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeRegistry, RuntimeState,
    actor::InstanceActor,
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
mod common;

struct Fixture {
    pool: sqlx::PgPool,
    gateway: Arc<ExtensionGateway>,
    app: Router,
    registration: ExtensionRegistration,
    instance: InstanceId,
    entity: EntityId,
}
async fn setup() -> Option<Fixture> {
    let pool = common::pool_or_skip().await?;
    let instance = InstanceId::generate();
    let entity = EntityId::generate();
    let registration = ExtensionRegistration {
        extension_id: Uuid::now_v7(),
        name: "command test".into(),
        description: None,
        endpoint: "https://extension.example.test/events".into(),
        subscribed_events: BTreeSet::new(),
        capabilities: [ENTITY_READ.into(), AUDIT_READ.into()].into(),
        token_scopes: [
            ENTITY_READ.into(),
            AUDIT_READ.into(),
            format!("instances:{instance}"),
        ]
        .into(),
        status: ExtensionStatus::Active,
        signing_secret_ref: "EXTENSION_TEST_KEY".into(),
    };
    PgExtensionRegistrationStore::new(pool.clone())
        .save_registration(registration.clone())
        .await
        .unwrap();
    let registry = Arc::new(RuntimeRegistry::new());
    let handle = registry.ensure_instance(InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id: instance,
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    }));
    assert!(matches!(
        handle
            .submit(InstanceCommand::SpawnEntity {
                command_id: None,
                entity_id: entity,
                kind: EntityKind::Object,
                owner: None,
                transform: Some(orbisync_domain::Transform::identity()),
                visibility: VisibilityPolicy::Global,
                requester: UserId::generate(),
                permissions: WorldPermissions::all()
            })
            .await
            .unwrap(),
        CommandOutcome::Applied { .. }
    ));
    let revision = handle
        .read_entity(entity)
        .await
        .unwrap()
        .unwrap()
        .revision();
    assert!(matches!(
        handle
            .submit(InstanceCommand::UpdateEntityComponent {
                command_id: None,
                entity_id: entity,
                component_key: "test.bytes".into(),
                payload_bytes: vec![0, 128, 255],
                expected_revision: revision,
                now: Timestamp::from_unix_millis(1).unwrap(),
                requester: UserId::generate(),
                permissions: WorldPermissions::all(),
            })
            .await
            .unwrap(),
        CommandOutcome::Applied { .. }
    ));
    let audit = Arc::new(
        PgIdentityQueryStore::new(
            pool.clone(),
            &Config::default(),
            &MapEnv::from_pairs([(
                "ORBISYNC_PAGINATION_HMAC_KEY",
                "extension-test-pagination-key-32bytes",
            )]),
        )
        .unwrap(),
    );
    let gateway = Arc::new(
        ExtensionGateway::new(
            Arc::new(PgExtensionTokenStore::new(pool.clone())),
            Arc::new(ExtensionReads { registry, audit }),
            vec![7; 32],
        )
        .unwrap(),
    );
    let state = HttpState::new(
        vec![],
        "test",
        "test",
        1,
        Arc::new(orbisync_testkit::FixedClock::new(
            Timestamp::from_unix_millis(1).unwrap(),
        )),
        900,
        2_592_000,
        vec![1; 32],
    )
    .with_extension_commands(gateway.clone());
    Some(Fixture {
        pool,
        gateway,
        app: router(state),
        registration,
        instance,
        entity,
    })
}
impl Fixture {
    fn scopes(&self) -> BTreeSet<String> {
        [ENTITY_READ.into(), format!("instances:{}", self.instance)].into()
    }
    fn command(&self) -> Value {
        json!({"command":"entity.get","instance_id":self.instance.to_string(),"entity_id":self.entity.to_string()})
    }
}
async fn post(app: &Router, token: &str, body: Value) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post("/v1/extensions/commands")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn real_http_reads_actor_and_denies_other_instances_unknown_commands_and_user_tokens() {
    let Some(f) = setup().await else { return };
    let token = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await
        .unwrap();
    let (status, body) = post(&f.app, token.expose_secret(), f.command()).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["entity_id"], f.entity.to_string());
    assert!(body["result"]["revision"].is_string());
    assert_eq!(body["result"]["transform"]["position"]["x"], 0.0);
    assert_eq!(
        body["result"]["components"]["test.bytes"],
        json!([0, 128, 255])
    );
    let mut cross = f.command();
    cross["instance_id"] = json!(InstanceId::generate().to_string());
    let (status, body) = post(&f.app, token.expose_secret(), cross).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "ACCESS_DENIED");
    assert!(
        body["error"]["request_id"]
            .as_str()
            .unwrap()
            .starts_with("req_")
    );
    for command in [
        json!({"command":"users.create"}),
        json!({"command":"entity.get","instance_id":"invalid","entity_id":"invalid"}),
    ] {
        assert_eq!(post(&f.app, token.expose_secret(), command).await.0, 400);
    }
    assert_eq!(
        post(&f.app, "ey.user-jwt.signature", f.command()).await.0,
        401
    );
    assert_eq!(
        post(
            &f.app,
            "orb_ext_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            f.command()
        )
        .await
        .0,
        401
    );
    assert_eq!(
        post(
            &f.app,
            token.expose_secret(),
            json!({"command":"audit.get","event_id":Uuid::now_v7().to_string()})
        )
        .await
        .0,
        403
    );
    assert!(!format!("{token:?}").contains(token.expose_secret()));
}

#[tokio::test]
async fn rotation_digest_only_storage_expiry_and_revocation_are_enforced() {
    let Some(f) = setup().await else { return };
    let first = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await
        .unwrap();
    assert_eq!(first.expose_secret().len(), 72);
    let row:(Vec<u8>,i64)=sqlx::query_as("SELECT token_digest,EXTRACT(EPOCH FROM (expires_at-issued_at))::bigint FROM extension_service_tokens WHERE extension_id=$1").bind(f.registration.extension_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(row.0.len(), 32);
    assert_ne!(row.0, first.expose_secret().as_bytes());
    assert_eq!(row.1, 2_592_000);
    let second = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        post(&f.app, first.expose_secret(), f.command()).await.0,
        401
    );
    assert_eq!(
        post(&f.app, second.expose_secret(), f.command()).await.0,
        200
    );
    sqlx::query("UPDATE extension_service_tokens SET issued_at=CURRENT_TIMESTAMP-INTERVAL '721 hours', expires_at=CURRENT_TIMESTAMP-INTERVAL '1 hour' WHERE extension_id=$1").bind(f.registration.extension_id).execute(&f.pool).await.unwrap();
    assert_eq!(
        post(&f.app, second.expose_secret(), f.command()).await.0,
        401
    );
    let third = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await
        .unwrap();
    f.gateway.revoke(f.registration.extension_id).await.unwrap();
    assert_eq!(
        post(&f.app, third.expose_secret(), f.command()).await.0,
        401
    );
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT metadata FROM audit_events WHERE target_type='extension' AND target_id=$1",
    )
    .bind(f.registration.extension_id.to_string())
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(audit.len(), 4);
    assert!(!json!(audit).to_string().contains(first.expose_secret()));
}

#[tokio::test]
async fn current_manifest_revokes_permissions_and_cannot_escalate_issued_scopes() {
    let Some(mut f) = setup().await else { return };
    let token = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await
        .unwrap();
    let store = PgExtensionRegistrationStore::new(f.pool.clone());
    f.registration.capabilities.remove(ENTITY_READ);
    store
        .save_registration(f.registration.clone())
        .await
        .unwrap();
    assert_eq!(
        post(&f.app, token.expose_secret(), f.command()).await.0,
        403
    );
    assert!(
        f.gateway
            .issue(f.registration.extension_id, f.scopes())
            .await
            .is_err()
    );
    f.registration.capabilities.insert(ENTITY_READ.into());
    f.registration
        .token_scopes
        .remove(&format!("instances:{}", f.instance));
    store
        .save_registration(f.registration.clone())
        .await
        .unwrap();
    assert_eq!(
        post(&f.app, token.expose_secret(), f.command()).await.0,
        403
    );
    f.registration
        .token_scopes
        .insert(format!("instances:{}", f.instance));
    f.registration.status = ExtensionStatus::Suspended;
    store
        .save_registration(f.registration.clone())
        .await
        .unwrap();
    assert_eq!(
        post(&f.app, token.expose_secret(), f.command()).await.0,
        401
    );
    assert!(
        f.gateway
            .issue(
                f.registration.extension_id,
                ["admin.users.create".into()].into()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn audit_capability_reads_public_projection_and_rotation_is_atomic_under_concurrency() {
    let Some(f) = setup().await else { return };
    let scopes = BTreeSet::from([AUDIT_READ.into()]);
    let (a, b) = tokio::join!(
        f.gateway.issue(f.registration.extension_id, scopes.clone()),
        f.gateway.issue(f.registration.extension_id, scopes)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM audit_events WHERE target_id=$1 LIMIT 1")
        .bind(f.registration.extension_id.to_string())
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let command = json!({"command":"audit.get","event_id":id.to_string()});
    let one = post(&f.app, a.expose_secret(), command.clone()).await;
    let two = post(&f.app, b.expose_secret(), command).await;
    assert!((one.0 == 200 && two.0 == 401) || (one.0 == 401 && two.0 == 200));
    let successful = if one.0 == 200 { one.1 } else { two.1 };
    assert_eq!(successful["result"]["action"], "extension.token_rotated");
    assert!(
        time::OffsetDateTime::parse(
            successful["result"]["timestamp"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339
        )
        .is_ok()
    );
    let token = if post(&f.app, a.expose_secret(), f.command()).await.0 == 403 {
        a
    } else {
        b
    };
    assert_eq!(
        post(&f.app, token.expose_secret(), f.command()).await.0,
        403
    );
    let missing = f
        .gateway
        .execute(
            token,
            ExtensionCommand::AuditGet {
                event_id: Uuid::now_v7(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        missing.kind(),
        orbisync_application::ApplicationErrorKind::NotFound
    );
    // Store checks expiry at the exact boundary, independent of wall-clock scheduling.
    let (digest, expires): (Vec<u8>, time::OffsetDateTime) = sqlx::query_as(
        "SELECT token_digest,expires_at FROM extension_service_tokens WHERE extension_id=$1",
    )
    .bind(f.registration.extension_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let store = PgExtensionTokenStore::new(f.pool.clone());
    assert!(
        store
            .resolve(&digest, expires - time::Duration::nanoseconds(1))
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.resolve(&digest, expires).await.unwrap().is_none());
}

#[tokio::test]
async fn failed_rotation_rolls_back_and_preserves_the_previous_credential() {
    let Some(f) = setup().await else { return };
    let original = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await
        .unwrap();
    // Fail the audit write after token replacement, only for this fixture.
    sqlx::query("CREATE OR REPLACE FUNCTION extension_test_reject_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected audit failure'; END $$")
        .execute(&f.pool).await.unwrap();
    let trigger = format!(
        "CREATE TRIGGER extension_test_audit_failure BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.target_id = '{}') EXECUTE FUNCTION extension_test_reject_audit()",
        f.registration.extension_id
    );
    sqlx::query(&trigger).execute(&f.pool).await.unwrap();
    let result = f
        .gateway
        .issue(f.registration.extension_id, f.scopes())
        .await;
    sqlx::query("DROP TRIGGER extension_test_audit_failure ON audit_events")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION extension_test_reject_audit()")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        post(&f.app, original.expose_secret(), f.command()).await.0,
        200
    );
}
