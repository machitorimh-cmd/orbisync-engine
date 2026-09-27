//! ADR-007 failure injection against PostgreSQL and the production worker/actor.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use async_trait::async_trait;
use orbisync_application::{
    ExtensionEvent, ExtensionOutboxStore, ExtensionRegistration, ExtensionRegistrationStore,
    ExtensionStatus, metrics::MetricsExporter,
};
use orbisync_domain::{
    EntityId, EntityKind, InstanceId, Revision, Timestamp, UserId, VisibilityPolicy,
};
use orbisync_extensions::{
    DeliveryEngine, DeliveryError, DeliveryOutcome, DeliveryPolicy, DeliveryWorker, HttpClient,
    SecretProvider, WebhookRequest, WebhookResponse,
};
use orbisync_observability::PrometheusMetrics;
use orbisync_storage_postgres::{
    AuditEvent, IdentityAdministrationStore, NewUser, PgExtensionOutboxStore,
    PgExtensionRegistrationStore,
};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeRegistry, RuntimeState,
    actor::InstanceActor,
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
};
use serde_json::json;
use sqlx::PgPool;
use std::{
    collections::HashSet,
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

mod common;

// A worker claims all due outbox rows. Give each test its own fully migrated
// database so parallel tests/other integration binaries cannot consume its rows.
// Requires CREATEDB on the integration-test user (the CI Postgres service owner).
struct TestDatabase {
    pool: PgPool,
    admin: PgPool,
    name: String,
}
static DATABASE_SETUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
impl TestDatabase {
    async fn new() -> Option<Self> {
        // Migrations also provision cluster-wide roles. Serialize that setup;
        // the actual tests and their database operations remain independent.
        let _setup = DATABASE_SETUP_LOCK.lock().await;
        let admin = common::pool_or_skip().await?;
        let name = format!("extension_acceptance_{}", Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("isolated test database requires CREATEDB");
        let options = admin.connect_options().as_ref().clone().database(&name);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options)
            .await
            .unwrap();
        orbisync_storage_postgres::run_migrations(&pool)
            .await
            .unwrap();
        Some(Self { pool, admin, name })
    }
    async fn close(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {}", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

// A marker with no production value, used only to detect accidental disclosure.
const SIGNING_MARKER: &str = "acceptance-signing-marker-7da9514e";

struct SigningKey;
#[async_trait]
impl SecretProvider for SigningKey {
    async fn resolve(&self, reference: &str) -> Result<Vec<u8>, DeliveryError> {
        if reference == "ACCEPTANCE_SIGNING_KEY" {
            Ok(SIGNING_MARKER.as_bytes().to_vec())
        } else {
            Err(DeliveryError::SecretUnavailable)
        }
    }
}

fn registration() -> ExtensionRegistration {
    ExtensionRegistration {
        extension_id: Uuid::now_v7(),
        name: "acceptance receiver".into(),
        description: None,
        endpoint: "https://extension.example.test/webhook".into(),
        subscribed_events: ["entity.updated".into()].into(),
        capabilities: Default::default(),
        token_scopes: Default::default(),
        status: ExtensionStatus::Active,
        signing_secret_ref: "ACCEPTANCE_SIGNING_KEY".into(),
    }
}

fn user_and_audit() -> (NewUser, AuditEvent) {
    let id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    (
        NewUser {
            id,
            login_id: format!("acceptance-{id}"),
            display_name: "Acceptance".into(),
            password_hash: "test-only-not-a-login-credential".into(),
            occurred_at: now,
        },
        AuditEvent {
            id: Uuid::now_v7(),
            occurred_at: now,
            actor_user_id: None,
            action: "user.created".into(),
            target_type: Some("user".into()),
            target_id: Some(id.to_string()),
            request_id: None,
            source_ip: None,
            result: "success".into(),
            metadata: json!({}),
        },
    )
}

#[tokio::test]
async fn outbox_insert_failure_rolls_back_user_credentials_and_audit() {
    let Some(database) = TestDatabase::new().await else {
        return;
    };
    let pool = database.pool.clone();
    let (user, audit) = user_and_audit();
    // Scope the fault to this generated user; never reject other test events.
    let name = format!("reject_outbox_{}", user.id.simple());
    sqlx::raw_sql(&format!(
        "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
         IF NEW.payload->>'user_id' = '{}' THEN RAISE EXCEPTION 'acceptance outbox fault'; END IF;
         RETURN NEW; END $$;
         CREATE TRIGGER {name} BEFORE INSERT ON outbox_events FOR EACH ROW EXECUTE FUNCTION {name}();", user.id))
        .execute(&pool).await.unwrap();
    let store = IdentityAdministrationStore::new(pool.clone());
    let failed = store.create_user_with_audit(&user, &audit).await;
    sqlx::raw_sql(&format!(
        "DROP TRIGGER {name} ON outbox_events; DROP FUNCTION {name}();"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert!(failed.is_err(), "outbox insertion must propagate failure");
    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users WHERE id=$1),
         (SELECT count(*) FROM user_credentials WHERE user_id=$1),
         (SELECT count(*) FROM audit_events WHERE id=$2),
         (SELECT count(*) FROM outbox_events WHERE payload->>'user_id'=$3)",
    )
    .bind(user.id)
    .bind(audit.id)
    .bind(user.id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0, 0, 0));
    // Positive control: the same operation succeeds after removing the fault.
    store.create_user_with_audit(&user, &audit).await.unwrap();
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE event_kind='user.created' AND payload->>'user_id'=$1")
        .bind(user.id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(payload, json!({"user_id": user.id.to_string()}));
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users WHERE id=$1),
         (SELECT count(*) FROM user_credentials WHERE user_id=$1),
         (SELECT count(*) FROM audit_events WHERE id=$2)",
    )
    .bind(user.id)
    .bind(audit.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 1));
    database.close().await;
}

#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<Vec<u8>>>);
impl Write for TraceCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct RetryReceiver {
    requests: Mutex<Vec<WebhookRequest>>,
}
#[async_trait]
impl HttpClient for RetryReceiver {
    async fn post(
        &self,
        request: WebhookRequest,
        _: Duration,
    ) -> Result<WebhookResponse, DeliveryError> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        match requests.len() {
            1 => Err(DeliveryError::Transport),
            2 => Ok(WebhookResponse { status: 503 }),
            _ => Ok(WebhookResponse { status: 204 }),
        }
    }
}

async fn wait_delivered(pool: &PgPool, event: Uuid, extension: Uuid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let done: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM extension_deliveries WHERE event_id=$1 AND extension_id=$2 AND delivered_at IS NOT NULL)")
                .bind(event).bind(extension).fetch_one(pool).await.unwrap();
            if done { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("durable worker completes delivery");
}

#[tokio::test]
async fn persisted_retries_preserve_event_id_and_do_not_expose_signing_key() {
    let Some(database) = TestDatabase::new().await else {
        return;
    };
    let pool = database.pool.clone();
    let capture = TraceCapture::default();
    let writer = capture.clone();
    // This test binary owns the subscriber, including events from worker tasks.
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_env_filter("orbisync_extensions=trace")
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let registration = registration();
    let registrations = PgExtensionRegistrationStore::new(pool.clone());
    registrations
        .save_registration(registration.clone())
        .await
        .unwrap();
    let outbox = Arc::new(PgExtensionOutboxStore::new(pool.clone()));
    let event = ExtensionEvent::EntityUpdated {
        instance_id: InstanceId::generate(),
        entity_id: EntityId::generate(),
    };
    let event_id = outbox.append_event(event.clone()).await.unwrap();
    let client = Arc::new(RetryReceiver::default());
    let metrics = Arc::new(PrometheusMetrics::new());
    let policy = DeliveryPolicy::new(1_000, 5).unwrap().with_backoff(0, 0);
    let engine = Arc::new(DeliveryEngine::new(
        client.clone(),
        Arc::new(SigningKey),
        metrics.clone(),
        policy,
    ));
    let worker = DeliveryWorker::new(engine.clone(), outbox, policy);
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { worker.run(rx).await });
    wait_delivered(&pool, event_id, registration.extension_id).await;
    stop.send(true).unwrap();
    task.await.unwrap();
    let requests = client.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3, "transport error then 503 then success");
    let mut received_ids = HashSet::new();
    for request in &requests {
        let header = |key: &str| {
            request
                .headers
                .iter()
                .find(|(k, _)| k == key)
                .unwrap()
                .1
                .clone()
        };
        let id = header("X-OrbiSync-Event-Id");
        assert_eq!(id, event_id.to_string());
        received_ids.insert(id);
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["event_id"], event_id.to_string());
        assert_eq!(body["payload"], event.payload());
        // Reproduce signature from the bytes actually handed to the receiver.
        use hmac::Mac;
        let mut mac =
            hmac::Hmac::<sha2::Sha256>::new_from_slice(SIGNING_MARKER.as_bytes()).unwrap();
        mac.update(format!("{}.{}.", header("X-OrbiSync-Timestamp"), event_id).as_bytes());
        mac.update(&request.body);
        let digest = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(header("X-OrbiSync-Signature"), format!("sha256={digest}"));
    }
    assert_eq!(
        received_ids.len(),
        1,
        "receiver deduplicates the three deliveries"
    );
    let attempts: i32 = sqlx::query_scalar(
        "SELECT attempt_count FROM extension_deliveries WHERE event_id=$1 AND extension_id=$2",
    )
    .bind(event_id)
    .bind(registration.extension_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        attempts, 2,
        "two failed attempts were persisted before success"
    );
    // Exercise the instrumented entrypoint and error event with span capture on.
    let mut unavailable = registration.clone();
    unavailable.signing_secret_ref = "MISSING_ACCEPTANCE_KEY".into();
    assert_eq!(
        engine
            .deliver(&unavailable, Uuid::now_v7(), event.kind(), &event.payload())
            .await,
        DeliveryOutcome::DeadLettered
    );
    let traces = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(traces.contains("extension.delivery_retry_scheduled"));
    assert!(traces.contains("extension_gateway.extension_delivery"));
    assert!(traces.contains("extension.delivery_secret_unavailable"));
    let exposition = metrics.render();
    assert!(exposition.contains("extension_delivery"));
    let manifest = format!(
        "{:?}",
        registrations
            .find_registration(registration.extension_id)
            .await
            .unwrap()
            .unwrap(),
    );
    for surface in [traces, exposition, manifest, format!("{requests:?}")] {
        assert!(
            !surface.contains(SIGNING_MARKER),
            "signing material must remain private"
        );
    }
    let mut suspended = registration;
    suspended.status = ExtensionStatus::Suspended;
    registrations.save_registration(suspended).await.unwrap();
    database.close().await;
}

struct BlockedReceiver {
    entered: Notify,
    release: Semaphore,
}
#[async_trait]
impl HttpClient for BlockedReceiver {
    async fn post(&self, _: WebhookRequest, _: Duration) -> Result<WebhookResponse, DeliveryError> {
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        Err(DeliveryError::Timeout)
    }
}

#[tokio::test]
async fn blocked_and_failed_delivery_does_not_block_actor_ticks_or_identity_commit() {
    let Some(database) = TestDatabase::new().await else {
        return;
    };
    let pool = database.pool.clone();
    let mut registration = registration();
    // Use another event kind so this fixture cannot claim the retry test's data.
    registration.subscribed_events = ["entity.deleted".into()].into();
    PgExtensionRegistrationStore::new(pool.clone())
        .save_registration(registration.clone())
        .await
        .unwrap();
    let outbox = Arc::new(PgExtensionOutboxStore::new(pool.clone()));
    let event_id = outbox
        .append_event(ExtensionEvent::EntityDeleted {
            instance_id: InstanceId::generate(),
            entity_id: EntityId::generate(),
        })
        .await
        .unwrap();
    let client = Arc::new(BlockedReceiver {
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    let policy = DeliveryPolicy::new(5_000, 1).unwrap();
    let engine = Arc::new(DeliveryEngine::new(
        client.clone(),
        Arc::new(SigningKey),
        Arc::new(PrometheusMetrics::new()),
        policy,
    ));
    let worker = DeliveryWorker::new(engine, outbox, policy);
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { worker.run(rx).await });
    tokio::time::timeout(Duration::from_secs(5), client.entered.notified())
        .await
        .unwrap();
    let registry = RuntimeRegistry::new();
    let handle = registry.ensure_instance(InstanceActor::new(InstanceRuntimeDescriptor {
        instance_id: InstanceId::generate(),
        state: RuntimeState::Running,
        revision: Revision::INITIAL,
    }));
    let entity_id = EntityId::generate();
    // The receiver cannot finish until release below. Timeout is a deadlock
    // guard, not a throughput/latency benchmark on this computer.
    tokio::time::timeout(Duration::from_secs(5), async {
        assert!(matches!(handle.submit(InstanceCommand::SpawnEntity {
            command_id: None, entity_id, kind: EntityKind::Object, owner: None,
            transform: None, visibility: VisibilityPolicy::Global, requester: UserId::generate(),
            permissions: WorldPermissions::all(),
        }).await.unwrap(), CommandOutcome::Applied { .. }));
        let initial = handle.read_entity(entity_id).await.unwrap().unwrap().revision();
        for n in 1..=3 {
            let entity = handle.read_entity(entity_id).await.unwrap().unwrap();
            assert!(matches!(handle.submit(InstanceCommand::UpdateEntityComponent {
                command_id: None, entity_id, component_key: "acceptance.value".into(),
                payload_bytes: vec![n], expected_revision: entity.revision(),
                now: Timestamp::from_unix_millis(i64::from(n)).unwrap(),
                requester: UserId::generate(), permissions: WorldPermissions::all(),
            }).await.unwrap(), CommandOutcome::Applied { .. }));
            let tick = handle.tick(Timestamp::from_unix_millis(i64::from(n)).unwrap(), true).await.unwrap();
            assert_eq!(tick.state, RuntimeState::Running);
            assert!(tick.checkpoint.is_some());
            assert!(tick.events.iter().any(|event| matches!(event, ExtensionEvent::EntityUpdated { entity_id: id, .. } if *id == entity_id)));
        }
        assert!(handle.read_entity(entity_id).await.unwrap().unwrap().revision() > initial);
        let (user, audit) = user_and_audit();
        IdentityAdministrationStore::new(pool.clone()).create_user_with_audit(&user, &audit).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id=$1").bind(user.id).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }).await.expect("tick and canonical commit progress while webhook is blocked");
    client.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM extension_dead_letters WHERE event_id=$1")
                    .bind(event_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if count == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout is durably dead-lettered");
    assert!(
        handle
            .tick(Timestamp::from_unix_millis(4).unwrap(), true)
            .await
            .is_some()
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    registration.status = ExtensionStatus::Suspended;
    PgExtensionRegistrationStore::new(pool)
        .save_registration(registration)
        .await
        .unwrap();
    database.close().await;
}
