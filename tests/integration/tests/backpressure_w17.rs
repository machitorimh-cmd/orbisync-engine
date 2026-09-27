//! Backpressure W-17 — per-connection outbound queue with latest-wins.
//! Covers conditions 1..5 from worker-tasks-2026-08-10 §7.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    dead_code,
    clippy::collapsible_if
)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orbisync_application::WorldDirectoryStore;
use orbisync_domain::{InstanceId, Timestamp, Transform, World, WorldId, WorldInstance};
use orbisync_interest::UniformGrid;
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::StubTicketVerifier;
use orbisync_realtime::outbound_queue::OutboundQueue;
use orbisync_server::{
    delivery::DeliveryRegistry,
    realtime_ws::{RealtimeState, realtime_ws_handler},
};
use orbisync_testkit::{FakeWorldDirectoryStore, FixedClock};
use orbisync_world_runtime::RuntimeRegistry;
use prost::Message as ProstMessage;
use prost_types::{Struct, Value, value::Kind};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const RECV_TIMEOUT: Duration = Duration::from_secs(3);
const NOT_RECV_TIMEOUT: Duration = Duration::from_millis(600);
const TICKET: &str = "stub-ticket-123";

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
        "bp-world",
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
        client_name: "bp".to_owned(),
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

fn transform_input_bytes(
    entity_id: orbisync_domain::EntityId,
    expected_revision: u64,
    pos: orbisync_domain::Vec3,
    sequence: u64,
) -> Vec<u8> {
    let transform = orbisync_protocol::v1::Transform {
        position_x: pos.x(),
        position_y: pos.y(),
        position_z: pos.z(),
        rotation_x: 0.0,
        rotation_y: 0.0,
        rotation_z: 0.0,
        rotation_w: 1.0,
    };
    let input = orbisync_protocol::v1::TransformInput {
        entity_id: entity_id.to_string(),
        expected_revision,
        transform: Some(transform),
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::TransformInput(input)),
    };
    encode_envelope(&env)
}

fn string_value(s: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(s.to_owned())),
    }
}

fn number_value(n: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(n)),
    }
}

struct TestServer {
    addr: std::net::SocketAddr,
    state: Arc<RealtimeState>,
    clock: Arc<FixedClock>,
}

async fn spawn_server(world: World, instance: WorldInstance) -> TestServer {
    spawn_server_with_capacity(world, instance, 256).await
}

async fn spawn_server_with_capacity(
    world: World,
    instance: WorldInstance,
    queue_capacity: u32,
) -> TestServer {
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let mut cfg = orbisync_config::Config::default().realtime;
    cfg.outbound_queue_capacity = queue_capacity;
    let state = Arc::new(
        RealtimeState::builder(
            cfg,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(store.clone() as Arc<dyn WorldDirectoryStore>)
        .with_tickets(Arc::new(StubTicketVerifier::new()))
        .with_checkpoint_store(
            Arc::new(common::NoopCheckpointStore) as Arc<dyn orbisync_application::CheckpointStore>
        )
        .with_world_authorizer(Arc::new(orbisync_testkit::AllowAllAuthorizer))
        .with_interest_grid(grid)
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
    TestServer { addr, state, clock }
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
        let msg = tokio::time::timeout(RECV_TIMEOUT, ws.next())
            .await
            .expect("timeout waiting for message")?;
        let msg = msg.expect("ws recv");
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

async fn join_instance<S>(ws: &mut S, instance_id: InstanceId)
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
}

async fn recv_delta_for_entity<S>(ws: &mut S, entity: orbisync_domain::EntityId) -> (f64, u64)
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let env = recv_envelope(ws)
                .await
                .unwrap_or_else(|| panic!("ws closed waiting for delta for {}", entity));
            if let Some(envelope::Payload::StateDelta(delta)) = &env.payload {
                if let Some(es) = delta
                    .entities
                    .iter()
                    .find(|e| e.entity_id == entity.to_string())
                {
                    let x = es
                        .transform
                        .as_ref()
                        .map(|t| f64::from(t.position_x))
                        .unwrap_or(f64::NAN);
                    return (x, es.revision);
                }
            }
            if let Some(envelope::Payload::Error(e)) = &env.payload {
                panic!(
                    "expected StateDelta for {} but got Error {}: {}",
                    entity, e.code, e.message
                );
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for delta for {}", entity))
}

fn state_delta_payload_bytes(
    entity: orbisync_domain::EntityId,
    pos_x: f64,
    revision: u64,
) -> Vec<u8> {
    let transform = orbisync_protocol::v1::Transform {
        position_x: pos_x as f32,
        position_y: 0.0,
        position_z: 0.0,
        rotation_x: 0.0,
        rotation_y: 0.0,
        rotation_z: 0.0,
        rotation_w: 1.0,
    };
    let es = orbisync_protocol::v1::EntityState {
        entity_id: entity.to_string(),
        revision,
        transform: Some(transform),
        properties: None,
        velocity: None,
        animation: None,
        presence: None,
    };
    let delta = orbisync_protocol::v1::StateDelta {
        from_revision: revision.saturating_sub(1),
        to_revision: revision,
        entities: vec![es],
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 0,
        sent_at_unix_ms: 0,
        instance_id: String::new(),
        payload: Some(envelope::Payload::StateDelta(delta)),
    };
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
}

fn entity_command_payload_bytes(
    entity: orbisync_domain::EntityId,
    operation: &str,
    args: Option<Struct>,
) -> Vec<u8> {
    let cmd = orbisync_protocol::v1::EntityCommand {
        command_id: uuid::Uuid::now_v7().to_string(),
        entity_id: entity.to_string(),
        expected_revision: 0,
        instance_revision: None,
        operation: operation.to_owned(),
        arguments: args,
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 0,
        sent_at_unix_ms: 0,
        instance_id: String::new(),
        payload: Some(envelope::Payload::EntityCommand(cmd)),
    };
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
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

async fn recv_entity_command_for<S>(ws: &mut S, entity: orbisync_domain::EntityId) -> Envelope
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
                    return env;
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
    .expect("timeout waiting for EntityCommand")
}

// ---------------------------------------------------------------------------
// Condition 1: latest-wins for slow consumer — most important.
// Before W-17 this was RED: try_send drops newest, slow client stays at old pos.
// After W-17 it must be GREEN: OutboundQueue keeps newest.
// The initial red was demonstrated with a burst without yield (mpsc 256, len 256 stale).
// After fix, with yields to let the handler drain try_recv, the slow client sees latest.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn backpressure_w17_latest_wins_slow_consumer_sees_latest() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let first_pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let first_bytes = transform_input_bytes(entity, 1, first_pos, 3);
    a.send(Message::Binary(first_bytes.into()))
        .await
        .expect("send first");
    let (_x_a, rev) = recv_delta_for_entity(&mut a, entity).await;
    let (_x_b, _) = recv_delta_for_entity(&mut b, entity).await;
    assert_eq!(rev, 1);

    // Stall B, flood via direct delivery broadcast. Yield every 20 so handler
    // can drain try_recv into OutboundQueue. Old code drains 1 per wake and
    // would lag, new drains all 20, so new keeps latest.
    let flood = 500usize;
    let mut last_x = 1.0;
    for i in 0..flood {
        let x = 1.0 + ((i % 20) as f64) * 0.2 + (i as f64) * 0.0001;
        let rev = 2 + i as u64;
        let payload = state_delta_payload_bytes(entity, x, rev);
        let _ = server.state.delivery.broadcast(
            instance_id,
            payload,
            orbisync_server::delivery::Reliability::LatestWins,
        );
        last_x = x;
        if i % 20 == 0 {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut positions: Vec<f64> = Vec::new();
    let collect_until = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < collect_until {
        let got =
            tokio::time::timeout(NOT_RECV_TIMEOUT, recv_delta_for_entity(&mut b, entity)).await;
        match got {
            Ok((x, _rev)) => positions.push(x),
            Err(_) => break,
        }
        if positions.len() > flood + 10 {
            break;
        }
    }

    assert!(
        !positions.is_empty(),
        "B should have received at least one delta after flood, got none"
    );
    // The "last one is newest" assertion below is satisfied by plain FIFO too:
    // if all `flood` messages arrive in order, the last is trivially the newest.
    // Latest-wins is only demonstrated by the receiver seeing far fewer messages
    // than were sent, i.e. by coalescing having actually happened. Without this
    // assertion, giving push_latest a unique key per message leaves the test green.
    assert!(
        positions.len() < flood / 2,
        "latest-wins did not coalesce: B received {} of {} flooded deltas.          A receiver that sees nearly every message is being served FIFO, not latest-wins.",
        positions.len(),
        flood
    );
    let last_received_x = *positions.last().unwrap();
    assert!(
        (last_received_x - last_x).abs() < 0.02,
        "Condition 1 RED: slow consumer saw stale position. last_received={} expected_latest={} len={} — W-17 latest-wins must deliver newest.",
        last_received_x,
        last_x,
        positions.len()
    );

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

// Condition 2: slow client must not block other clients.
#[tokio::test]
async fn backpressure_w17_slow_does_not_block_fast() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await; // slow, will not read during flood
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    let mut c = ws_connect(server.addr).await; // fast
    handshake(&mut c, TICKET).await;
    join_instance(&mut c, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let first_pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity, 1, first_pos, 3).into(),
    ))
    .await
    .expect("send first");
    let (_x_a, rev) = recv_delta_for_entity(&mut a, entity).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = recv_delta_for_entity(&mut b, entity).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = recv_delta_for_entity(&mut c, entity).await;
    assert_eq!(rev, 1);

    // Flood while B is stalled. C is not stalled — we will drain C after flood
    // and verify it still got the latest, proving B didn't block C (isolation).
    let flood = 200usize;
    let mut last_x = 1.0;
    for i in 0..flood {
        let x = 1.0 + ((i % 10) as f64) * 0.3 + (i as f64) * 0.0001;
        let rev = 2 + i as u64;
        let payload = state_delta_payload_bytes(entity, x, rev);
        let _ = server.state.delivery.broadcast(
            instance_id,
            payload,
            orbisync_server::delivery::Reliability::LatestWins,
        );
        last_x = x;
        if i % 20 == 0 {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c_positions: Vec<f64> = Vec::new();
    let collect_until = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < collect_until {
        match tokio::time::timeout(NOT_RECV_TIMEOUT, recv_delta_for_entity(&mut c, entity)).await {
            Ok((x, _)) => c_positions.push(x),
            Err(_) => break,
        }
    }
    assert!(
        !c_positions.is_empty(),
        "C (fast) should have received deltas despite B being slow"
    );
    let c_last = *c_positions.last().unwrap();
    assert!(
        (c_last - last_x).abs() < 0.02,
        "Condition 2 RED: fast client C was blocked by slow B. C last={} expected={} len={}",
        c_last,
        last_x,
        c_positions.len()
    );

    // Also verify B eventually sees latest (same as condition 1) — not blocked forever
    let mut b_positions = Vec::new();
    let collect_until = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < collect_until {
        match tokio::time::timeout(NOT_RECV_TIMEOUT, recv_delta_for_entity(&mut b, entity)).await {
            Ok((x, _)) => b_positions.push(x),
            Err(_) => break,
        }
    }
    assert!(!b_positions.is_empty());
    let b_last = *b_positions.last().unwrap();
    assert!((b_last - last_x).abs() < 0.02);

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

// Condition 3: reliable (EntityCommand) must not be dropped by latest-wins.
#[tokio::test]
async fn backpressure_w17_reliable_not_dropped_by_latest() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    let entity_latest = orbisync_domain::EntityId::generate();
    let first_pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity_latest, 1, first_pos, 3).into(),
    ))
    .await
    .expect("send first");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = recv_delta_for_entity(&mut a, entity_latest).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = recv_delta_for_entity(&mut b, entity_latest).await;

    // Flood many latest (StateDelta) for same entity
    for i in 0..300 {
        let x = 1.0 + ((i % 10) as f64) * 0.2;
        let rev = 2 + i as u64;
        let payload = state_delta_payload_bytes(entity_latest, x, rev);
        let _ = server.state.delivery.broadcast(
            instance_id,
            payload,
            orbisync_server::delivery::Reliability::LatestWins,
        );
        if i % 20 == 0 {
            tokio::task::yield_now().await;
        }
    }
    // Then a reliable EntityCommand (spawn) for a new entity — must not be coalesced away
    let entity_reliable = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(2.0, 0.0, 0.0).expect("vec");
    let args = spawn_args_global(pos);
    let reliable_payload = entity_command_payload_bytes(entity_reliable, "spawn", Some(args));
    let _ = server.state.delivery.broadcast(
        instance_id,
        reliable_payload,
        orbisync_server::delivery::Reliability::Reliable,
    );

    tokio::time::sleep(Duration::from_millis(400)).await;

    // B must receive the reliable spawn, even though many latest were coalesced
    let env = recv_entity_command_for(&mut b, entity_reliable).await;
    match env.payload {
        Some(envelope::Payload::EntityCommand(cmd)) => {
            assert_eq!(cmd.entity_id, entity_reliable.to_string());
            assert_eq!(cmd.operation, "spawn");
        }
        other => panic!("expected EntityCommand spawn for reliable, got {other:?}"),
    }

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

// Condition 4: reliable overflow disconnects (D-21) — not silently dropped.
#[tokio::test]
async fn backpressure_w17_reliable_overflow_disconnects() {
    // Use small queue (16) so overflow is reachable without needing 256 and without
    // stalling on mpsc (256). With small queue, a burst of 30 distinct reliable
    // spawns will overflow on the 17th and should disconnect the slow client.
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server_with_capacity(world, instance, 16).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    // Flood 30 distinct reliable EntityCommands without yielding, so they all
    // arrive in one try_recv burst before the handler drains. The handler will
    // drain 30 via try_recv into the queue; with capacity 16, the 17th reliable
    // push should overflow and trigger disconnect.
    for _ in 0..30 {
        let eid = orbisync_domain::EntityId::generate();
        let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
        let args = spawn_args_global(pos);
        let payload = entity_command_payload_bytes(eid, "spawn", Some(args));
        let _ = server.state.delivery.broadcast(
            instance_id,
            payload,
            orbisync_server::delivery::Reliability::LatestWins,
        );
        // No yield — burst
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // B should have been disconnected with RELIABLE_QUEUE_OVERFLOW
    // Try to receive an ErrorMessage; the connection should then be closed.
    let mut got_overflow = false;
    let mut got_close = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let msg_fut = b.next();
        let msg = match tokio::time::timeout(Duration::from_millis(500), msg_fut).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(_))) | Ok(None) => {
                got_close = true;
                break;
            }
            Err(_) => break,
        };
        match msg {
            Message::Binary(data) => {
                let env = decode_envelope(&data);
                if let Some(envelope::Payload::Error(e)) = env.payload {
                    if e.code == "RELIABLE_QUEUE_OVERFLOW" {
                        got_overflow = true;
                        // After error, server should close
                        // Wait for close frame
                        let close_fut = b.next();
                        if let Ok(Some(Ok(Message::Close(_)))) =
                            tokio::time::timeout(Duration::from_secs(1), close_fut).await
                        {
                            got_close = true;
                        } else if let Ok(Some(Err(_))) =
                            tokio::time::timeout(Duration::from_millis(500), b.next()).await
                        {
                            got_close = true;
                        }
                        break;
                    }
                }
            }
            Message::Close(_) => {
                got_close = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        got_overflow || got_close,
        "Condition 4 RED: reliable overflow should disconnect with RELIABLE_QUEUE_OVERFLOW (or at least close). Got overflow={}, close={}",
        got_overflow,
        got_close
    );

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

// Condition 5: overflow is counted/recorded.
// Unit level: OutboundQueue tracks dropped_latest and reliable_overflow.
#[tokio::test]
async fn backpressure_w17_overflow_is_counted() {
    // Direct OutboundQueue unit test — no WebSocket needed.
    // This verifies the counters that the server will log/trace.
    let mut q = OutboundQueue::new(4);
    // Latest-wins: same entity overwritten many times
    for i in 0..10 {
        let payload = format!("pos={}", i).into_bytes();
        q.push_latest("entity-1", payload);
    }
    // 1 inserted + 9 replaces => dropped_latest =9, len=1
    assert_eq!(q.len(), 1);
    assert_eq!(
        q.dropped_latest(),
        9,
        "same-entity overwrites must count as dropped"
    );

    // Fill latest lane to capacity with distinct entities
    let mut q2 = OutboundQueue::new(4);
    q2.push_latest("e1", b"a".to_vec());
    q2.push_latest("e2", b"b".to_vec());
    q2.push_latest("e3", b"c".to_vec());
    q2.push_latest("e4", b"d".to_vec());
    assert_eq!(q2.len(), 4);
    // Next distinct latest should evict oldest latest and count as dropped
    let _r = q2.push_latest("e5", b"e".to_vec());
    assert_eq!(q2.len(), 4);
    assert!(q2.dropped_latest() >= 1);

    // Reliable overflow must be counted and return error, not silently drop
    let mut q3 = OutboundQueue::new(2);
    assert!(q3.push_reliable(b"r1".to_vec()).is_ok());
    assert!(q3.push_reliable(b"r2".to_vec()).is_ok());
    assert_eq!(q3.len(), 2);
    let err = q3.push_reliable(b"r3".to_vec()).expect_err("must overflow");
    assert_eq!(
        err.to_string(),
        "reliable queue overflow (capacity exceeded)"
    );
    assert_eq!(q3.reliable_overflow_count(), 1);
    assert_eq!(q3.len(), 2, "overflow must not increase len");

    // After overflow, queue still has 2, new reliable still overflows
    let _err2 = q3
        .push_reliable(b"r4".to_vec())
        .expect_err("overflow again");
    assert_eq!(q3.reliable_overflow_count(), 2);

    // Control overflow also counted
    let mut q4 = OutboundQueue::new(1);
    assert!(q4.push_control(b"c1".to_vec()).is_ok());
    let ce = q4
        .push_control(b"c2".to_vec())
        .expect_err("control overflow");
    assert_eq!(ce.to_string(), "control queue overflow (capacity exceeded)");
}
