//! EntityCommand wiring E2E (W-16) — real WebSocket, interest-filtered broadcast.
//! Validates N-4 visibility leak fix: OwnerOnly must not be broadcast to non-owners.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::collapsible_if,
    clippy::match_like_matches_macro,
    dead_code
)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orbisync_application::{AppCheckpoint, CheckpointStore, WorldDirectoryStore};
use orbisync_domain::{InstanceId, Timestamp, Transform, World, WorldId, WorldInstance};
use orbisync_interest::UniformGrid;
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::StubTicketVerifier;
use orbisync_server::{
    delivery::DeliveryRegistry,
    realtime_ws::{RealtimeState, realtime_ws_handler},
};
use orbisync_testkit::{AllowEntityOwnerAuthorizer, FakeWorldDirectoryStore, FixedClock};
use orbisync_world_runtime::RuntimeRegistry;
use prost::Message as ProstMessage;
use prost_types::{Struct, Value, value::Kind};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_secs(3);
const NOT_RECV_TIMEOUT: Duration = Duration::from_millis(600);
const TICKET: &str = "stub-ticket-123";

// ---------------------------------------------------------------------------
// helpers — similar to realtime_e2e.rs
// ---------------------------------------------------------------------------

fn fixed_clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ))
}

fn now_ts() -> Timestamp {
    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid")
}

fn make_world() -> World {
    World::new(
        WorldId::generate(),
        "e2e-world",
        None,
        Transform::identity(),
        100,
        now_ts(),
    )
    .expect("valid world")
}

fn make_instance(world_id: WorldId, capacity: u32) -> WorldInstance {
    WorldInstance::new(InstanceId::generate(), world_id, capacity, now_ts())
        .expect("valid instance")
}

fn encode_envelope(env: &Envelope) -> Vec<u8> {
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
}

fn decode_envelope(bytes: &[u8]) -> Envelope {
    Envelope::decode(bytes).expect("decode")
}

fn client_hello_bytes(ticket: &str) -> Vec<u8> {
    let hello = orbisync_protocol::v1::ClientHello {
        supported_minor_min: 0,
        supported_minor_max: 0,
        realtime_ticket: ticket.to_owned(),
        client_name: "e2e".to_owned(),
        client_version: "0.1.0".to_owned(),
        client_type: "test".to_owned(),
        supported_compressions: Vec::new(),
        supported_features: Vec::new(),
        resume_token: String::new(),
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 1,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::ClientHello(hello)),
    };
    encode_envelope(&env)
}

fn join_instance_bytes(instance_id: InstanceId) -> Vec<u8> {
    let payload = orbisync_protocol::v1::JoinInstance {
        world_instance_id: instance_id.to_string(),
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 2,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::JoinInstance(payload)),
    };
    encode_envelope(&env)
}

fn string_value(s: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(s.to_owned())),
    }
}

fn extract_string<'a>(value: &'a Struct, key: &str) -> Option<&'a str> {
    value.fields.get(key).and_then(|value| match &value.kind {
        Some(Kind::StringValue(value)) => Some(value.as_str()),
        _ => None,
    })
}

fn number_value(n: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(n)),
    }
}

fn entity_command_bytes(
    entity_id: orbisync_domain::EntityId,
    expected_revision: u64,
    operation: &str,
    arguments: Option<Struct>,
    sequence: u64,
) -> Vec<u8> {
    entity_command_bytes_with_id(
        entity_id,
        expected_revision,
        operation,
        arguments,
        uuid::Uuid::now_v7().to_string(),
        sequence,
    )
}

fn entity_command_bytes_with_id(
    entity_id: orbisync_domain::EntityId,
    expected_revision: u64,
    operation: &str,
    arguments: Option<Struct>,
    command_id: String,
    sequence: u64,
) -> Vec<u8> {
    let cmd = orbisync_protocol::v1::EntityCommand {
        command_id,
        entity_id: entity_id.to_string(),
        expected_revision,
        operation: operation.to_owned(),
        arguments,
        instance_revision: None,
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::EntityCommand(cmd)),
    };
    encode_envelope(&env)
}

fn spawn_args_global(pos: orbisync_domain::Vec3) -> Struct {
    let mut fields = BTreeMap::new();
    fields.insert("kind".to_owned(), string_value("object"));
    fields.insert("visibility".to_owned(), string_value("global"));
    fields.insert("position_x".to_owned(), number_value(f64::from(pos.x())));
    fields.insert("position_y".to_owned(), number_value(f64::from(pos.y())));
    fields.insert("position_z".to_owned(), number_value(f64::from(pos.z())));
    Struct { fields }
}

fn spawn_args_owner_only(pos: orbisync_domain::Vec3) -> Struct {
    let mut fields = BTreeMap::new();
    fields.insert("kind".to_owned(), string_value("object"));
    fields.insert("visibility".to_owned(), string_value("owner_only"));
    fields.insert("position_x".to_owned(), number_value(f64::from(pos.x())));
    fields.insert("position_y".to_owned(), number_value(f64::from(pos.y())));
    fields.insert("position_z".to_owned(), number_value(f64::from(pos.z())));
    Struct { fields }
}

fn update_args(component_key: &str, payload_value: i64) -> Struct {
    let mut fields = BTreeMap::new();
    fields.insert("component_key".to_owned(), string_value(component_key));
    fields.insert("value".to_owned(), number_value(payload_value as f64));
    // add another field to ensure sorting is exercised
    fields.insert("a_key".to_owned(), string_value("alpha"));
    fields.insert("z_key".to_owned(), string_value("zeta"));
    Struct { fields }
}

fn transfer_args(new_owner_id: &str) -> Struct {
    let mut fields = BTreeMap::new();
    fields.insert("new_owner_id".to_owned(), string_value(new_owner_id));
    Struct { fields }
}

struct TestServer {
    addr: std::net::SocketAddr,
    _store: Arc<FakeWorldDirectoryStore>,
    _registry: Arc<RuntimeRegistry>,
    _state: Arc<RealtimeState>,
    _clock: Arc<FixedClock>,
}

#[derive(Clone, Default)]
struct MemoryCheckpointStore {
    latest: Arc<std::sync::Mutex<Option<AppCheckpoint>>>,
    failures: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl CheckpointStore for MemoryCheckpointStore {
    async fn save_checkpoint(
        &self,
        checkpoint: AppCheckpoint,
    ) -> Result<
        Vec<orbisync_application::CheckpointSaveReceipt>,
        orbisync_application::ApplicationError,
    > {
        if self
            .failures
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(orbisync_application::ApplicationError::new(
                orbisync_application::ApplicationErrorKind::PortFailure,
                "injected checkpoint outage",
            ));
        }
        let receipts = orbisync_world_runtime::Checkpoint::from_json_bytes(&checkpoint.payload)
            .map_err(|error| {
                orbisync_application::ApplicationError::new(
                    orbisync_application::ApplicationErrorKind::PortFailure,
                    error.to_string(),
                )
            })?
            .dedup
            .into_iter()
            .map(|entry| orbisync_application::CheckpointSaveReceipt {
                command_id: entry.command_id,
                created_at_millis: entry.created_at_millis,
                expires_at_millis: entry.expires_at_millis,
            })
            .collect();
        let mut latest = self.latest.lock().expect("checkpoint lock");
        if latest.as_ref().is_none_or(|current| {
            checkpoint.revision >= current.revision && checkpoint.created_at >= current.created_at
        }) {
            *latest = Some(checkpoint);
        }
        Ok(receipts)
    }

    async fn load_latest(
        &self,
        instance_id: InstanceId,
    ) -> Result<Option<AppCheckpoint>, orbisync_application::ApplicationError> {
        Ok(self
            .latest
            .lock()
            .expect("checkpoint lock")
            .as_ref()
            .filter(|checkpoint| checkpoint.instance_id == instance_id)
            .cloned())
    }
}

async fn spawn_server(world: World, instance: WorldInstance) -> TestServer {
    spawn_server_with_checkpoint(world, instance, None).await
}

async fn spawn_server_with_checkpoint(
    world: World,
    instance: WorldInstance,
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
) -> TestServer {
    spawn_server_with_rules(world, instance, checkpoint_store, Default::default()).await
}

async fn spawn_server_with_rules(
    world: World,
    instance: WorldInstance,
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    rules: orbisync_server::input::InputRules,
) -> TestServer {
    let store = Arc::new(FakeWorldDirectoryStore::new());
    let input_world = world.id();
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let checkpoint_store = checkpoint_store
        .unwrap_or_else(|| Arc::new(common::NoopCheckpointStore) as Arc<dyn CheckpointStore>);
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(store.clone() as Arc<dyn WorldDirectoryStore>)
        .with_tickets(Arc::new(StubTicketVerifier::new()))
        .with_checkpoint_store(checkpoint_store)
        .with_world_authorizer(Arc::new(AllowEntityOwnerAuthorizer))
        .with_interest_grid(grid)
        .with_input_rule(input_world, "example.move", Arc::new(movement::Movement))
        .with_input_rules(rules)
        .build(),
    );
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    TestServer {
        addr,
        _store: store,
        _registry: registry,
        _state: state,
        _clock: clock,
    }
}

async fn ws_connect(
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let uri = format!("ws://{addr}/ws").try_into().expect("uri");
    let request = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    ws
}

async fn recv_envelope<S>(ws: &mut S) -> Option<Envelope>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = match tokio::time::timeout(RECV_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                panic!("ws stream error while waiting for envelope after {RECV_TIMEOUT:?}: {e}")
            }
            Ok(None) => return None,
            Err(_) => panic!(
                "timeout waiting for envelope after {RECV_TIMEOUT:?}: no message received within timeout; if this was waiting for REVISION_MISMATCH, the server may have applied the command and not sent Error (revision validation bypassed)"
            ),
        };
        match msg {
            Message::Binary(data) => {
                let env = decode_envelope(&data);
                match &env.payload {
                    Some(envelope::Payload::Heartbeat(_))
                    | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                    _ => return Some(env),
                }
            }
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

async fn handshake<S>(ws: &mut S, ticket: &str)
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let hello = client_hello_bytes(ticket);
    ws.send(Message::Binary(hello.into()))
        .await
        .expect("send hello");
    let env = recv_envelope(ws).await.expect("server hello");
    assert!(
        matches!(env.payload, Some(envelope::Payload::ServerHello(_))),
        "expected ServerHello, got {env:?}"
    );
}

async fn join_instance<S>(
    ws: &mut S,
    instance_id: InstanceId,
) -> orbisync_protocol::v1::JoinAccepted
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let join = join_instance_bytes(instance_id);
    ws.send(Message::Binary(join.into()))
        .await
        .expect("send join");
    let first = recv_envelope(ws).await.expect("join response 1");
    let second = recv_envelope(ws).await.expect("join response 2");
    let is_accepted = |e: &Envelope| matches!(e.payload, Some(envelope::Payload::JoinAccepted(_)));
    let is_snapshot = |e: &Envelope| matches!(e.payload, Some(envelope::Payload::Snapshot(_)));
    assert!(
        (is_accepted(&first) && is_snapshot(&second))
            || (is_snapshot(&first) && is_accepted(&second)),
        "expected JoinAccepted+Snapshot, got {first:?} and {second:?}"
    );
    [first, second]
        .into_iter()
        .find_map(|envelope| match envelope.payload {
            Some(envelope::Payload::JoinAccepted(accepted)) => Some(accepted),
            _ => None,
        })
        .expect("JoinAccepted response")
}

/// What a receiver observed about an applied entity mutation.
///
/// D-15 delivers applied mutations as reliable `EntityCommand`, not as
/// `StateDelta`, so the resulting revision travels in `expected_revision`
/// (the field is a precondition  client to server and the applied revision
/// server to client; the proto is a published contract and was not changed).
#[derive(Debug, Clone)]
struct AppliedEntity {
    message_id: String,
    command_id: String,
    entity_id: String,
    revision: u64,
    operation: String,
    arguments: Option<Struct>,
}

async fn expect_entity_applied<S>(ws: &mut S, entity: orbisync_domain::EntityId) -> AppliedEntity
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let env = recv_envelope(ws)
                .await
                .unwrap_or_else(|| panic!("ws closed waiting for EntityCommand for {}", entity));
            if let Some(envelope::Payload::EntityCommand(cmd)) = &env.payload {
                if cmd.entity_id == entity.to_string() {
                    return AppliedEntity {
                        message_id: env.message_id.clone(),
                        command_id: cmd.command_id.clone(),
                        entity_id: cmd.entity_id.clone(),
                        revision: cmd.expected_revision,
                        operation: cmd.operation.clone(),
                        arguments: cmd.arguments.clone(),
                    };
                }
            }
            if let Some(envelope::Payload::Error(e)) = &env.payload {
                panic!(
                    "expected EntityCommand for {} but got Error {}: {}",
                    entity, e.code, e.message
                );
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for EntityCommand for {}", entity))
}

async fn expect_error<S>(ws: &mut S) -> orbisync_protocol::v1::ErrorMessage
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let res = tokio::time::timeout(RECV_TIMEOUT, async {
        let mut last_seen: Option<String> = None;
        loop {
            let env = recv_envelope(ws).await.unwrap_or_else(|| {
                panic!(
                    "ws closed while waiting for Error after {RECV_TIMEOUT:?}; last_seen={last_seen:?} (expected e.g. REVISION_MISMATCH)"
                )
            });
            if let Some(envelope::Payload::Error(e)) = &env.payload {
                return e.clone();
            }
            if let Some(envelope::Payload::StateDelta(_)) = &env.payload {
                panic!("expected Error but got StateDelta: {env:?}");
            }
            if let Some(envelope::Payload::EntityCommand(cmd)) = &env.payload {
                panic!(
                    "expected Error (e.g. REVISION_MISMATCH) but got EntityCommand instead: entity_id={} revision={} operation={} payload={:?} — if stale revision was sent, revision mismatch detection may be disabled (server applied instead of rejecting)",
                    cmd.entity_id, cmd.expected_revision, cmd.operation, env
                );
            }
            // Track other payloads for diagnostics
            last_seen = Some(format!("{env:?}"));
        }
    })
    .await;
    match res {
        Ok(err) => err,
        Err(_) => panic!(
            "timeout waiting for Error after {RECV_TIMEOUT:?} (expected Error, e.g. REVISION_MISMATCH for stale revision); no Error received within timeout — if revision validation is disabled the server likely applied the command and broadcast EntityCommand instead of Error"
        ),
    }
}

async fn try_recv_entity_command_for<S>(ws: &mut S, entity: orbisync_domain::EntityId) -> bool
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let fut = async {
        loop {
            let env = recv_envelope(ws).await?;
            if let Some(envelope::Payload::EntityCommand(cmd)) = &env.payload {
                if cmd.entity_id == entity.to_string() {
                    return Some(());
                }
            }
            if let Some(envelope::Payload::Error(_)) = &env.payload {
                // Error counts as not receiving delta for this helper
                return None::<()>;
            }
        }
    };
    match tokio::time::timeout(NOT_RECV_TIMEOUT, fut).await {
        Ok(Some(_)) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests — conditions 1..7
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entity_w16_spawn_global_visible_within_30m() {
    // Condition 1: A spawn Global within 30m -> B receives.
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let args = spawn_args_global(pos);
    let bytes = entity_command_bytes(entity, 0, "spawn", Some(args), 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send spawn");

    // Both A (owner at origin, entity at 1m) and B at origin should receive.
    let es_a = expect_entity_applied(&mut a, entity).await;
    assert_eq!(es_a.entity_id, entity.to_string());
    assert_eq!(es_a.revision, 1, "spawned entity revision must be 1");
    let es_b = expect_entity_applied(&mut b, entity).await;
    assert_eq!(es_b.entity_id, entity.to_string());

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn entity_w16_command_id_replay_preserves_broadcast_correlation_and_conflict() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let command_id = uuid::Uuid::now_v7().to_string();
    let payload = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(
            orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
        )),
        command_id.clone(),
        3,
    );
    a.send(Message::Binary(payload.into()))
        .await
        .expect("spawn");
    let first_a = expect_entity_applied(&mut a, entity).await;
    let first_b = expect_entity_applied(&mut b, entity).await;
    assert_eq!(first_a.command_id, command_id);
    assert_eq!(first_b.command_id, command_id);
    assert_eq!(first_a.message_id, first_b.message_id);
    assert_eq!(first_a.revision, 1);

    // A new envelope sequence with the same command_id is an idempotent replay:
    // it returns the stored event to A and never broadcasts a second event.
    let replay = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(
            orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
        )),
        command_id.clone(),
        4,
    );
    a.send(Message::Binary(replay.into()))
        .await
        .expect("replay");
    let replayed = expect_entity_applied(&mut a, entity).await;
    assert_eq!(replayed.command_id, command_id);
    assert_eq!(replayed.revision, first_a.revision);
    assert_eq!(replayed.message_id, first_a.message_id);
    assert!(!try_recv_entity_command_for(&mut b, entity).await);

    // Reusing the ID for a different payload is fail-closed and has no actor
    // or broadcast side effect.
    let conflict = entity_command_bytes_with_id(entity, 1, "delete", None, command_id, 5);
    a.send(Message::Binary(conflict.into()))
        .await
        .expect("conflict");
    assert_eq!(expect_error(&mut a).await.code, "COMMAND_ID_CONFLICT");

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn entity_command_checkpoint_injection_survives_restart_and_replays() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let checkpoint_store = Arc::new(MemoryCheckpointStore::default());
    let server = spawn_server_with_checkpoint(
        world.clone(),
        instance.clone(),
        Some(Arc::clone(&checkpoint_store) as Arc<dyn CheckpointStore>),
    )
    .await;
    let mut client = ws_connect(server.addr).await;
    handshake(&mut client, TICKET).await;
    join_instance(&mut client, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let command_id = uuid::Uuid::now_v7().to_string();
    let command = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(
            orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
        )),
        command_id.clone(),
        3,
    );
    client
        .send(Message::Binary(command.into()))
        .await
        .expect("apply command");
    let applied = expect_entity_applied(&mut client, entity).await;
    assert_eq!(applied.command_id, command_id);
    assert!(
        checkpoint_store
            .latest
            .lock()
            .expect("checkpoint lock")
            .is_some()
    );
    drop(client);
    drop(server);

    // A new realtime state injects the same checkpoint store, restoring the
    // actor and its dedup records before accepting the replay.
    let restarted = spawn_server_with_checkpoint(
        world,
        instance,
        Some(Arc::clone(&checkpoint_store) as Arc<dyn CheckpointStore>),
    )
    .await;
    let mut replay_client = ws_connect(restarted.addr).await;
    handshake(&mut replay_client, TICKET).await;
    join_instance(&mut replay_client, instance_id).await;
    replay_client
        .send(Message::Binary(
            entity_command_bytes_with_id(
                entity,
                0,
                "spawn",
                Some(spawn_args_global(
                    orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
                )),
                command_id,
                3,
            )
            .into(),
        ))
        .await
        .expect("replay command");
    let replayed = expect_entity_applied(&mut replay_client, entity).await;
    assert_eq!(replayed.revision, applied.revision);
    assert_eq!(replayed.message_id, applied.message_id);
}

#[tokio::test]
async fn entity_command_save_outage_returns_bounded_error_then_recovers() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let checkpoint_store = Arc::new(MemoryCheckpointStore {
        latest: Arc::new(std::sync::Mutex::new(None)),
        failures: Arc::new(std::sync::atomic::AtomicUsize::new(100)),
    });
    let server = spawn_server_with_checkpoint(
        world,
        instance,
        Some(Arc::clone(&checkpoint_store) as Arc<dyn CheckpointStore>),
    )
    .await;
    let mut client = ws_connect(server.addr).await;
    handshake(&mut client, TICKET).await;
    join_instance(&mut client, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let command_id = uuid::Uuid::now_v7().to_string();
    client
        .send(Message::Binary(
            entity_command_bytes_with_id(
                entity,
                0,
                "spawn",
                Some(spawn_args_global(
                    orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
                )),
                command_id.clone(),
                3,
            )
            .into(),
        ))
        .await
        .expect("send command");
    assert_eq!(
        expect_error(&mut client).await.code,
        "PERSISTENCE_UNAVAILABLE"
    );

    // Clear the outage after the bounded foreground window. The detached
    // worker owns the reservation, so replay waits for recovery rather than
    // dispatching a second spawn.
    checkpoint_store
        .failures
        .store(0, std::sync::atomic::Ordering::Release);
    client
        .send(Message::Binary(
            entity_command_bytes_with_id(
                entity,
                0,
                "spawn",
                Some(spawn_args_global(
                    orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
                )),
                command_id,
                4,
            )
            .into(),
        ))
        .await
        .expect("replay after recovery");
    let replayed = expect_entity_applied(&mut client, entity).await;
    assert_eq!(replayed.revision, 1);
    assert!(
        checkpoint_store
            .latest
            .lock()
            .expect("checkpoint lock")
            .is_some()
    );
}

#[tokio::test]
async fn entity_command_socket_cancel_does_not_drop_recovery_reservation() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let checkpoint_store = Arc::new(MemoryCheckpointStore {
        latest: Arc::new(std::sync::Mutex::new(None)),
        failures: Arc::new(std::sync::atomic::AtomicUsize::new(100)),
    });
    let server = spawn_server_with_checkpoint(
        world,
        instance,
        Some(Arc::clone(&checkpoint_store) as Arc<dyn CheckpointStore>),
    )
    .await;
    let mut client = ws_connect(server.addr).await;
    handshake(&mut client, TICKET).await;
    join_instance(&mut client, instance_id).await;
    let entity = orbisync_domain::EntityId::generate();
    client
        .send(Message::Binary(
            entity_command_bytes(
                entity,
                0,
                "spawn",
                Some(spawn_args_global(
                    orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec"),
                )),
                3,
            )
            .into(),
        ))
        .await
        .expect("send command");
    // Closing while the bounded foreground save is waiting must not release
    // the reservation. Restore is allowed to complete after the outage ends.
    // Reason: the peer is deliberately being cancelled; a transport error is
    // expected when the server has already observed the close.
    #[allow(clippy::let_underscore_must_use)]
    let _ = client.close(None).await;
    checkpoint_store
        .failures
        .store(0, std::sync::atomic::Ordering::Release);
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if checkpoint_store
                .latest
                .lock()
                .expect("checkpoint lock")
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("cancelled socket recovery must eventually save");
}

#[tokio::test]
async fn entity_w16_owner_only_not_leaked() {
    // Condition 2 (most important): OwnerOnly entity must NOT be visible to non-owner.
    // Also verifies that Global still works in same test (so delivery not broken).
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    // First, Global must be received by B (positive control).
    let global = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(2.0, 0.0, 0.0).expect("vec");
    let args_global = spawn_args_global(pos);
    a.send(Message::Binary(
        entity_command_bytes(global, 0, "spawn", Some(args_global), 3).into(),
    ))
    .await
    .expect("send global");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut b, global).await;
    // Drain A's copy as well
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut a, global).await;

    // Now OwnerOnly at same distance — B must NOT receive.
    let owner_only = orbisync_domain::EntityId::generate();
    let args_owner = spawn_args_owner_only(pos);
    a.send(Message::Binary(
        entity_command_bytes(owner_only, 0, "spawn", Some(args_owner), 4).into(),
    ))
    .await
    .expect("send owner_only");

    // A (owner) should receive its own OwnerOnly entity (owner sees own).
    let es_a = expect_entity_applied(&mut a, owner_only).await;
    assert_eq!(es_a.entity_id, owner_only.to_string());

    // B must NOT receive within timeout. If it does, visibility leak.
    let leaked = try_recv_entity_command_for(&mut b, owner_only).await;
    assert!(
        !leaked,
        "OwnerOnly entity must not be delivered to non-owner B — visibility leak"
    );

    // Verify B can still receive another Global after (delivery not broken).
    let global2 = orbisync_domain::EntityId::generate();
    let args_global2 = spawn_args_global(pos);
    a.send(Message::Binary(
        entity_command_bytes(global2, 0, "spawn", Some(args_global2), 5).into(),
    ))
    .await
    .expect("send global2");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut b, global2).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut a, global2).await;

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn entity_w16_update_increments_revision() {
    // Condition 3: A update of own entity increments revision.
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("spawn");
    let es = expect_entity_applied(&mut a, entity).await;
    assert_eq!(es.revision, 1);
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut b, entity).await;

    // Update with correct expected_revision 1 -> should succeed and revision 2
    let args = update_args("test.key", 42);
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(args), 4).into(),
    ))
    .await
    .expect("update");
    let es2_a = expect_entity_applied(&mut a, entity).await;
    assert_eq!(es2_a.revision, 2, "update must bump entity revision to 2");
    let es2_b = expect_entity_applied(&mut b, entity).await;
    assert_eq!(es2_b.revision, 2);

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn entity_ownership_transfer_is_authorized_visible_and_idempotent() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    let accepted_a = join_instance(&mut a, instance_id).await;
    let accepted_b = join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let position = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_owner_only(position)), 3).into(),
    ))
    .await
    .expect("spawn owner-only entity");
    assert_eq!(expect_entity_applied(&mut a, entity).await.revision, 1);
    assert!(
        !try_recv_entity_command_for(&mut b, entity).await,
        "non-owner must not see the owner-only spawn"
    );

    let transfer_command_id = uuid::Uuid::now_v7().to_string();
    let transfer = entity_command_bytes_with_id(
        entity,
        1,
        "transfer_ownership",
        Some(transfer_args(&accepted_b.user_id)),
        transfer_command_id.clone(),
        4,
    );
    a.send(Message::Binary(transfer.clone().into()))
        .await
        .expect("transfer ownership");

    let transfer_a = expect_entity_applied(&mut a, entity).await;
    let transfer_b = expect_entity_applied(&mut b, entity).await;
    for applied in [&transfer_a, &transfer_b] {
        assert_eq!(applied.operation, "transfer_ownership");
        assert_eq!(applied.revision, 2);
        assert_eq!(applied.command_id, transfer_command_id);
        let arguments = applied.arguments.as_ref().expect("transfer arguments");
        assert_eq!(
            extract_string(arguments, "previous_owner_id"),
            Some(accepted_a.user_id.as_str())
        );
        assert_eq!(
            extract_string(arguments, "new_owner_id"),
            Some(accepted_b.user_id.as_str())
        );
    }

    let replay = entity_command_bytes_with_id(
        entity,
        1,
        "transfer_ownership",
        Some(transfer_args(&accepted_b.user_id)),
        transfer_command_id,
        5,
    );
    a.send(Message::Binary(replay.into()))
        .await
        .expect("replay transfer");
    assert_eq!(expect_entity_applied(&mut a, entity).await.revision, 2);
    assert!(
        !try_recv_entity_command_for(&mut b, entity).await,
        "idempotent replay must not rebroadcast"
    );

    b.send(Message::Binary(
        entity_command_bytes(entity, 2, "update", Some(update_args("test.key", 7)), 3).into(),
    ))
    .await
    .expect("new owner update");
    assert_eq!(expect_entity_applied(&mut b, entity).await.revision, 3);
    assert!(
        !try_recv_entity_command_for(&mut a, entity).await,
        "previous owner must lose owner-only visibility"
    );

    a.send(Message::Binary(
        entity_command_bytes(entity, 3, "update", Some(update_args("test.key", 8)), 6).into(),
    ))
    .await
    .expect("previous owner update attempt");
    assert_eq!(expect_error(&mut a).await.code, "NOT_OWNER");

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn entity_w16_update_by_non_owner_rejected_only_sender() {
    // Condition 4: B update of A's entity is rejected, only B gets Error, A gets nothing.
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("spawn");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut a, entity).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut b, entity).await;

    // B tries to update A's entity (owner is A)
    let args = update_args("test.key", 99);
    b.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(args), 3).into(),
    ))
    .await
    .expect("b update");

    let err = expect_error(&mut b).await;
    assert!(
        err.code == "NOT_OWNER" || err.code == "INVALID_ARGUMENT",
        "non-owner update must be rejected with NOT_OWNER, got {}",
        err.code
    );

    // A must NOT receive a StateDelta for B's rejected update
    let leaked = try_recv_entity_command_for(&mut a, entity).await;
    assert!(!leaked, "A must not receive delta for B's rejected update");

    // A can still update own entity
    let args2 = update_args("test.key", 100);
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(args2), 4).into(),
    ))
    .await
    .expect("a update");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut a, entity).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut b, entity).await;

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn entity_w16_stale_revision_rejected() {
    // Condition 5: stale expected_revision is rejected.
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("spawn");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut a, entity).await;

    // Update with correct rev 1 -> rev 2
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("test.key", 1)), 4).into(),
    ))
    .await
    .expect("update1");
    let es = expect_entity_applied(&mut a, entity).await;
    assert_eq!(es.revision, 2);

    // Stale rev 1 again -> must be rejected
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("test.key", 2)), 5).into(),
    ))
    .await
    .expect("stale update");
    let err = expect_error(&mut a).await;
    assert_eq!(
        err.code, "REVISION_MISMATCH",
        "stale revision must be REVISION_MISMATCH; got code={} message={} for entity {} (expected REVISION_MISMATCH for stale expected_revision=1, current=2; revision mismatch detection may be disabled)",
        err.code, err.message, entity
    );

    // A rejected component update must not terminate the connection.
    a.send(Message::Binary(
        entity_command_bytes(entity, 2, "update", Some(update_args("test.key", 3)), 6).into(),
    ))
    .await
    .expect("update after rejection");
    let after_rejection = expect_entity_applied(&mut a, entity).await;
    assert_eq!(after_rejection.revision, 3);

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

#[tokio::test]
async fn entity_w16_unknown_operation_rejected() {
    // Condition 6: unknown operation -> ErrorMessage
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let mut fields = BTreeMap::new();
    fields.insert("kind".to_owned(), string_value("object"));
    let args = Struct { fields };
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "teleport", Some(args), 3).into(),
    ))
    .await
    .expect("unknown op");
    let err = expect_error(&mut a).await;
    assert_eq!(err.code, "INVALID_ARGUMENT");

    // transfer_ownership is explicitly out of scope -> also rejected as unknown
    let entity2 = orbisync_domain::EntityId::generate();
    a.send(Message::Binary(
        entity_command_bytes(
            entity2,
            0,
            "transfer_ownership",
            Some(Struct {
                fields: BTreeMap::new(),
            }),
            4,
        )
        .into(),
    ))
    .await
    .expect("transfer");
    let err2 = expect_error(&mut a).await;
    assert_eq!(err2.code, "INVALID_ARGUMENT");

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

#[tokio::test]
async fn entity_w16_delete_removes_entity() {
    // Condition 7: delete after, B no longer sees entity.
    // Verify via re-join snapshot and via actor state: after delete, a new client's snapshot does not contain deleted id.
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("spawn");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut a, entity).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = expect_entity_applied(&mut b, entity).await;

    // Delete with correct expected_revision 1
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "delete", None, 4).into(),
    ))
    .await
    .expect("delete");
    // Delete broadcasts a reliable EntityCommand carrying the tombstone (D-15).
    // B should receive it (Global), and A as well. This is the deletion notice.
    // For delete, the tombstone is filtered as Global, so both receive.
    let del_a = expect_entity_applied(&mut a, entity).await;
    assert_eq!(del_a.entity_id, entity.to_string());
    assert_eq!(
        del_a.operation, "delete",
        "the broadcast must identify itself as a delete, not an update"
    );
    let del_b = expect_entity_applied(&mut b, entity).await;
    assert_eq!(del_b.entity_id, entity.to_string());

    // After delete, the entity should no longer be in snapshots.
    // New client C joins and its snapshot must NOT contain the deleted entity.
    let mut c = ws_connect(server.addr).await;
    handshake(&mut c, TICKET).await;
    // Capture snapshot for C
    let join = join_instance_bytes(instance_id);
    c.send(Message::Binary(join.into())).await.expect("c join");
    let first = recv_envelope(&mut c).await.expect("c first");
    let second = recv_envelope(&mut c).await.expect("c second");
    let snapshot_env = if matches!(first.payload, Some(envelope::Payload::Snapshot(_))) {
        first
    } else {
        second
    };
    if let Some(envelope::Payload::Snapshot(snap)) = snapshot_env.payload {
        let text = String::from_utf8_lossy(&snap.data);
        assert!(
            !text.contains(&entity.to_string()),
            "deleted entity must not appear in new snapshot, got {text}"
        );
    } else {
        panic!("expected Snapshot for C");
    }

    // Also, further update of deleted entity must be rejected (ENTITY_NOT_FOUND)
    a.send(Message::Binary(
        entity_command_bytes(entity, 2, "update", Some(update_args("test.key", 1)), 5).into(),
    ))
    .await
    .expect("update deleted");
    let err = expect_error(&mut a).await;
    assert_eq!(err.code, "ENTITY_NOT_FOUND");

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = c.close(None).await;
}

#[path = "../../../examples/server-input/movement.rs"]
mod movement;

#[tokio::test]
async fn server_input_computes_delivers_replays_and_rejects() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;
    exercise_input(server, instance_id, None).await;
}

async fn exercise_input(
    server: TestServer,
    instance_id: InstanceId,
    external: Option<external_rules::Service>,
) {
    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;
    let entity = orbisync_domain::EntityId::generate();
    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            0,
            "spawn",
            Some(spawn_args_global(
                orbisync_domain::Vec3::new(0.0, 0.0, 0.0).unwrap(),
            )),
            3,
        )
        .into(),
    ))
    .await
    .unwrap();
    expect_entity_applied(&mut a, entity).await;
    expect_entity_applied(&mut b, entity).await;
    let intent = |dx| Struct {
        fields: [
            ("rule".into(), string_value("example.move")),
            (
                "intent".into(),
                Value {
                    kind: Some(Kind::StructValue(Struct {
                        fields: [
                            ("dx".into(), number_value(dx)),
                            ("x".into(), number_value(999.0)), // forged outcome is ignored
                        ]
                        .into(),
                    })),
                },
            ),
        ]
        .into(),
    };
    let id = uuid::Uuid::now_v7().to_string();
    a.send(Message::Binary(
        entity_command_bytes_with_id(entity, 1, "input", Some(intent(1.0)), id.clone(), 4).into(),
    ))
    .await
    .unwrap();
    let accepted = expect_entity_applied(&mut a, entity).await;
    let peer = expect_entity_applied(&mut b, entity).await;
    assert_eq!(accepted.command_id, id);
    assert_eq!(accepted.operation, "update");
    assert_eq!(accepted.revision, 2);
    assert_eq!(accepted.arguments, peer.arguments);
    assert_eq!(
        accepted.arguments.as_ref().unwrap().fields["x"],
        number_value(1.0)
    );
    a.send(Message::Binary(
        entity_command_bytes_with_id(entity, 1, "input", Some(intent(1.0)), id.clone(), 5).into(),
    ))
    .await
    .unwrap();
    let replay = expect_entity_applied(&mut a, entity).await;
    assert_eq!(replay.revision, 2);
    assert_eq!(replay.arguments, accepted.arguments);
    assert!(!try_recv_entity_command_for(&mut b, entity).await);
    // Reusing an identity for different intent must not compute a second mutation.
    a.send(Message::Binary(
        entity_command_bytes_with_id(entity, 2, "input", Some(intent(-1.0)), id, 6).into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut a).await.code, "COMMAND_ID_CONFLICT");
    a.send(Message::Binary(
        entity_command_bytes(entity, 2, "input", Some(intent(2.0)), 7).into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut a).await.code, "INVALID_ARGUMENT");
    b.send(Message::Binary(
        entity_command_bytes(entity, 2, "input", Some(intent(1.0)), 3).into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut b).await.code, "INVALID_ARGUMENT");
    // A client cannot bypass computation by submitting the authoritative component.
    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            2,
            "update",
            Some(update_args("example.position", 999)),
            8,
        )
        .into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut a).await.code, "INVALID_ARGUMENT");
    if let Some(service) = external {
        assert_eq!(service.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        for (sequence, dx) in [(9, 3.0), (10, 4.0)] {
            a.send(Message::Binary(
                entity_command_bytes(entity, 2, "input", Some(intent(dx)), sequence).into(),
            ))
            .await
            .unwrap();
            assert_eq!(expect_error(&mut a).await.code, "INVALID_ARGUMENT");
        }
        service.task.abort();
        let _ = service.task.await;
        a.send(Message::Binary(
            entity_command_bytes(entity, 2, "input", Some(intent(1.0)), 11).into(),
        ))
        .await
        .unwrap();
        assert_eq!(expect_error(&mut a).await.code, "INVALID_ARGUMENT");
        assert!(!try_recv_entity_command_for(&mut b, entity).await);
    }
    let entity = server
        ._registry
        .read_entity(instance_id, entity)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.revision().as_u64(), 2);
    let position: serde_json::Value =
        serde_json::from_slice(&entity.components()["example.position"]).unwrap();
    assert_eq!(position["x"], 1.0);
    a.close(None).await.unwrap();
    b.close(None).await.unwrap();
}

#[path = "common/external_rules.rs"]
mod external_rules;

#[tokio::test]
async fn external_input_computes_delivers_replays_and_fails_closed() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let service = external_rules::Service::start().await;
    let rules = service.register(world.id()).await;
    let server = spawn_server_with_rules(world, instance, None, rules).await;
    exercise_input(server, instance_id, Some(service)).await;
}
